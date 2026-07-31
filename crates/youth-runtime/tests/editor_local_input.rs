//! Headless proof that ordinary Editor typing stays entirely host-local.

mod common;

use std::sync::Arc;

use common::test_component;
use youth_runtime::{
    ClipboardService, EditorLocalEdit, RecordingClipboardService, YouthAppConfig, YouthAppHandle,
};
use youth_tree::NodeId;

const INITIAL: &str = "Scratchpad draft";

fn id(value: u64) -> NodeId {
    NodeId::new(value).expect("test IDs are nonzero")
}

#[tokio::test]
async fn ten_thousand_local_edits_make_zero_guest_calls() {
    let clipboard = RecordingClipboardService::default();
    clipboard
        .write_text(" from clipboard")
        .expect("recording clipboard accepts text");
    let mut config = YouthAppConfig::ephemeral(test_component("youth-editor-capability-v006"));
    config.limits.time.clipboard_service = Arc::new(clipboard);
    let app = YouthAppHandle::spawn(config).expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    let baseline = app.inspect().await.expect("baseline inspection succeeds");
    let baseline_guest_calls = baseline.guest_call_count;
    let mut result = None;
    for _ in 0..10_000 {
        result = Some(
            app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("x".to_owned()))
                .await
                .expect("host-local insert succeeds"),
        );
    }

    let result = result.expect("the edit loop produces a result");
    assert_eq!(result.document_revision, 42);
    assert_eq!(result.edit_sequence, 10_000);
    assert_eq!(
        result.text,
        format!("Scratchpad draft{}", "x".repeat(10_000))
    );
    let after_local_edits = app.inspect().await.expect("post-edit inspection succeeds");
    assert_eq!(
        after_local_edits.guest_call_count, baseline_guest_calls,
        "10,000 local edits must not enter the guest"
    );
    assert_eq!(after_local_edits.last_event_sequence, None);
    assert!(after_local_edits.last_turn.is_none());

    let undone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("host-local undo succeeds");
    assert_eq!(undone.text, INITIAL);
    let redone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Redo)
        .await
        .expect("host-local redo succeeds");
    assert_eq!(redone.text, result.text);
    let pasted = app
        .edit_editor_locally(id(2), EditorLocalEdit::Paste)
        .await
        .expect("host-local paste succeeds");
    assert!(pasted.text.ends_with(" from clipboard"));
    let after_history_edits = app.inspect().await.expect("history inspection succeeds");
    assert_eq!(
        after_history_edits.guest_call_count, baseline_guest_calls,
        "undo, redo, and paste must not enter the guest"
    );

    app.activate(id(4))
        .await
        .expect("real Snapshot activation succeeds");
    let after_activation = app.inspect().await.expect("control inspection succeeds");
    assert_eq!(
        after_activation.guest_call_count,
        baseline_guest_calls + 1,
        "the counter must detect a real guest handle call"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn consecutive_inserts_merge_and_undo_redo_are_exact_and_bounded_by_history() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    for text in ["a", "b", "c"] {
        app.edit_editor_locally(id(2), EditorLocalEdit::InsertText(text.to_owned()))
            .await
            .expect("typing succeeds");
    }
    let undone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("merged typing undoes");
    assert_eq!(undone.text, INITIAL, "one undo removes the whole abc group");
    assert_eq!(undone.edit_sequence, 4);

    let extra_undo = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("undo past history is safe");
    assert_eq!(extra_undo, undone, "exhausted undo is a complete no-op");

    let redone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Redo)
        .await
        .expect("redo restores typing");
    assert_eq!(redone.text, format!("{INITIAL}abc"));
    let extra_redo = app
        .edit_editor_locally(id(2), EditorLocalEdit::Redo)
        .await
        .expect("redo past history is safe");
    assert_eq!(extra_redo, redone, "exhausted redo is a complete no-op");

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn backspace_is_separate_and_a_new_edit_after_undo_clears_redo() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("abc".to_owned()))
        .await
        .expect("insert succeeds");
    app.edit_editor_locally(id(2), EditorLocalEdit::Backspace)
        .await
        .expect("backspace succeeds");
    let undone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("backspace undo succeeds");
    assert_eq!(undone.text, format!("{INITIAL}abc"));

    app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("!".to_owned()))
        .await
        .expect("fresh edit succeeds");
    let redo = app
        .edit_editor_locally(id(2), EditorLocalEdit::Redo)
        .await
        .expect("cleared redo is safe");
    assert_eq!(redo.text, format!("{INITIAL}abc!"));

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn paste_is_an_isolated_undo_unit_between_typing_groups() {
    let clipboard = RecordingClipboardService::default();
    clipboard
        .write_text("PASTE")
        .expect("recording clipboard accepts text");
    let mut config = YouthAppConfig::ephemeral(test_component("youth-editor-capability-v006"));
    config.limits.time.clipboard_service = Arc::new(clipboard);
    let app = YouthAppHandle::spawn(config).expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("a".to_owned()))
        .await
        .unwrap();
    app.edit_editor_locally(id(2), EditorLocalEdit::Paste)
        .await
        .unwrap();
    app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("b".to_owned()))
        .await
        .unwrap();

    let without_b = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .unwrap();
    assert_eq!(without_b.text, format!("{INITIAL}aPASTE"));
    let without_paste = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .unwrap();
    assert_eq!(without_paste.text, format!("{INITIAL}a"));
    let without_a = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .unwrap();
    assert_eq!(without_a.text, INITIAL);

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn absent_clipboard_paste_is_a_safe_no_op_without_an_empty_history_group() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    let pasted = app
        .edit_editor_locally(id(2), EditorLocalEdit::Paste)
        .await
        .expect("empty clipboard paste succeeds");
    assert_eq!(pasted.edit_sequence, 0);
    assert_eq!(pasted.text, INITIAL);
    let undone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("undo after empty paste succeeds");
    assert_eq!(undone, pasted);

    app.stop().await.expect("worker stops");
}
