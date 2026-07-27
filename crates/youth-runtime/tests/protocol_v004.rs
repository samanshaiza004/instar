//! Protocol 0.0.4 scheduling plumbing and coexistence regressions.

mod common;

use common::{counter_component, test_component};
use tempfile::tempdir;
use youth_runtime::{AppId, AppLifecycle, RuntimeLimits, StateLocation, YouthApp, YouthAppConfig};
use youth_state::{GuestCallPhase, StateLimits, StateStore};
use youth_tree::{NodeData, NodeId};

#[test]
fn time_component_loads_reports_v004_and_mounts() {
    let component = test_component("youth-time-stub");
    let mut app = YouthApp::load(component).expect("time component loads");

    assert_eq!(app.inspect().world, "youth:app/application@0.0.4");
    let snapshot = app.mount().expect("time component mounts");
    assert_eq!(snapshot.revision, 0);
    assert_eq!(snapshot.nodes.len(), 6);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
}

#[test]
fn direct_time_component_creates_a_schedule_without_faulting() {
    let mut app = YouthApp::load(test_component("youth-time-stub")).expect("time component loads");
    app.mount().expect("time component mounts");

    let receipt = app
        .activate(NodeId::new(4).expect("fixture node ID is valid"))
        .expect("schedule creation commits");

    assert!(receipt.committed);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
    let snapshot = app.snapshot().expect("mounted snapshot is available");
    assert!(snapshot.nodes.iter().any(|node| {
        matches!(
            &node.data,
            NodeData::Text { value } if value == "scheduled"
        )
    }));
}

#[test]
fn v002_v003_and_v004_components_coexist() {
    let mut counter = YouthApp::load(counter_component()).expect("v0.0.2 counter loads");
    assert_eq!(counter.inspect().world, "youth:app/application@0.0.2");
    counter.mount().expect("v0.0.2 counter mounts");

    let mut legacy =
        YouthApp::load(test_component("youth-legacy-v003")).expect("v0.0.3 legacy guest loads");
    assert_eq!(legacy.inspect().world, "youth:app/application@0.0.3");
    legacy.mount().expect("v0.0.3 legacy guest mounts");

    let mut tally =
        YouthApp::load(test_component("youth-sdk-tally")).expect("v0.0.4 SDK tally loads");
    assert_eq!(tally.inspect().world, "youth:app/application@0.0.4");
    tally.mount().expect("v0.0.4 SDK tally mounts");

    let mut sdk_time =
        YouthApp::load(test_component("youth-sdk-time")).expect("v0.0.4 SDK time guest loads");
    assert_eq!(sdk_time.inspect().world, "youth:app/application@0.0.4");
    sdk_time.mount().expect("v0.0.4 SDK time guest mounts");
}

fn sdk_time_config(database: &std::path::Path) -> YouthAppConfig {
    YouthAppConfig {
        component_path: test_component("youth-sdk-time"),
        app_id: AppId::parse("dev.youth.time").unwrap(),
        state: StateLocation::File(database.to_owned()),
        limits: RuntimeLimits::default(),
    }
}

#[test]
fn sdk_time_schedule_survives_a_real_runtime_restart_and_is_hidden_from_state_get() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    {
        let mut app = YouthApp::load_config(sdk_time_config(&database))
            .expect("first SDK time runtime loads");
        let snapshot = app.mount().expect("SDK time component mounts");
        let button = snapshot
            .nodes
            .iter()
            .find(|node| matches!(node.data, NodeData::Button { .. }))
            .expect("fixture has a button")
            .id;
        let receipt = app.activate(button).expect("SDK schedule creation commits");
        assert!(receipt.committed);
        assert!(app.snapshot().expect("snapshot").nodes.iter().any(|node| {
            matches!(
                &node.data,
                NodeData::Text { value } if value == "scheduled"
            )
        }));
    }

    {
        let mut restarted = YouthApp::load_config(sdk_time_config(&database))
            .expect("second SDK time runtime loads against the same state file");
        restarted
            .mount()
            .expect("restarted SDK time component mounts");
    }

    let mut store =
        StateStore::open(StateLocation::File(database), StateLimits::default()).unwrap();
    let schedules = store.schedules().unwrap();
    assert_eq!(schedules.len(), 1);
    assert_eq!(
        schedules[0].notification,
        Some(("Youth timer".into(), "Time elapsed".into()))
    );
    store.begin(GuestCallPhase::Resync).unwrap();
    assert_eq!(store.get("__schedule_storage_probe").unwrap(), None);
    store.rollback().unwrap();
}
