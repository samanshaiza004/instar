//! Headless real-component tests for youth:editor/session@0.0.1.

mod common;

use common::test_component;
use youth_runtime::{EditorLocalEdit, YouthApp};
use youth_tree::{NodeData, NodeId};

const INITIAL: &str = "42|0|Scratchpad draft";

fn mounted() -> YouthApp {
    let mut app = YouthApp::load(test_component("youth-editor-capability-v006"))
        .expect("Editor capability fixture loads");
    app.mount().expect("Editor capability fixture mounts");
    app
}

fn activate(app: &mut YouthApp, label: &str) {
    let id = app
        .tree()
        .expect("mounted tree exists")
        .to_snapshot()
        .nodes
        .into_iter()
        .find_map(|node| match node.data {
            NodeData::Button {
                label: candidate, ..
            } if candidate == label => Some(node.id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("button {label:?} exists"));
    app.activate(id)
        .unwrap_or_else(|error| panic!("{label:?} activation succeeds: {error}"));
}

fn status(app: &YouthApp) -> String {
    app.tree()
        .expect("mounted tree exists")
        .to_snapshot()
        .nodes
        .into_iter()
        .find_map(|node| (node.id == NodeId::new(3).unwrap()).then_some(node.data))
        .and_then(|data| match data {
            NodeData::Text { value } => Some(value),
            _ => None,
        })
        .expect("status text exists")
}

#[test]
fn snapshot_returns_current_revision_sequence_and_text() {
    let mut app = mounted();
    activate(&mut app, "Snapshot");
    assert_eq!(status(&app), INITIAL);
}

#[test]
fn accept_preserves_text_updates_revision_and_keeps_sequence() {
    let mut app = mounted();
    activate(&mut app, "Accept");
    assert_eq!(status(&app), "43|0|Scratchpad draft");
    activate(&mut app, "Snapshot");
    assert_eq!(status(&app), "43|0|Scratchpad draft");
}

#[test]
fn stale_accept_revision_or_sequence_does_not_mutate_the_session() {
    for label in ["Accept stale revision", "Accept stale sequence"] {
        let mut app = mounted();
        activate(&mut app, label);
        assert_eq!(status(&app), INITIAL, "{label}");
        activate(&mut app, "Snapshot");
        assert_eq!(status(&app), INITIAL, "{label} subsequent snapshot");
    }
}

#[test]
fn replace_installs_authoritative_text_revision_and_base_sequence() {
    let mut app = mounted();
    activate(&mut app, "Replace");
    assert_eq!(status(&app), "50|0|Authoritative text");
    activate(&mut app, "Snapshot");
    assert_eq!(status(&app), "50|0|Authoritative text");
}

#[test]
fn stale_replace_revision_or_sequence_does_not_mutate_the_session() {
    for label in ["Replace stale revision", "Replace stale sequence"] {
        let mut app = mounted();
        activate(&mut app, label);
        assert_eq!(status(&app), INITIAL, "{label}");
        activate(&mut app, "Snapshot");
        assert_eq!(status(&app), INITIAL, "{label} subsequent snapshot");
    }
}

#[test]
fn all_calls_report_unknown_editor_without_panicking() {
    let mut app = mounted();
    activate(&mut app, "Unknown");
    assert_eq!(status(&app), "unknown-editor");
}

#[test]
fn replace_current_clears_both_undo_and_redo_history() {
    let mut app = mounted();
    app.edit_editor_locally(
        NodeId::new(2).unwrap(),
        EditorLocalEdit::InsertText("x".to_owned()),
    )
    .unwrap();
    app.edit_editor_locally(NodeId::new(2).unwrap(), EditorLocalEdit::Backspace)
        .unwrap();
    app.edit_editor_locally(NodeId::new(2).unwrap(), EditorLocalEdit::Undo)
        .unwrap();

    activate(&mut app, "Replace current");
    assert_eq!(status(&app), "60|0|Current authoritative text");
    let undone = app
        .edit_editor_locally(NodeId::new(2).unwrap(), EditorLocalEdit::Undo)
        .expect("post-replace undo is safe");
    assert_eq!(undone.edit_sequence, 0);
    assert_eq!(
        app.editor_snapshot(NodeId::new(2).unwrap())
            .expect("explicit editor snapshot succeeds")
            .text,
        "Current authoritative text"
    );
    let redone = app
        .edit_editor_locally(NodeId::new(2).unwrap(), EditorLocalEdit::Redo)
        .expect("post-replace redo is safe");
    assert_eq!(redone, undone, "replace clears redo as well as undo");
}

#[test]
fn locally_dirty_tracks_current_accept_and_unknown_editors() {
    let mut app = mounted();
    let editor = NodeId::new(2).unwrap();
    assert_eq!(app.editor_locally_dirty(editor), Some(false));
    assert_eq!(app.editor_locally_dirty(NodeId::new(999).unwrap()), None);

    app.edit_editor_locally(editor, EditorLocalEdit::InsertText("!".to_owned()))
        .expect("local edit succeeds");
    assert_eq!(app.editor_locally_dirty(editor), Some(true));

    activate(&mut app, "Accept current");
    assert_eq!(status(&app), "43|1|Scratchpad draft!");
    assert_eq!(app.editor_locally_dirty(editor), Some(false));
}
