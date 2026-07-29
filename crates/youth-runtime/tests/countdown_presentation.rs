//! Host-owned countdown presentation does not require guest turns.

mod common;

use std::sync::Arc;

use common::test_component;
use tempfile::tempdir;
use youth_runtime::{
    AppId, RuntimeLimits, RuntimeTimeSeams, StateLocation, VirtualDeadlineClock,
    VirtualGuestMonotonicClock, VirtualWakeDriver, YouthApp, YouthAppConfig,
    resolve_countdown_display,
};
use youth_state::{GuestCallPhase, StateLimits, StateStore, StateValue};
use youth_tree::NodeData;

fn read_integer(database: &std::path::Path, key: &str) -> Option<i64> {
    let mut store = StateStore::open(
        StateLocation::File(database.to_owned()),
        StateLimits::default(),
    )
    .unwrap();
    store.begin(GuestCallPhase::Resync).unwrap();
    let value = store.get(key).unwrap();
    store.rollback().unwrap();
    match value {
        Some(StateValue::Integer(value)) => Some(value),
        None => None,
        other => panic!("unexpected state value: {other:?}"),
    }
}

#[test]
fn countdown_redraw_is_guestless_and_due_delivery_occurs_once() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let deadline = VirtualDeadlineClock::new(0);
    let app_id = AppId::parse("dev.youth.countdown-presentation").unwrap();
    let config = YouthAppConfig {
        component_path: test_component("youth-countdown-presentation"),
        app_id: app_id.clone(),
        state: StateLocation::File(database.clone()),
        limits: RuntimeLimits {
            time: RuntimeTimeSeams {
                deadline_clock: Arc::new(deadline.clone()),
                wake_driver: Arc::new(VirtualWakeDriver::default()),
                guest_monotonic_clock: Arc::new(VirtualGuestMonotonicClock::new(0)),
            },
            ..RuntimeLimits::default()
        },
    };
    let mut app = YouthApp::load_config(config).unwrap();
    let mounted = app.mount().unwrap();
    let start = mounted
        .nodes
        .iter()
        .find(|node| matches!(&node.data, NodeData::Button { label, .. } if label == "Start"))
        .unwrap()
        .id;
    app.activate(start).unwrap();
    app.resync().unwrap();

    let (schedule, precision, format) = app
        .tree()
        .unwrap()
        .to_snapshot()
        .nodes
        .iter()
        .find_map(|node| node.data.countdown_ref())
        .expect("the committed tree contains its countdown reference");
    let record = app.schedule(schedule.id).unwrap().unwrap();
    assert_eq!(record.deadline_millis, Some(300_000));

    // Presentation reads below are pure host work: there is deliberately no
    // app.handle(...), activation, delivery, or other guest-invoking call.
    assert_eq!(
        resolve_countdown_display(schedule, precision, format, Some(&record), 299_000),
        "00:01"
    );
    assert_eq!(
        resolve_countdown_display(schedule, precision, format, Some(&record), 300_000),
        "00:00"
    );
    assert_eq!(read_integer(&database, "elapsed-count"), None);

    deadline.set(300_000);
    let mut store = StateStore::open_for_app(
        StateLocation::File(database.clone()),
        StateLimits::default(),
        app_id,
    )
    .unwrap();
    store.reconcile_overdue(300_000).unwrap();
    assert_eq!(app.pending_deliveries().unwrap().len(), 1);

    let receipt = app.deliver_next_pending().unwrap().expect("one delivery");
    assert!(receipt.committed);
    assert!(app.pending_deliveries().unwrap().is_empty());
    assert_eq!(app.deliver_next_pending().unwrap(), None);
    assert_eq!(read_integer(&database, "elapsed-count"), Some(1));
}
