//! Protocol 0.0.4 scheduling plumbing and coexistence regressions.

mod common;

use common::{counter_component, test_component};
use youth_runtime::{AppLifecycle, YouthApp};
use youth_tree::{NodeData, NodeId};

#[test]
fn time_stub_loads_reports_v004_and_mounts() {
    let component = test_component("youth-time-stub");
    let mut app = YouthApp::load(component).expect("time stub component loads");

    assert_eq!(app.inspect().world, "youth:app/application@0.0.4");
    let snapshot = app.mount().expect("time stub mounts");
    assert_eq!(snapshot.revision, 0);
    assert_eq!(snapshot.nodes.len(), 3);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
}

#[test]
fn time_stub_observes_unavailable_without_faulting() {
    let mut app =
        YouthApp::load(test_component("youth-time-stub")).expect("time stub component loads");
    app.mount().expect("time stub mounts");

    let receipt = app
        .activate(NodeId::new(2).expect("fixture node ID is valid"))
        .expect("unavailable scheduling error is handled by the guest");

    assert!(receipt.committed);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
    let snapshot = app.snapshot().expect("mounted snapshot is available");
    assert!(snapshot.nodes.iter().any(|node| {
        matches!(
            &node.data,
            NodeData::Text { value } if value == "unavailable"
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

#[test]
fn sdk_time_handles_unavailable_without_faulting() {
    let mut app =
        YouthApp::load(test_component("youth-sdk-time")).expect("SDK time component loads");
    let snapshot = app.mount().expect("SDK time component mounts");
    let button = snapshot
        .nodes
        .iter()
        .find(|node| matches!(node.data, NodeData::Button { .. }))
        .expect("fixture has a button")
        .id;

    let receipt = app
        .activate(button)
        .expect("SDK guest handles unavailable scheduling");

    assert!(receipt.committed);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
    assert!(app.snapshot().expect("snapshot").nodes.iter().any(|node| {
        matches!(
            &node.data,
            NodeData::Text { value } if value == "unavailable"
        )
    }));
}
