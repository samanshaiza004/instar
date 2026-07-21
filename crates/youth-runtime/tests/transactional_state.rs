mod common;

use common::{counter_component, test_component};
use tempfile::tempdir;
use youth_runtime::{AppId, RuntimeLimits, StateLocation, YouthApp, YouthAppConfig};
use youth_state::{GuestCallPhase, StateLimits, StateStore, StateValue};
use youth_tree::NodeId;

fn config(path: &std::path::Path) -> YouthAppConfig {
    YouthAppConfig {
        component_path: counter_component(),
        app_id: AppId::parse("dev.youth.counter").unwrap(),
        state: StateLocation::File(path.to_owned()),
        limits: RuntimeLimits::default(),
    }
}

#[test]
fn count_and_visible_tree_survive_runtime_restart() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");

    {
        let mut app = YouthApp::load_config(config(&database)).expect("first runtime loads");
        let mounted = app.mount().expect("first mount commits");
        assert_eq!(mounted.revision, 0);
        assert!(app.tree().unwrap().canonical().contains("Count: 0"));
        let receipt = app
            .activate(NodeId::new(4).unwrap())
            .expect("activation commits");
        assert_eq!(receipt.state_writes, 1);
        assert!(app.tree().unwrap().canonical().contains("Count: 1"));
    }

    let mut reopened = YouthApp::load_config(config(&database)).expect("second runtime loads");
    reopened.mount().expect("second mount reconstructs");
    assert!(reopened.tree().unwrap().canonical().contains("Count: 1"));
    let inspection = reopened.inspect();
    let summary = inspection
        .state_summary
        .expect("state summary is available");
    assert_eq!(summary.key_count, 1);
    assert_eq!(summary.logical_bytes, 45);
}

fn initialize_count(path: &std::path::Path, count: i64) {
    let mut store =
        StateStore::open(StateLocation::File(path.to_owned()), StateLimits::default()).unwrap();
    store.begin(GuestCallPhase::Mount).unwrap();
    store.set("count", StateValue::Integer(count)).unwrap();
    store.commit().unwrap();
}

fn read_count(path: &std::path::Path) -> i64 {
    let mut store =
        StateStore::open(StateLocation::File(path.to_owned()), StateLimits::default()).unwrap();
    store.begin(GuestCallPhase::Resync).unwrap();
    let value = store.get("count").unwrap();
    store.rollback().unwrap();
    match value {
        Some(StateValue::Integer(value)) => value,
        other => panic!("unexpected count value: {other:?}"),
    }
}

fn fixture_config(component: &str, database: &std::path::Path) -> YouthAppConfig {
    YouthAppConfig {
        component_path: test_component(component),
        app_id: AppId::parse("dev.youth.counter").unwrap(),
        state: StateLocation::File(database.to_owned()),
        limits: RuntimeLimits::default(),
    }
}

#[test]
fn every_failure_after_state_write_rolls_back_and_faults() {
    for component in [
        "youth-trap-after-state-write",
        "youth-invalid-patch-after-state-write",
        "youth-bad-revision-after-state-write",
        "youth-app-error-after-state-write",
    ] {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state.sqlite3");
        initialize_count(&database, 1);
        let mut app = YouthApp::load_config(fixture_config(component, &database)).unwrap();
        app.mount().unwrap();
        let before = app.tree().unwrap().canonical();
        assert!(app.activate(NodeId::new(4).unwrap()).is_err());
        assert_eq!(app.lifecycle(), youth_runtime::AppLifecycle::Faulted);
        assert_eq!(
            app.tree().unwrap().canonical(),
            before,
            "fixture {component}"
        );
        assert_eq!(read_count(&database), 1, "fixture {component}");
    }
}

#[cfg(feature = "test-support")]
#[test]
fn injected_commit_failure_retains_old_state_and_tree() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let mut app = YouthApp::load_config(config(&database)).unwrap();
    app.mount().unwrap();
    let before = app.tree().unwrap().canonical();
    app.fail_next_state_commit();
    let error = app.activate(NodeId::new(4).unwrap()).unwrap_err();
    assert_eq!(
        error.category(),
        youth_runtime::RuntimeErrorCategory::StateCommitFailed
    );
    assert_eq!(app.lifecycle(), youth_runtime::AppLifecycle::Faulted);
    assert_eq!(app.tree().unwrap().canonical(), before);
    assert_eq!(read_count(&database), 0);
}
