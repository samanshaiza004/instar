//! Explicit, transactional elapsed-delivery regressions for DP2 B-4a.

mod common;

use common::test_component;
use tempfile::tempdir;
use youth_runtime::{
    AppId, AppLifecycle, RuntimeErrorCategory, RuntimeLimits, StateLocation, YouthApp,
    YouthAppConfig,
};
use youth_state::{GuestCallPhase, StateLimits, StateStore, StateValue};

fn config(component: &str, database: &std::path::Path) -> YouthAppConfig {
    YouthAppConfig {
        component_path: test_component(component),
        app_id: AppId::parse("dev.youth.elapsed").unwrap(),
        state: StateLocation::File(database.to_owned()),
        limits: RuntimeLimits::default(),
    }
}

fn create_due(database: &std::path::Path, created_at: u64, duration: u64) -> u64 {
    let mut store = StateStore::open(
        StateLocation::File(database.to_owned()),
        StateLimits::default(),
    )
    .unwrap();
    store.begin(GuestCallPhase::Handle).unwrap();
    let schedule = store.schedule_create(created_at, duration, None).unwrap();
    store.commit().unwrap();
    store.reconcile_overdue(created_at + duration).unwrap();
    schedule.id
}

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
fn due_schedule_queues_once_without_a_guest_and_mounting_later_delivers_it() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_due(&database, 0, 100);
    {
        let store = StateStore::open(
            StateLocation::File(database.clone()),
            StateLimits::default(),
        )
        .unwrap();
        assert_eq!(store.pending_deliveries().unwrap().len(), 1);
    }

    let mut app = YouthApp::load_config(config("youth-sdk-elapsed", &database)).unwrap();
    app.mount().unwrap();
    let receipt = app
        .deliver_next_pending()
        .unwrap()
        .expect("one pending delivery");
    assert!(receipt.committed);
    assert!(app.pending_deliveries().unwrap().is_empty());
    assert_eq!(read_integer(&database, "elapsed-count"), Some(1));
    assert!(app.tree().unwrap().canonical().contains("Elapsed: 1"));
}

#[test]
fn elapsed_trap_retains_delivery_and_faults_without_retry() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_due(&database, 0, 100);
    let mut app = YouthApp::load_config(config("youth-elapsed-trap", &database)).unwrap();
    app.mount().unwrap();
    let before = app.tree().unwrap().canonical();

    let error = app.deliver_next_pending().unwrap_err();
    assert_eq!(error.category(), RuntimeErrorCategory::GuestTrap);
    assert_eq!(app.lifecycle(), AppLifecycle::Faulted);
    assert_eq!(app.tree().unwrap().canonical(), before);
    assert_eq!(app.pending_deliveries().unwrap().len(), 1);
}

#[test]
fn invalid_elapsed_patch_retains_delivery_and_prior_tree() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_due(&database, 0, 100);
    let mut app = YouthApp::load_config(config("youth-elapsed-invalid-patch", &database)).unwrap();
    app.mount().unwrap();
    let before = app.tree().unwrap().canonical();

    let error = app.deliver_next_pending().unwrap_err();
    assert_eq!(error.category(), RuntimeErrorCategory::InvalidPatchBatch);
    assert_eq!(app.lifecycle(), AppLifecycle::Faulted);
    assert_eq!(app.tree().unwrap().canonical(), before);
    assert_eq!(app.pending_deliveries().unwrap().len(), 1);
}

#[cfg(feature = "test-support")]
#[test]
fn commit_failure_keeps_state_tree_and_acknowledgement_together() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_due(&database, 0, 100);
    let mut app = YouthApp::load_config(config("youth-sdk-elapsed", &database)).unwrap();
    app.mount().unwrap();
    let before = app.tree().unwrap().canonical();
    app.fail_next_state_commit();

    let error = app.deliver_next_pending().unwrap_err();
    assert_eq!(error.category(), RuntimeErrorCategory::StateCommitFailed);
    assert_eq!(app.lifecycle(), AppLifecycle::Faulted);
    assert_eq!(app.tree().unwrap().canonical(), before);
    assert_eq!(read_integer(&database, "elapsed-count"), None);
    assert_eq!(app.pending_deliveries().unwrap().len(), 1);
}

#[test]
fn success_commits_state_tree_and_only_one_acknowledgement_together() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let first = create_due(&database, 0, 100);
    let second = create_due(&database, 0, 200);
    let mut app = YouthApp::load_config(config("youth-sdk-elapsed", &database)).unwrap();
    app.mount().unwrap();

    app.deliver_next_pending().unwrap().unwrap();

    assert_eq!(read_integer(&database, "elapsed-count"), Some(1));
    assert!(app.tree().unwrap().canonical().contains("Elapsed: 1"));
    let remaining = app.pending_deliveries().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].schedule_id, second);
    assert_ne!(remaining[0].schedule_id, first);
}

#[test]
fn recognized_but_ignored_elapsed_event_is_acknowledged() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_due(&database, 0, 100);
    let mut app = YouthApp::load_config(config("youth-sdk-tally", &database)).unwrap();
    app.mount().unwrap();
    let before = app.tree().unwrap().canonical();

    let receipt = app.deliver_next_pending().unwrap().unwrap();

    assert!(receipt.committed);
    assert_eq!(receipt.patch_count, 0);
    assert_eq!(app.tree().unwrap().canonical(), before);
    assert!(app.pending_deliveries().unwrap().is_empty());
}

#[test]
fn old_protocol_fails_closed_without_consuming_delivery() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    create_due(&database, 0, 100);
    let mut app = YouthApp::load_config(config("youth-legacy-v003", &database)).unwrap();
    app.mount().unwrap();
    let before = app.tree().unwrap().canonical();

    let error = app.deliver_next_pending().unwrap_err();

    assert_eq!(error.category(), RuntimeErrorCategory::UnsupportedWorld);
    assert_eq!(app.lifecycle(), AppLifecycle::Faulted);
    assert_eq!(app.tree().unwrap().canonical(), before);
    assert_eq!(app.pending_deliveries().unwrap().len(), 1);
}

#[test]
fn pending_deliveries_are_consumed_in_deterministic_order() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let later = create_due(&database, 0, 300);
    let first = create_due(&database, 0, 200);
    let second = create_due(&database, 0, 200);
    let mut app = YouthApp::load_config(config("youth-sdk-elapsed", &database)).unwrap();
    app.mount().unwrap();
    assert_eq!(
        app.pending_deliveries()
            .unwrap()
            .iter()
            .map(|delivery| delivery.schedule_id)
            .collect::<Vec<_>>(),
        vec![first, second, later]
    );

    app.deliver_next_pending().unwrap().unwrap();
    assert_eq!(
        app.pending_deliveries()
            .unwrap()
            .iter()
            .map(|delivery| delivery.schedule_id)
            .collect::<Vec<_>>(),
        vec![second, later]
    );
}

#[test]
fn failed_first_delivery_prevents_the_second_from_leapfrogging() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let first = create_due(&database, 0, 100);
    let second = create_due(&database, 0, 200);
    let mut app = YouthApp::load_config(config("youth-elapsed-trap", &database)).unwrap();
    app.mount().unwrap();

    assert!(app.deliver_next_pending().is_err());
    assert_eq!(
        app.pending_deliveries()
            .unwrap()
            .iter()
            .map(|delivery| delivery.schedule_id)
            .collect::<Vec<_>>(),
        vec![first, second]
    );
    let error = app.deliver_next_pending().unwrap_err();
    assert_eq!(error.category(), RuntimeErrorCategory::InvalidLifecycle);
    assert_eq!(app.pending_deliveries().unwrap().len(), 2);
}
