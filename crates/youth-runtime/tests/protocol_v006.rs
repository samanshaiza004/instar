//! Protocol 0.0.6 Editor-node plumbing and multi-version coexistence regressions.

mod common;

use common::{counter_component, test_component};
use youth_runtime::{AppLifecycle, YouthApp};
use youth_tree::{Limits, NodeData, NodeId, Tree};

const EDITOR_CANONICAL: &str = "root #1\n└── editor #2 document-revision=42 \"Scratchpad draft\"\n";

#[test]
fn v006_editor_component_mounts_and_preserves_both_editor_fields() {
    let mut app =
        YouthApp::load(test_component("youth-editor-v006")).expect("v0.0.6 editor component loads");
    assert_eq!(app.inspect().world, "youth:app/application@0.0.6");

    let mounted = app.mount().expect("v0.0.6 editor component mounts");
    assert_eq!(mounted.revision, 0);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
    assert_eq!(
        mounted
            .nodes
            .iter()
            .find(|node| node.id == NodeId::new(2).unwrap())
            .map(|node| &node.data),
        Some(&NodeData::Editor {
            document_revision: 42,
            text: "Scratchpad draft".into(),
        })
    );

    let mounted_tree = app.tree().expect("mounted tree is retained");
    assert_eq!(mounted_tree.canonical(), EDITOR_CANONICAL);
    let canonical_snapshot = mounted_tree.to_snapshot();
    let round_trip = Tree::from_snapshot(canonical_snapshot.clone(), &Limits::default()).unwrap();
    assert_eq!(round_trip.to_snapshot(), canonical_snapshot);
    assert_eq!(round_trip.canonical(), EDITOR_CANONICAL);

    let resynced = app.resync().expect("v0.0.6 editor component resyncs");
    assert_eq!(resynced, canonical_snapshot);
}

#[test]
fn v002_through_v006_components_mount_without_cross_version_drift() {
    let fixtures = [
        (
            counter_component(),
            "youth:app/application@0.0.2",
            "v0.0.2 counter",
        ),
        (
            test_component("youth-legacy-v003"),
            "youth:app/application@0.0.3",
            "v0.0.3 legacy",
        ),
        (
            test_component("youth-time-stub"),
            "youth:app/application@0.0.4",
            "v0.0.4 time",
        ),
        (
            test_component("youth-sdk-tally"),
            "youth:app/application@0.0.6",
            "v0.0.6 SDK tally",
        ),
        (
            test_component("youth-editor-v006"),
            "youth:app/application@0.0.6",
            "v0.0.6 editor",
        ),
    ];

    for (component, expected_world, label) in fixtures {
        let mut app =
            YouthApp::load(component).unwrap_or_else(|error| panic!("{label} loads: {error}"));
        assert_eq!(app.inspect().world, expected_world, "{label}");
        app.mount()
            .unwrap_or_else(|error| panic!("{label} mounts: {error}"));
        assert_eq!(app.lifecycle(), AppLifecycle::Mounted, "{label}");
        let canonical_snapshot = app.tree().unwrap().to_snapshot();
        let round_trip = Tree::from_snapshot(canonical_snapshot.clone(), &Limits::default())
            .unwrap_or_else(|error| panic!("{label} snapshot revalidates: {error}"));
        assert_eq!(round_trip.to_snapshot(), canonical_snapshot, "{label}");
    }
}
