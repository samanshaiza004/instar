mod common;

use common::test_component;
use youth_runtime::{AppId, StateLocation, YouthApp, YouthAppConfig};
use youth_sdk::named_node_id;
use youth_tree::{NodeData, NodeId};

fn config(component: std::path::PathBuf, state: &std::path::Path) -> YouthAppConfig {
    YouthAppConfig {
        component_path: component,
        app_id: AppId::parse("dev.youth.sdk-tally").expect("fixture app ID is valid"),
        state: StateLocation::File(state.to_owned()),
        limits: youth_runtime::RuntimeLimits::default(),
        workspace: None,
    }
}

fn text(snapshot: &youth_tree::TreeSnapshot) -> &str {
    snapshot
        .nodes
        .iter()
        .find_map(|node| match &node.data {
            NodeData::Text { value } if node.id.get() == named_node_id("count") => Some(value),
            _ => None,
        })
        .expect("snapshot contains the named count text")
}

#[test]
fn sdk_owns_protocol_bookkeeping_and_state_survives_restart() {
    let component = test_component("youth-sdk-tally");
    let temporary = tempfile::tempdir().expect("temporary directory is created");
    let state = temporary.path().join("state.sqlite3");

    let mut first = YouthApp::load_config(config(component.clone(), &state))
        .expect("SDK component loads and links");
    // youth-sdk-tally is a local-path youth-sdk fixture, so it always tracks
    // whichever protocol the SDK crate in this workspace currently targets
    // (0.0.8, since the text-document capability), not a version frozen
    // at this test's writing.
    assert_eq!(first.inspect().world, "youth:app/application@0.0.8");
    assert_eq!(
        youth_runtime::validate_component(&component)
            .expect("SDK component validates")
            .world,
        "youth:app/application@0.0.8"
    );
    let mounted = first.mount().expect("SDK mount succeeds");
    assert_eq!(mounted.revision, 0);
    assert_eq!(text(&mounted), "Count: 0");

    let receipt = first
        .activate(NodeId::new(named_node_id("increment")).expect("named IDs are nonzero"))
        .expect("SDK activation succeeds");
    assert_eq!(receipt.base_revision, 0);
    assert_eq!(receipt.next_revision, 1);
    assert_eq!(receipt.processed_through, 1);
    assert_eq!(receipt.patch_count, 1);
    assert_eq!(
        text(&first.snapshot().expect("host snapshot succeeds")),
        "Count: 1"
    );

    drop(first);
    let mut restarted =
        YouthApp::load_config(config(component, &state)).expect("restarted component loads");
    let mounted = restarted.mount().expect("restarted component mounts");
    assert_eq!(text(&mounted), "Count: 1");
    let resynced = restarted.resync().expect("read-only SDK resync succeeds");
    assert_eq!(resynced.revision, 0);
    assert_eq!(text(&resynced), "Count: 1");
}
