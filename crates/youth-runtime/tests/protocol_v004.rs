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
fn v002_counter_and_v003_sdk_tally_still_load_and_mount() {
    let mut counter = YouthApp::load(counter_component()).expect("v0.0.2 counter loads");
    assert_eq!(counter.inspect().world, "youth:app/application@0.0.2");
    counter.mount().expect("v0.0.2 counter mounts");

    let mut tally =
        YouthApp::load(test_component("youth-sdk-tally")).expect("v0.0.3 SDK tally loads");
    assert_eq!(tally.inspect().world, "youth:app/application@0.0.3");
    tally.mount().expect("v0.0.3 SDK tally mounts");
}
