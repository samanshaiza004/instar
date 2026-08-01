//! Proof that live Editor sessions expose a real AccessKit-shaped
//! accessibility snapshot, synced the same way as `TextPresentation`.

mod common;

use common::test_component;
use youth_runtime::{EditorLocalEdit, YouthAppHandle};
use youth_tree::NodeId;

fn id(value: u64) -> NodeId {
    NodeId::new(value).expect("test IDs are nonzero")
}

#[tokio::test]
async fn a_freshly_mounted_editor_has_a_populated_accessibility_snapshot() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    let snapshot = app
        .presentation()
        .editor_accessibility(id(2))
        .expect("a live Editor session has an accessibility snapshot");
    assert_eq!(snapshot.node.role(), accesskit::Role::MultilineTextInput);
    assert!(
        snapshot.node.supports_action(accesskit::Action::Focus),
        "an Editor must be reachable via accessibility focus navigation"
    );
    assert!(
        snapshot
            .node
            .supports_action(accesskit::Action::SetTextSelection),
        "an Editor must accept accessibility-driven selection changes"
    );
    assert!(
        !snapshot.extra_nodes.is_empty(),
        "non-empty declared text produces at least one TextRun child node"
    );
    assert_eq!(
        snapshot.node.children().len(),
        snapshot.extra_nodes.len(),
        "every produced child node is linked from the editor's own node"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn the_accessibility_snapshot_tracks_local_edits() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");
    let before = app
        .presentation()
        .editor_accessibility(id(2))
        .expect("initial snapshot exists");

    app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("!".to_owned()))
        .await
        .expect("host-local insert succeeds");

    let after = app
        .presentation()
        .editor_accessibility(id(2))
        .expect("post-edit snapshot exists");
    assert_ne!(
        before.extra_nodes.len(),
        0,
        "sanity: the pre-edit snapshot already had run nodes"
    );
    // A changed run of text produces a different (freshly allocated) set of
    // child node ids each sync -- the snapshot is not stale after a local
    // edit that never entered the guest.
    assert_ne!(
        before
            .extra_nodes
            .iter()
            .map(|(_, node)| node.value().map(str::to_owned))
            .collect::<Vec<_>>(),
        after
            .extra_nodes
            .iter()
            .map(|(_, node)| node.value().map(str::to_owned))
            .collect::<Vec<_>>(),
        "the accessibility snapshot's text must reflect the local edit"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn a_set_text_selection_action_is_guest_turn_free_and_updates_the_live_cursor() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");
    let baseline = app.inspect().await.expect("baseline inspection succeeds");

    let snapshot = app
        .presentation()
        .editor_accessibility(id(2))
        .expect("a live Editor session has an accessibility snapshot");
    let (run_id, _) = *snapshot
        .extra_nodes
        .first()
        .expect("non-empty text has at least one run node");

    let result = app
        .edit_editor_locally(
            id(2),
            EditorLocalEdit::SetSelectionFromAccessKit(accesskit::TextSelection {
                anchor: accesskit::TextPosition {
                    node: run_id,
                    character_index: 0,
                },
                focus: accesskit::TextPosition {
                    node: run_id,
                    character_index: 5,
                },
            }),
        )
        .await
        .expect("accessibility-driven selection succeeds");
    assert_eq!(result.selection, Some(0..5));
    assert_eq!(
        result.edit_sequence, 0,
        "a selection change is pure cursor/selection movement, not a content edit"
    );

    let after = app
        .inspect()
        .await
        .expect("post-action inspection succeeds");
    assert_eq!(
        after.guest_call_count, baseline.guest_call_count,
        "AccessKit-driven selection must never enter the guest"
    );

    app.stop().await.expect("worker stops");
}
