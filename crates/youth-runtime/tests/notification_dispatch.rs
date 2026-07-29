//! OS-notification dispatch regressions for DP2 C-4.

mod common;

use std::sync::Arc;

use common::test_component;
use tempfile::tempdir;
use youth_runtime::{
    AppId, AppLifecycle, RecordingNotificationDispatcher, RuntimeErrorCategory, RuntimeLimits,
    RuntimeTimeSeams, ScheduleWake, StateLocation, VirtualDeadlineClock,
    VirtualGuestMonotonicClock, VirtualWakeDriver, WakeDisposition, WakeToken, YouthApp,
    YouthAppConfig,
};
use youth_state::{GuestCallPhase, ScheduleRecord, StateLimits, StateStore};

const START: u64 = 1_000;

fn app_id() -> AppId {
    AppId::parse("dev.youth.notification-dispatch").unwrap()
}

fn config(
    component: &str,
    database: &std::path::Path,
    deadline: &VirtualDeadlineClock,
    wakes: &VirtualWakeDriver,
    notifications: &RecordingNotificationDispatcher,
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
                notification_dispatcher: Arc::new(notifications.clone()),
            },
            ..RuntimeLimits::default()
        },
    }
}

fn create_schedule(
    database: &std::path::Path,
    duration: u64,
    notification: Option<(String, String)>,
) -> ScheduleRecord {
    let mut store = StateStore::open_for_app(
        StateLocation::File(database.to_owned()),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    store.begin(GuestCallPhase::Handle).unwrap();
    let schedule = store
        .schedule_create(START, duration, notification)
        .unwrap();
    store.commit().unwrap();
    schedule
}

#[test]
fn overdue_on_open_dispatches_the_stored_notification_exactly_once() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_schedule(
        &database,
        100,
        Some(("Timer finished".into(), "Take a break".into())),
    );
    let deadline = VirtualDeadlineClock::new(START + 100);
    let wakes = VirtualWakeDriver::default();
    let notifications = RecordingNotificationDispatcher::default();

    let app = YouthApp::load_config(config(
        "youth-sdk-elapsed",
        &database,
        &deadline,
        &wakes,
        &notifications,
    ))
    .unwrap();

    assert_eq!(
        notifications.dispatched(),
        vec![("Timer finished".into(), "Take a break".into())]
    );
    assert_eq!(app.pending_deliveries().unwrap().len(), 1);
    drop(app);

    YouthApp::load_config(config(
        "youth-sdk-elapsed",
        &database,
        &deadline,
        &wakes,
        &notifications,
    ))
    .unwrap();
    assert_eq!(notifications.dispatched().len(), 1);
}

#[test]
fn live_wake_dispatches_the_stored_notification_exactly_once() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let schedule = create_schedule(
        &database,
        100,
        Some(("Focus complete".into(), "Nice work".into())),
    );
    let deadline = VirtualDeadlineClock::new(START);
    let wakes = VirtualWakeDriver::default();
    let notifications = RecordingNotificationDispatcher::default();
    let mut app = YouthApp::load_config(config(
        "youth-sdk-elapsed",
        &database,
        &deadline,
        &wakes,
        &notifications,
    ))
    .unwrap();
    deadline.set(START + 100);
    let token = WakeToken::for_record(app_id(), &schedule);
    let wake = ScheduleWake {
        application_id: app_id(),
        token,
    };

    assert_eq!(
        app.receive_schedule_wake(&wake).unwrap(),
        WakeDisposition::DeliveryQueued
    );
    assert_eq!(
        notifications.dispatched(),
        vec![("Focus complete".into(), "Nice work".into())]
    );
    assert_eq!(
        app.receive_schedule_wake(&wake).unwrap(),
        WakeDisposition::Discarded
    );
    assert_eq!(notifications.dispatched().len(), 1);
}

#[test]
fn due_schedule_without_a_descriptor_does_not_dispatch() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_schedule(&database, 100, None);
    let deadline = VirtualDeadlineClock::new(START + 100);
    let wakes = VirtualWakeDriver::default();
    let notifications = RecordingNotificationDispatcher::default();

    YouthApp::load_config(config(
        "youth-sdk-elapsed",
        &database,
        &deadline,
        &wakes,
        &notifications,
    ))
    .unwrap();

    assert!(notifications.dispatched().is_empty());
}

#[test]
fn dispatch_is_independent_of_a_subsequent_trapped_delivery() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_schedule(
        &database,
        100,
        Some(("Still dispatched".into(), "Delivery may fail".into())),
    );
    let deadline = VirtualDeadlineClock::new(START + 100);
    let wakes = VirtualWakeDriver::default();
    let notifications = RecordingNotificationDispatcher::default();
    let mut app = YouthApp::load_config(config(
        "youth-elapsed-trap",
        &database,
        &deadline,
        &wakes,
        &notifications,
    ))
    .unwrap();
    app.mount().unwrap();

    let error = app.deliver_next_pending().unwrap_err();

    assert_eq!(error.category(), RuntimeErrorCategory::GuestTrap);
    assert_eq!(app.lifecycle(), AppLifecycle::Faulted);
    assert_eq!(app.pending_deliveries().unwrap().len(), 1);
    assert_eq!(
        notifications.dispatched(),
        vec![("Still dispatched".into(), "Delivery may fail".into())]
    );
}

#[test]
fn paused_and_cancelled_schedules_never_dispatch() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let paused = create_schedule(
        &database,
        100,
        Some(("Paused".into(), "Must stay quiet".into())),
    );
    let cancelled = create_schedule(
        &database,
        100,
        Some(("Cancelled".into(), "Must stay quiet".into())),
    );
    let mut store = StateStore::open_for_app(
        StateLocation::File(database.clone()),
        StateLimits::default(),
        app_id(),
    )
    .unwrap();
    store.begin(GuestCallPhase::Handle).unwrap();
    store
        .schedule_pause(START + 50, paused.id, paused.generation)
        .unwrap();
    store
        .schedule_cancel(cancelled.id, cancelled.generation)
        .unwrap();
    store.commit().unwrap();
    drop(store);
    let deadline = VirtualDeadlineClock::new(START + 1_000);
    let wakes = VirtualWakeDriver::default();
    let notifications = RecordingNotificationDispatcher::default();

    let app = YouthApp::load_config(config(
        "youth-sdk-elapsed",
        &database,
        &deadline,
        &wakes,
        &notifications,
    ))
    .unwrap();

    assert!(notifications.dispatched().is_empty());
    assert!(app.pending_deliveries().unwrap().is_empty());
}
