mod common;

use common::counter_component;
use tempfile::tempdir;
use youth_runtime::{AppId, RuntimeLimits, StateLocation, YouthApp, YouthAppConfig};
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
