//! Event turns, resynchronization, inspection, and lifecycle behavior.

mod common;

use common::{MOUNTED_TREE, counter_component};
use youth_runtime::{AppLifecycle, RuntimeErrorCategory, YouthApp};
use youth_tree::NodeId;

const COUNT_ONE_TREE: &str =
    "root #1\n└── box #2\n    ├── text #3 \"Count: 1\"\n    └── button #4 \"Increment\"\n";
const COUNT_THREE_TREE: &str =
    "root #1\n└── box #2\n    ├── text #3 \"Count: 3\"\n    └── button #4 \"Increment\"\n";

fn id(value: u64) -> NodeId {
    NodeId::new(value).expect("test IDs are nonzero")
}

#[test]
fn activate_once_commits_one_patch() {
    let mut app = YouthApp::load(counter_component()).expect("counter component loads");
    app.mount().expect("mount succeeds");

    let receipt = app.activate(id(4)).expect("activation succeeds");

    assert_eq!(receipt.turn_id, 1);
    assert_eq!(receipt.event_sequence, 1);
    assert_eq!(receipt.base_revision, 0);
    assert_eq!(receipt.next_revision, 1);
    assert_eq!(receipt.patch_count, 1);
    assert!(receipt.committed);
    let tree = app.tree().expect("tree remains mounted");
    assert_eq!(tree.revision(), 1);
    assert_eq!(tree.canonical(), COUNT_ONE_TREE);
}

#[test]
fn three_activations_have_strict_sequences_and_revision_three() {
    let mut app = YouthApp::load(counter_component()).expect("counter component loads");
    app.mount().expect("mount succeeds");

    let receipts = (0..3)
        .map(|_| app.activate(id(4)).expect("activation succeeds"))
        .collect::<Vec<_>>();

    assert_eq!(
        receipts
            .iter()
            .map(|receipt| receipt.event_sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let tree = app.tree().expect("tree remains mounted");
    assert_eq!(tree.revision(), 3);
    assert_eq!(tree.canonical(), COUNT_THREE_TREE);
}

#[test]
fn resync_after_activations_replaces_the_tree_at_the_live_revision() {
    let mut app = YouthApp::load(counter_component()).expect("counter component loads");
    app.mount().expect("mount succeeds");
    for _ in 0..3 {
        app.activate(id(4)).expect("activation succeeds");
    }

    let snapshot = app.resync().expect("resync succeeds");

    assert_eq!(snapshot.revision, 3);
    assert_eq!(
        app.tree().expect("tree remains mounted").canonical(),
        COUNT_THREE_TREE
    );
}

#[test]
fn rejected_text_activation_does_not_mutate_or_fault() {
    let mut app = YouthApp::load(counter_component()).expect("counter component loads");
    app.mount().expect("mount succeeds");

    let error = app
        .activate(id(3))
        .expect_err("text activation is rejected");

    assert_eq!(error.category(), RuntimeErrorCategory::GuestRejected);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
    let tree = app.tree().expect("tree remains mounted");
    assert_eq!(tree.revision(), 0);
    assert_eq!(tree.canonical(), MOUNTED_TREE);
    let inspection = app.inspect();
    assert_eq!(inspection.last_event_sequence, Some(1));
    assert_eq!(inspection.next_event_sequence, Some(2));
    assert!(
        !inspection
            .last_turn
            .expect("rejected turn metrics are retained")
            .committed
    );
}

#[test]
fn lifecycle_rejects_calls_before_mount_and_after_stop() {
    let mut app = YouthApp::load(counter_component()).expect("counter component loads");
    assert_eq!(
        app.activate(id(4))
            .expect_err("activate before mount fails")
            .category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
    assert_eq!(
        app.resync()
            .expect_err("resync before mount fails")
            .category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
    app.mount().expect("mount succeeds");
    assert_eq!(
        app.mount().expect_err("second mount fails").category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
    app.stop().expect("stop succeeds");
    assert_eq!(app.lifecycle(), AppLifecycle::Stopped);
    assert!(app.tree().is_none());
    assert_eq!(
        app.activate(id(4))
            .expect_err("activate after stop fails")
            .category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
    assert_eq!(
        app.resync()
            .expect_err("resync after stop fails")
            .category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
    assert_eq!(
        app.stop().expect_err("second stop fails").category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
}

#[test]
fn inspect_reports_mounted_tree_metrics_and_last_turn() {
    let mut app = YouthApp::load(counter_component()).expect("counter component loads");
    app.mount().expect("mount succeeds");
    let receipt = app.activate(id(4)).expect("activation succeeds");

    let inspection = app.inspect();

    assert_eq!(inspection.lifecycle, AppLifecycle::Mounted);
    assert_eq!(inspection.world, "youth:app/application@0.0.2");
    assert_eq!(inspection.current_revision, Some(1));
    assert_eq!(inspection.node_count, 4);
    assert_eq!(inspection.depth, 3);
    assert_eq!(inspection.last_event_sequence, Some(1));
    assert_eq!(inspection.next_event_sequence, Some(2));
    assert_eq!(inspection.last_turn, Some(receipt));
    assert!(inspection.fault.is_none());
    assert_eq!(inspection.canonical_tree, COUNT_ONE_TREE);
}
