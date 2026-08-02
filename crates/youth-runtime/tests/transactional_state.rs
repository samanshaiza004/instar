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
        workspace: None,
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
        workspace: None,
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

#[test]
fn trap_and_invalid_patch_after_schedule_creation_roll_back_the_schedule() {
    for node in [5, 6] {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state.sqlite3");
        let mut app = YouthApp::load_config(fixture_config("youth-time-stub", &database)).unwrap();
        app.mount().unwrap();
        assert!(
            app.activate(NodeId::new(node).unwrap()).is_err(),
            "fixture node {node}"
        );
        assert_eq!(app.lifecycle(), youth_runtime::AppLifecycle::Faulted);
        drop(app);

        let store =
            StateStore::open(StateLocation::File(database), StateLimits::default()).unwrap();
        assert!(
            store.schedules().unwrap().is_empty(),
            "fixture node {node} left a schedule after rollback"
        );
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

#[cfg(feature = "test-support")]
#[tokio::test]
async fn todo_structural_commit_failure_retains_legacy_or_current_state_tree_focus_and_observers() {
    use youth_interaction::{InteractionState, LogicalKey, Modifiers};
    use youth_runtime::{RuntimeErrorCategory, RuntimeEvent, YouthAppHandle};
    use youth_tree::Tree;

    for schema in [1_i64, 2] {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state.sqlite3");
        let app_id = AppId::parse(format!("dev.youth.todo-rollback-v{schema}")).unwrap();
        seed_todo(&database, &app_id, schema);
        let app = YouthAppHandle::spawn(YouthAppConfig {
            component_path: test_component("youth-sdk-todo"),
            app_id: app_id.clone(),
            state: StateLocation::File(database.clone()),
            limits: RuntimeLimits::default(),
            workspace: None,
        })
        .unwrap();
        let mut events = app.subscribe();
        let mounted = app.mount().await.unwrap();
        assert!(matches!(
            events.recv().await,
            Ok(RuntimeEvent::SnapshotReplaced(_))
        ));
        let before = app.inspect().await.unwrap();

        let tree = Tree::from_snapshot(mounted, &youth_tree::Limits::default()).unwrap();
        let mut interaction = InteractionState::default();
        interaction.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        interaction.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        let focused =
            NodeId::new(youth_sdk::derived_node_id("todo", 1, "toggle").unwrap()).unwrap();
        assert_eq!(interaction.focused(), Some(focused));

        app.fail_next_state_commit().await.unwrap();
        let add = NodeId::new(youth_sdk::named_node_id("add")).unwrap();
        let error = app.activate(add).await.unwrap_err();
        assert_eq!(error.category(), RuntimeErrorCategory::StateCommitFailed);
        let after = app.inspect().await.unwrap();
        assert_eq!(after.canonical_tree, before.canonical_tree);
        assert_eq!(interaction.focused(), Some(focused));
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert_todo_seed_unchanged(&database, &app_id, schema);
    }
}

#[cfg(feature = "test-support")]
fn seed_todo(path: &std::path::Path, app_id: &AppId, schema: i64) {
    let mut store = StateStore::open_for_app(
        StateLocation::File(path.to_owned()),
        StateLimits::default(),
        app_id.clone(),
    )
    .unwrap();
    store.begin(GuestCallPhase::Mount).unwrap();
    store
        .set("model-schema-version", StateValue::Integer(schema))
        .unwrap();
    store
        .set("todos-next-id", StateValue::Text("2".into()))
        .unwrap();
    store
        .set("todos-order", StateValue::Text("1".into()))
        .unwrap();
    store
        .set("todo/1/title", StateValue::Text("Task 1".into()))
        .unwrap();
    if schema == 1 {
        store
            .set("todo/1/done", StateValue::Boolean(false))
            .unwrap();
    } else {
        store
            .set("todo/1/status", StateValue::Text("active".into()))
            .unwrap();
    }
    store.commit().unwrap();
}

#[cfg(feature = "test-support")]
fn assert_todo_seed_unchanged(path: &std::path::Path, app_id: &AppId, schema: i64) {
    let mut store = StateStore::open_for_app(
        StateLocation::File(path.to_owned()),
        StateLimits::default(),
        app_id.clone(),
    )
    .unwrap();
    store.begin(GuestCallPhase::Resync).unwrap();
    assert_eq!(
        store.get("model-schema-version").unwrap(),
        Some(StateValue::Integer(schema))
    );
    assert_eq!(
        store.get("todos-order").unwrap(),
        Some(StateValue::Text("1".into()))
    );
    assert_eq!(store.get("todo/2/title").unwrap(), None);
    assert_eq!(store.get("todo/2/status").unwrap(), None);
    if schema == 1 {
        assert_eq!(
            store.get("todo/1/done").unwrap(),
            Some(StateValue::Boolean(false))
        );
        assert_eq!(store.get("todo/1/status").unwrap(), None);
    } else {
        assert_eq!(store.get("todo/1/done").unwrap(), None);
        assert_eq!(
            store.get("todo/1/status").unwrap(),
            Some(StateValue::Text("active".into()))
        );
    }
    store.rollback().unwrap();
}
