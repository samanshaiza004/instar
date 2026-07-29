//! End-to-end host-autonomy regressions for DP2 B-4b.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::test_component;
use tempfile::tempdir;
use tokio::sync::broadcast::error::TryRecvError;
use youth_runtime::{
    AppId, AppLifecycle, RecordingNotificationDispatcher, RuntimeEvent, RuntimeLimits,
    RuntimeTimeSeams, StateLocation, TurnOrigin, VirtualDeadlineClock, VirtualGuestMonotonicClock,
    VirtualWakeDriver, WakeToken, YouthAppConfig, YouthAppHandle,
};
use youth_state::{GuestCallPhase, ScheduleStatus, StateLimits, StateStore, StateValue};
use youth_tree::{NodeData, NodeId};

const START: u64 = 1_000;

fn app_id() -> AppId {
    AppId::parse("dev.youth.host-autonomy").unwrap()
}

fn config(
    component: &str,
    database: &std::path::Path,
    deadline: &VirtualDeadlineClock,
    wakes: &VirtualWakeDriver,
) -> YouthAppConfig {
    YouthAppConfig {
        component_path: test_component(component),
        app_id: app_id(),
        state: StateLocation::File(database.to_owned()),
        limits: RuntimeLimits {
            time: RuntimeTimeSeams {
                deadline_clock: Arc::new(deadline.clone()),
                wake_driver: Arc::new(wakes.clone()),
                guest_monotonic_clock: Arc::new(VirtualGuestMonotonicClock::new(0)),
                ..RuntimeTimeSeams::default()
            },
            ..RuntimeLimits::default()
        },
    }
}

fn create_schedule(database: &std::path::Path, duration: u64) -> WakeToken {
    let mut store = StateStore::open_for_app(
        StateLocation::File(database.to_owned()),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    store.begin(GuestCallPhase::Handle).unwrap();
    let schedule = store.schedule_create(START, duration, None).unwrap();
    store.commit().unwrap();
    WakeToken::for_record(app_id(), &schedule)
}

fn assert_no_event(receiver: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>) {
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
}

async fn mount_and_drain(
    app: &YouthAppHandle,
    receiver: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> youth_tree::TreeSnapshot {
    let snapshot = app.mount().await.unwrap();
    assert!(matches!(
        receiver.try_recv(),
        Ok(RuntimeEvent::SnapshotReplaced(_))
    ));
    snapshot
}

#[tokio::test]
async fn due_wake_without_requester_commits_exactly_one_observed_turn() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let token = create_schedule(&database, 100);
    let deadline = VirtualDeadlineClock::new(START);
    let wakes = VirtualWakeDriver::default();
    let app =
        YouthAppHandle::spawn(config("youth-sdk-elapsed", &database, &deadline, &wakes)).unwrap();
    let mut events = app.subscribe();
    mount_and_drain(&app, &mut events).await;

    deadline.advance(Duration::from_millis(100));
    wakes.advance(Duration::from_millis(100));
    assert!(wakes.fire(&token));
    let inspection = app.inspect().await.unwrap();

    let RuntimeEvent::TurnCommitted(outcome) = events.try_recv().unwrap() else {
        panic!("wake must publish one committed turn");
    };
    assert_eq!(
        outcome.origin,
        TurnOrigin::ScheduleElapsed {
            schedule_id: token.schedule_id,
            generation: token.generation,
        }
    );
    assert!(outcome.receipt.committed);
    assert!(inspection.canonical_tree.contains("Elapsed: 1"));
    assert_no_event(&mut events);
    let mut store = StateStore::open_for_app(
        StateLocation::File(database),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    assert!(store.pending_deliveries().unwrap().is_empty());
    store.begin(GuestCallPhase::Resync).unwrap();
    assert_eq!(
        store.get("elapsed-count").unwrap(),
        Some(StateValue::Integer(1))
    );
    store.rollback().unwrap();
    app.stop().await.unwrap();
}

#[tokio::test]
async fn stale_missing_and_repeated_wakes_do_not_create_turns() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let token = create_schedule(&database, 100);
    let deadline = VirtualDeadlineClock::new(START);
    let wakes = VirtualWakeDriver::default();
    let app =
        YouthAppHandle::spawn(config("youth-sdk-elapsed", &database, &deadline, &wakes)).unwrap();
    let mut events = app.subscribe();
    mount_and_drain(&app, &mut events).await;

    wakes.fire_stale(WakeToken::new(
        app_id(),
        token.schedule_id,
        token.generation + 1,
    ));
    wakes.fire_stale(WakeToken::new(app_id(), 999, 1));
    app.inspect().await.unwrap();
    assert_no_event(&mut events);

    deadline.advance(Duration::from_millis(100));
    wakes.advance(Duration::from_millis(100));
    assert!(wakes.fire(&token));
    app.inspect().await.unwrap();
    assert!(matches!(
        events.try_recv(),
        Ok(RuntimeEvent::TurnCommitted(_))
    ));

    wakes.fire_stale(token);
    app.inspect().await.unwrap();
    assert_no_event(&mut events);
    app.stop().await.unwrap();
}

#[tokio::test]
async fn cancel_before_wake_discards_the_stale_hint() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let deadline = VirtualDeadlineClock::new(START);
    let wakes = VirtualWakeDriver::default();
    let app =
        YouthAppHandle::spawn(config("youth-sdk-time", &database, &deadline, &wakes)).unwrap();
    let mut events = app.subscribe();
    let snapshot = mount_and_drain(&app, &mut events).await;
    let schedule = button(&snapshot, "Schedule");
    let cancel = button(&snapshot, "Cancel");

    app.activate(schedule).await.unwrap();
    let token = wakes.armed()[0].0.clone();
    assert!(matches!(
        events.try_recv(),
        Ok(RuntimeEvent::TurnCommitted(_))
    ));
    app.activate(cancel).await.unwrap();
    assert!(matches!(
        events.try_recv(),
        Ok(RuntimeEvent::TurnCommitted(_))
    ));

    deadline.advance(Duration::from_secs(1));
    wakes.advance(Duration::from_secs(1));
    wakes.fire_stale(token);
    app.inspect().await.unwrap();
    assert_no_event(&mut events);
    app.stop().await.unwrap();
}

#[tokio::test]
async fn wake_before_cancel_attempts_delivery_first() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let deadline = VirtualDeadlineClock::new(START);
    let wakes = VirtualWakeDriver::default();
    let app =
        YouthAppHandle::spawn(config("youth-sdk-time", &database, &deadline, &wakes)).unwrap();
    let mut events = app.subscribe();
    let snapshot = mount_and_drain(&app, &mut events).await;
    let schedule = button(&snapshot, "Schedule");
    let cancel = button(&snapshot, "Cancel");
    app.activate(schedule).await.unwrap();
    let token = wakes.armed()[0].0.clone();
    assert!(matches!(
        events.try_recv(),
        Ok(RuntimeEvent::TurnCommitted(_))
    ));

    deadline.advance(Duration::from_secs(1));
    wakes.advance(Duration::from_secs(1));
    assert!(wakes.fire(&token));
    assert!(app.activate(cancel).await.is_err());

    let RuntimeEvent::TurnCommitted(outcome) = events.try_recv().unwrap() else {
        panic!("wake must precede the cancellation request");
    };
    assert!(matches!(outcome.origin, TurnOrigin::ScheduleElapsed { .. }));
    let store = StateStore::open_for_app(
        StateLocation::File(database),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    assert_eq!(
        store.schedule(token.schedule_id).unwrap().unwrap().status,
        ScheduleStatus::Cancelled
    );
}

#[tokio::test]
async fn faulted_delivery_publishes_once_without_retry_and_retains_pending() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let token = create_schedule(&database, 100);
    let deadline = VirtualDeadlineClock::new(START);
    let wakes = VirtualWakeDriver::default();
    let app =
        YouthAppHandle::spawn(config("youth-elapsed-trap", &database, &deadline, &wakes)).unwrap();
    let mut events = app.subscribe();
    mount_and_drain(&app, &mut events).await;
    deadline.advance(Duration::from_millis(100));
    wakes.advance(Duration::from_millis(100));
    assert!(wakes.fire(&token));
    let inspection = app.inspect().await.unwrap();

    assert_eq!(inspection.lifecycle, AppLifecycle::Faulted);
    assert!(matches!(events.try_recv(), Ok(RuntimeEvent::Faulted(_))));
    assert_no_event(&mut events);
    let store = StateStore::open_for_app(
        StateLocation::File(database),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    assert_eq!(store.pending_deliveries().unwrap().len(), 1);
}

#[tokio::test]
async fn shutdown_preserves_an_overdue_pending_delivery() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_schedule(&database, 100);
    let deadline = VirtualDeadlineClock::new(START + 100);
    let wakes = VirtualWakeDriver::default();
    let app =
        YouthAppHandle::spawn(config("youth-sdk-elapsed", &database, &deadline, &wakes)).unwrap();
    app.mount().await.unwrap();
    app.stop().await.unwrap();

    let store = StateStore::open_for_app(
        StateLocation::File(database.clone()),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    assert_eq!(store.pending_deliveries().unwrap().len(), 1);
    drop(store);

    let reopened =
        YouthAppHandle::spawn(config("youth-sdk-elapsed", &database, &deadline, &wakes)).unwrap();
    assert_eq!(
        reopened.inspect().await.unwrap().lifecycle,
        AppLifecycle::Loaded
    );
    let store = StateStore::open_for_app(
        StateLocation::File(database),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    assert_eq!(store.pending_deliveries().unwrap().len(), 1);
    drop(store);
    reopened.mount().await.unwrap();
    reopened.stop().await.unwrap();
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn rolled_back_delivery_publishes_nothing() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let token = create_schedule(&database, 100);
    let deadline = VirtualDeadlineClock::new(START);
    let wakes = VirtualWakeDriver::default();
    let app =
        YouthAppHandle::spawn(config("youth-sdk-elapsed", &database, &deadline, &wakes)).unwrap();
    let mut events = app.subscribe();
    mount_and_drain(&app, &mut events).await;
    app.fail_next_state_commit().await.unwrap();

    deadline.advance(Duration::from_millis(100));
    wakes.advance(Duration::from_millis(100));
    assert!(wakes.fire(&token));
    app.inspect().await.unwrap();
    assert_no_event(&mut events);
    let store = StateStore::open_for_app(
        StateLocation::File(database),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    assert_eq!(store.pending_deliveries().unwrap().len(), 1);
}

#[tokio::test]
async fn reconcile_on_open_arms_future_and_queues_overdue_without_a_guest_turn() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_schedule(&database, 100);
    create_schedule(&database, 300);
    let deadline = VirtualDeadlineClock::new(START + 150);
    let wakes = VirtualWakeDriver::default();
    let app =
        YouthAppHandle::spawn(config("youth-sdk-elapsed", &database, &deadline, &wakes)).unwrap();

    assert_eq!(app.inspect().await.unwrap().lifecycle, AppLifecycle::Loaded);
    let store = StateStore::open_for_app(
        StateLocation::File(database),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    assert_eq!(store.pending_deliveries().unwrap().len(), 1);
    assert_eq!(wakes.armed().len(), 1);
    assert_eq!(wakes.armed()[0].1, Duration::from_millis(150));
    drop(store);
    app.mount().await.unwrap();
    app.stop().await.unwrap();
}

/// `YouthAppHandle::spawn` is the path `.youth-test`'s `restart` and the
/// real desktop app both use to reopen a closed process. Its overdue
/// reconciliation runs through `reconcile_without_guest` in `worker.rs`,
/// a separate code path from `instantiate`'s own `reconcile_on_open`
/// branch (which `YouthApp::load_config` alone would use) -- both must
/// dispatch a schedule's notification independently, since this is the
/// concrete "process was closed, deadline passed, reopening" case Gate
/// C-4 exists to restore evidence for.
#[tokio::test]
async fn worker_spawn_dispatches_notification_for_an_overdue_schedule_on_open() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let deadline = VirtualDeadlineClock::new(START + 150);
    let wakes = VirtualWakeDriver::default();
    let notifications = RecordingNotificationDispatcher::default();
    {
        let mut store = StateStore::open_for_app(
            StateLocation::File(database.clone()),
            StateLimits::default(),
            app_id(),
        )
        .unwrap();
        store.begin(GuestCallPhase::Handle).unwrap();
        store
            .schedule_create(
                START,
                100,
                Some((
                    "Reopened overdue".into(),
                    "Dispatched on worker spawn".into(),
                )),
            )
            .unwrap();
        store.commit().unwrap();
    }
    let config = YouthAppConfig {
        component_path: test_component("youth-sdk-elapsed"),
        app_id: app_id(),
        state: StateLocation::File(database.clone()),
        limits: RuntimeLimits {
            time: RuntimeTimeSeams {
                deadline_clock: Arc::new(deadline.clone()),
                wake_driver: Arc::new(wakes.clone()),
                guest_monotonic_clock: Arc::new(VirtualGuestMonotonicClock::new(0)),
                notification_dispatcher: Arc::new(notifications.clone()),
            },
            ..RuntimeLimits::default()
        },
    };
    let app = YouthAppHandle::spawn(config).unwrap();

    assert_eq!(
        notifications.dispatched(),
        vec![(
            "Reopened overdue".to_owned(),
            "Dispatched on worker spawn".to_owned()
        )]
    );
    app.mount().await.unwrap();
    app.stop().await.unwrap();
}

#[tokio::test]
async fn observer_overflow_signals_resync_without_blocking_worker() {
    let app = YouthAppHandle::spawn_ephemeral(common::counter_component()).unwrap();
    let mut events = app.subscribe();
    let snapshot = app.mount().await.unwrap();
    let increment = button(&snapshot, "Increment");
    for _ in 0..80 {
        app.activate(increment).await.unwrap();
    }
    assert_eq!(app.inspect().await.unwrap().current_revision, Some(80));
    assert!(matches!(
        events.try_recv(),
        Err(TryRecvError::Lagged(skipped)) if skipped > 0
    ));
    app.stop().await.unwrap();
}

fn button(snapshot: &youth_tree::TreeSnapshot, label: &str) -> NodeId {
    snapshot
        .nodes
        .iter()
        .find_map(|node| match &node.data {
            NodeData::Button {
                label: candidate, ..
            } if candidate == label => Some(node.id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("fixture has a {label:?} button"))
}
