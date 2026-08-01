//! Headless proof that ordinary Editor typing stays entirely host-local.

mod common;

use std::sync::Arc;

use common::test_component;
use youth_runtime::{
    ClipboardService, EditorLocalEdit, Movement, RecordingClipboardService, YouthAppConfig,
    YouthAppHandle,
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
    // Uses a deterministic empty `RecordingClipboardService` rather than
    // `spawn_ephemeral`'s real `SystemClipboardService` -- this test
    // specifically wants an empty clipboard, and the real OS pasteboard's
    // contents are outside this test's control (a developer running the
    // suite may well have something copied).
    let mut config = YouthAppConfig::ephemeral(test_component("youth-editor-capability-v006"));
    config.limits.time.clipboard_service = Arc::new(RecordingClipboardService::default());
    let app = YouthAppHandle::spawn(config).expect("Editor worker starts");
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

#[tokio::test]
async fn insert_happens_at_the_real_cursor_position_not_always_at_the_end() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    // A freshly mounted session starts with the cursor at the end, so move
    // it to the very start before inserting.
    for _ in 0..INITIAL.len() {
        app.edit_editor_locally(id(2), EditorLocalEdit::MoveCursor(Movement::Left))
            .await
            .expect("cursor moves left");
    }
    let inserted = app
        .edit_editor_locally(id(2), EditorLocalEdit::InsertText(">>".to_owned()))
        .await
        .expect("insert at cursor succeeds");
    assert_eq!(inserted.text, format!(">>{INITIAL}"));
    assert_eq!(inserted.cursor, 2);

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn cursor_movement_is_a_real_undo_group_boundary() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("abc".to_owned()))
        .await
        .expect("first typing group");
    app.edit_editor_locally(id(2), EditorLocalEdit::MoveCursor(Movement::Left))
        .await
        .expect("movement closes the group");
    app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("XY".to_owned()))
        .await
        .expect("second typing group");
    let before_undo = app
        .edit_editor_locally(id(2), EditorLocalEdit::MoveCursor(Movement::Home))
        .await
        .expect("movement is not itself undoable");

    // Undoing once removes only the second group ("XY"), not "abc" too.
    let undo_one = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("first undo removes only the second group");
    assert_eq!(undo_one.text, format!("{INITIAL}abc"));
    let undo_two = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("second undo removes the first group");
    assert_eq!(undo_two.text, INITIAL);
    let extra_undo = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("exhausted undo is safe");
    assert_eq!(
        extra_undo, undo_two,
        "pure cursor movement left nothing to undo"
    );
    assert_ne!(
        before_undo.text, INITIAL,
        "sanity: content existed before undoing"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn undo_restores_cursor_and_selection_not_just_text() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    // Move to the start (a defined, known position) before the edit whose
    // undo we're about to verify.
    for _ in 0..INITIAL.len() {
        app.edit_editor_locally(id(2), EditorLocalEdit::MoveCursor(Movement::Left))
            .await
            .expect("cursor moves left");
    }
    let before_cursor = app
        .edit_editor_locally(id(2), EditorLocalEdit::MoveCursor(Movement::Left))
        .await
        .expect("already-at-start movement is a safe no-op")
        .cursor;
    assert_eq!(before_cursor, 0);

    app.edit_editor_locally(id(2), EditorLocalEdit::InsertText("Z".to_owned()))
        .await
        .expect("insert at start succeeds");

    let undone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("undo succeeds");
    assert_eq!(undone.text, INITIAL);
    assert_eq!(
        undone.cursor, 0,
        "undo must restore the cursor to where it was before the edit, not just the text"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn extend_selection_produces_a_real_selection_range_end_to_end() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    for _ in 0..INITIAL.len() {
        app.edit_editor_locally(id(2), EditorLocalEdit::MoveCursor(Movement::Left))
            .await
            .expect("cursor moves left");
    }
    app.edit_editor_locally(id(2), EditorLocalEdit::ExtendSelection(Movement::Right))
        .await
        .expect("selection extends");
    let selected = app
        .edit_editor_locally(id(2), EditorLocalEdit::ExtendSelection(Movement::Right))
        .await
        .expect("selection extends further");
    assert_eq!(selected.selection, Some(0..2));

    // Typing with a live selection replaces it, exactly like a real editor.
    let replaced = app
        .edit_editor_locally(id(2), EditorLocalEdit::InsertText("Q".to_owned()))
        .await
        .expect("insert replaces the selection");
    assert_eq!(replaced.text, format!("Q{}", &INITIAL[2..]));

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn ime_composition_updates_are_guest_turn_free_and_do_not_advance_edit_sequence() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");
    let baseline = app.inspect().await.expect("baseline inspection succeeds");

    let first = app
        .edit_editor_locally(
            id(2),
            EditorLocalEdit::ImeSetCompose {
                text: "n".to_owned(),
                cursor: Some((0, 1)),
            },
        )
        .await
        .expect("first compose update succeeds");
    assert_eq!(
        first.edit_sequence, 0,
        "preedit is not accepted content yet"
    );
    assert_eq!(
        first.text, INITIAL,
        "snapshot excludes in-progress preedit content"
    );

    let second = app
        .edit_editor_locally(
            id(2),
            EditorLocalEdit::ImeSetCompose {
                text: "ni".to_owned(),
                cursor: Some((0, 2)),
            },
        )
        .await
        .expect("repeated compose update replaces rather than accumulates");
    assert_eq!(second.edit_sequence, 0);
    assert_eq!(second.text, INITIAL);

    let after = app
        .inspect()
        .await
        .expect("post-compose inspection succeeds");
    assert_eq!(
        after.guest_call_count, baseline.guest_call_count,
        "IME preedit updates must never enter the guest"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn ime_clear_compose_discards_preedit_with_nothing_to_undo() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");

    app.edit_editor_locally(
        id(2),
        EditorLocalEdit::ImeSetCompose {
            text: "n".to_owned(),
            cursor: Some((0, 1)),
        },
    )
    .await
    .expect("compose update succeeds");
    let cleared = app
        .edit_editor_locally(id(2), EditorLocalEdit::ImeClearCompose)
        .await
        .expect("clearing composition succeeds");
    assert_eq!(cleared.text, INITIAL);
    assert_eq!(cleared.edit_sequence, 0);

    let undone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("undo after a cancelled composition is a safe no-op");
    assert_eq!(
        undone, cleared,
        "a cancelled composition never touched accepted content, so there is nothing to undo"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn ime_finish_compose_commits_the_whole_composition_as_one_undo_group() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");
    let baseline = app.inspect().await.expect("baseline inspection succeeds");

    app.edit_editor_locally(
        id(2),
        EditorLocalEdit::ImeSetCompose {
            text: "n".to_owned(),
            cursor: Some((0, 1)),
        },
    )
    .await
    .expect("first compose update succeeds");
    app.edit_editor_locally(
        id(2),
        EditorLocalEdit::ImeSetCompose {
            text: "ni".to_owned(),
            cursor: Some((0, 2)),
        },
    )
    .await
    .expect("second compose update succeeds");
    let finished = app
        .edit_editor_locally(id(2), EditorLocalEdit::ImeFinishCompose)
        .await
        .expect("finishing composition succeeds");
    assert_eq!(finished.text, format!("{INITIAL}ni"));
    assert_eq!(
        finished.edit_sequence, 1,
        "committing a composition advances edit_sequence exactly once"
    );

    let after = app
        .inspect()
        .await
        .expect("post-commit inspection succeeds");
    assert_eq!(
        after.guest_call_count, baseline.guest_call_count,
        "committing an IME composition must never enter the guest"
    );

    let undone = app
        .edit_editor_locally(id(2), EditorLocalEdit::Undo)
        .await
        .expect("undo succeeds");
    assert_eq!(
        undone.text, INITIAL,
        "one undo removes the whole composition regardless of how many preedit updates it took"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn move_to_point_positions_the_cursor_without_a_guest_turn() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");
    let baseline = app.inspect().await.expect("baseline inspection succeeds");

    let at_start = app
        .edit_editor_locally(id(2), EditorLocalEdit::MoveToPoint { x: 0.0, y: 0.0 })
        .await
        .expect("click near the start succeeds");
    assert_eq!(at_start.cursor, 0);
    assert_eq!(at_start.selection, None);

    let at_end = app
        .edit_editor_locally(id(2), EditorLocalEdit::MoveToPoint { x: 1_000.0, y: 0.0 })
        .await
        .expect("click far to the right succeeds");
    assert!(
        at_end.cursor > at_start.cursor,
        "a far-right click must land further into the text than a click at the start"
    );

    let after = app.inspect().await.expect("post-click inspection succeeds");
    assert_eq!(
        after.guest_call_count, baseline.guest_call_count,
        "pointer-driven cursor placement must never enter the guest"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn extend_selection_to_point_selects_from_the_anchor_to_the_current_point() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");
    let baseline = app.inspect().await.expect("baseline inspection succeeds");

    let dragged = app
        .edit_editor_locally(
            id(2),
            EditorLocalEdit::ExtendSelectionToPoint {
                anchor_x: 0.0,
                anchor_y: 0.0,
                x: 1_000.0,
                y: 0.0,
            },
        )
        .await
        .expect("drag from the start to far right succeeds");
    let selection = dragged
        .selection
        .expect("dragging across text produces a real selection");
    assert_eq!(selection.start, 0, "the anchor is the selection's start");
    assert!(
        selection.end > selection.start,
        "the drag's current point extends the selection forward"
    );
    assert_eq!(
        dragged.cursor, selection.end,
        "the reported cursor tracks the moving end of the drag, not the anchor"
    );

    let after = app.inspect().await.expect("post-drag inspection succeeds");
    assert_eq!(
        after.guest_call_count, baseline.guest_call_count,
        "pointer-driven selection must never enter the guest"
    );

    app.stop().await.expect("worker stops");
}

#[tokio::test]
async fn movement_and_selection_operations_are_guest_turn_free() {
    let app = YouthAppHandle::spawn_ephemeral(test_component("youth-editor-capability-v006"))
        .expect("Editor worker starts");
    app.mount().await.expect("Editor fixture mounts");
    let baseline = app.inspect().await.expect("baseline inspection succeeds");

    for _ in 0..50 {
        app.edit_editor_locally(id(2), EditorLocalEdit::MoveCursor(Movement::Left))
            .await
            .expect("move succeeds");
        app.edit_editor_locally(id(2), EditorLocalEdit::ExtendSelection(Movement::Right))
            .await
            .expect("extend succeeds");
    }

    let after = app
        .inspect()
        .await
        .expect("post-movement inspection succeeds");
    assert_eq!(
        after.guest_call_count, baseline.guest_call_count,
        "cursor and selection movement must never enter the guest"
    );

    app.activate(id(4))
        .await
        .expect("real Snapshot activation succeeds");
    let after_activation = app.inspect().await.expect("control inspection succeeds");
    assert_eq!(
        after_activation.guest_call_count,
        baseline.guest_call_count + 1
    );

    app.stop().await.expect("worker stops");
}
