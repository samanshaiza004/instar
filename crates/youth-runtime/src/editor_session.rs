//! Host-owned Editor session bookkeeping.
//!
//! The guest declares an Editor node's initial document, but the host owns the
//! corresponding live session for as long as that stable node remains in the
//! installed tree. A slot remains after its session is destroyed so that a
//! later Editor with the same node ID receives a distinct generation.
//!
//! Text editing itself (Unicode/grapheme correctness, cursor, selection,
//! layout) is delegated to `youth_editor_engine::ParleyEditorEngine` -- this
//! module owns only the session lifecycle, revision/sequence bookkeeping,
//! and undo/redo grouping around it.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;

use youth_editor_engine::{EditorEngine, Movement, ParleyEditorEngine};
use youth_tree::{NodeData, NodeId, Tree};

pub(super) type EditorSessionRegistry = HashMap<NodeId, EditorSessionSlot>;

/// Every newly created process-local session starts at this edit sequence.
pub(super) const EDIT_SEQUENCE_BASE: u64 = 0;

/// Maximum reversible edit groups retained per live Editor session.
///
/// 512 groups keeps ordinary scratchpad undo useful while placing a fixed
/// bound on the number of retained snapshots. The oldest group is discarded
/// when the limit is reached.
const UNDO_GROUP_LIMIT: usize = 512;

/// The host-owned Editor state after one accepted local edit operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorLocalEditResult {
    pub document_revision: u64,
    pub edit_sequence: u64,
    pub text: String,
    pub cursor: usize,
    pub selection: Option<Range<usize>>,
}

/// The local, guest-turn-free mutations supported against a host Editor
/// session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorLocalEdit {
    InsertText(String),
    Backspace,
    Undo,
    Redo,
    Paste,
    MoveCursor(Movement),
    ExtendSelection(Movement),
}

/// The guest-facing view of a session: whole-buffer text only. Cursor and
/// selection are host-local UI state and never cross the `youth:editor`
/// capability boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EditorSessionSnapshot {
    pub(super) document_revision: u64,
    pub(super) edit_sequence: u64,
    pub(super) text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EditorSessionError {
    UnknownEditor,
    StaleDocumentRevision,
    StaleEditSequence,
}

#[derive(Clone)]
pub(super) struct EditorSessionSlot {
    generation: u64,
    session: Option<EditorSession>,
}

struct EditorSession {
    /// The guest declaration from which this generation was first created.
    /// Later declarations never overwrite host-owned session state.
    document_revision: u64,
    /// Host-owned ordering of live buffer changes. Pure cursor/selection
    /// movement does not advance this -- only operations that mutate
    /// content do, since this is what `accept`/`replace`'s staleness check
    /// (and the derived `locally_dirty` fact) cares about.
    edit_sequence: u64,
    /// The latest host edit sequence acknowledged by a successful guest
    /// `accept`. Dirty state is derived rather than separately stored.
    accepted_edit_sequence: u64,
    /// The real Unicode-aware buffer, cursor, selection, and layout.
    engine: ParleyEditorEngine,
    /// Most recent revision declared for this still-live node. A mismatch is
    /// remembered without replacing the session's creation payload.
    last_declared_document_revision: u64,
    undo_stack: VecDeque<UndoSnapshot>,
    redo_stack: VecDeque<UndoSnapshot>,
    /// True only while the next `InsertText` may extend the newest
    /// insertion group. Every other operation closes the group.
    insert_group_open: bool,
}

impl Clone for EditorSession {
    fn clone(&self) -> Self {
        Self {
            document_revision: self.document_revision,
            edit_sequence: self.edit_sequence,
            accepted_edit_sequence: self.accepted_edit_sequence,
            engine: self.engine.clone(),
            last_declared_document_revision: self.last_declared_document_revision,
            undo_stack: self.undo_stack.clone(),
            redo_stack: self.redo_stack.clone(),
            insert_group_open: self.insert_group_open,
        }
    }
}

/// A full (text, cursor, selection) snapshot taken immediately before an
/// undo group's edits, so undo/redo can restore exact prior state rather
/// than reversing individual character deltas against a cursor-aware
/// buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
struct UndoSnapshot {
    text: String,
    cursor: usize,
    selection: Option<Range<usize>>,
}

/// Reconciles host Editor sessions against a candidate committed tree.
///
/// This function does not mutate `previous`, allowing callers to stage the
/// registry update and install it only after the enclosing tree transaction
/// succeeds. Generation 1 is the first live session for a node ID. Destroyed
/// sessions retain their slot and increment the generation on reintroduction.
pub(super) fn reconcile_editor_sessions(
    previous: &EditorSessionRegistry,
    tree: &Tree,
) -> EditorSessionRegistry {
    let mut reconciled = EditorSessionRegistry::new();
    for (&node_id, slot) in previous {
        reconciled.insert(
            node_id,
            EditorSessionSlot {
                generation: slot.generation,
                session: None,
            },
        );
    }

    for node in tree.to_snapshot().nodes {
        let NodeData::Editor {
            document_revision,
            text,
        } = node.data
        else {
            continue;
        };

        match previous.get(&node.id) {
            Some(previous_slot) if previous_slot.session.is_some() => {
                // Stable node identity owns the live session. In particular,
                // an out-of-band declaration revision change must not clobber
                // host-owned state; replacement is only authorized through
                // the youth:editor capability boundary.
                let mut session = previous_slot
                    .session
                    .clone()
                    .expect("live Editor slot has a session");
                session.last_declared_document_revision = document_revision;
                reconciled
                    .get_mut(&node.id)
                    .expect("slot copied from previous registry")
                    .session = Some(session);
            }
            Some(previous_slot) => {
                let generation = previous_slot
                    .generation
                    .checked_add(1)
                    .expect("Editor session generation exhausted");
                reconciled.insert(
                    node.id,
                    EditorSessionSlot {
                        generation,
                        session: Some(new_session(document_revision, &text)),
                    },
                );
            }
            None => {
                reconciled.insert(
                    node.id,
                    EditorSessionSlot {
                        generation: 1,
                        session: Some(new_session(document_revision, &text)),
                    },
                );
            }
        }
    }

    reconciled
}

fn new_session(document_revision: u64, text: &str) -> EditorSession {
    let mut engine = ParleyEditorEngine::with_text(text);
    // `with_text` collapses to the start (the correct behavior for an
    // authoritative `replace`, where the prior cursor position is
    // meaningless). A freshly mounted session instead starts the cursor at
    // the end of the guest's initial content, matching ordinary "continue
    // where the document left off" editor behavior.
    engine.move_to_byte(text.len());
    EditorSession {
        document_revision,
        edit_sequence: EDIT_SEQUENCE_BASE,
        accepted_edit_sequence: EDIT_SEQUENCE_BASE,
        engine,
        last_declared_document_revision: document_revision,
        undo_stack: VecDeque::new(),
        redo_stack: VecDeque::new(),
        insert_group_open: false,
    }
}

pub(super) fn snapshot_editor_session(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorSessionSnapshot, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let text = session.engine.snapshot().text;
    Ok(EditorSessionSnapshot {
        document_revision: session.document_revision,
        edit_sequence: session.edit_sequence,
        text,
    })
}

/// Inserts `text` at the cursor (replacing the selection, if any) without
/// entering the guest. Consecutive inserts merge into one undo group.
pub(super) fn local_insert_text(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    text: &str,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.redo_stack.clear();
    if !session.insert_group_open {
        let before = snapshot_of(&mut session.engine);
        push_bounded(&mut session.undo_stack, before);
        session.insert_group_open = true;
    }
    session.engine.insert(text);
    advance_edit_sequence(session);
    Ok(local_edit_result(session))
}

/// Deletes one Unicode-safe unit before the cursor (or the selection, if
/// any) without entering the guest. Always its own undo group.
pub(super) fn local_backspace(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    session.redo_stack.clear();
    let before = snapshot_of(&mut session.engine);
    push_bounded(&mut session.undo_stack, before);
    session.engine.backspace();
    advance_edit_sequence(session);
    Ok(local_edit_result(session))
}

/// Inserts clipboard text as an isolated undo group. Empty clipboard text is
/// a safe no-op (no history entry, no sequence advance), but still closes
/// surrounding typing groups so later typing cannot merge across a paste.
pub(super) fn local_paste_text(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    text: &str,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    if text.is_empty() {
        return Ok(local_edit_result(session));
    }
    session.redo_stack.clear();
    let before = snapshot_of(&mut session.engine);
    push_bounded(&mut session.undo_stack, before);
    session.engine.insert(text);
    advance_edit_sequence(session);
    Ok(local_edit_result(session))
}

pub(super) fn local_undo(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    let Some(before) = session.undo_stack.pop_back() else {
        return Ok(local_edit_result(session));
    };
    let current = snapshot_of(&mut session.engine);
    push_bounded(&mut session.redo_stack, current);
    restore(&mut session.engine, &before);
    advance_edit_sequence(session);
    Ok(local_edit_result(session))
}

pub(super) fn local_redo(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    let Some(after) = session.redo_stack.pop_back() else {
        return Ok(local_edit_result(session));
    };
    let current = snapshot_of(&mut session.engine);
    push_bounded(&mut session.undo_stack, current);
    restore(&mut session.engine, &after);
    advance_edit_sequence(session);
    Ok(local_edit_result(session))
}

/// Moves the cursor (collapsing any selection). Not itself undoable, but
/// closes any open insertion group -- typing before and after a cursor
/// move produces two separate undo groups. Does not advance `edit_sequence`
/// since no content changes.
pub(super) fn local_move_cursor(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    movement: Movement,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    session.engine.move_cursor(movement);
    Ok(local_edit_result(session))
}

/// Extends the selection focus. Same group-boundary and non-content-mutating
/// behavior as [`local_move_cursor`].
pub(super) fn local_extend_selection(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    movement: Movement,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    session.engine.extend_selection(movement);
    Ok(local_edit_result(session))
}

pub(super) fn editor_locally_dirty(
    registry: &EditorSessionRegistry,
    editor: NodeId,
) -> Option<bool> {
    registry
        .get(&editor)
        .and_then(|slot| slot.session.as_ref())
        .map(|session| session.edit_sequence != session.accepted_edit_sequence)
}

pub(super) fn accept_editor_session(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    expected_document_revision: u64,
    expected_edit_sequence: u64,
    new_document_revision: u64,
) -> Result<(), EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    validate_expected_session(session, expected_document_revision, expected_edit_sequence)?;
    session.document_revision = new_document_revision;
    session.accepted_edit_sequence = session.edit_sequence;
    Ok(())
}

/// Installs authoritative guest content. Per the frozen replace reset
/// policy: cursor collapses to a defined valid position (buffer start, via
/// the engine's own `set_text`), selection clears, edit sequence resets to
/// base, and undo/redo history clears entirely (there is nothing coherent
/// to undo back into once the buffer's identity has been replaced).
pub(super) fn replace_editor_session(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    expected_document_revision: u64,
    expected_edit_sequence: u64,
    new_document_revision: u64,
    authoritative_text: String,
) -> Result<(), EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    validate_expected_session(session, expected_document_revision, expected_edit_sequence)?;
    session.document_revision = new_document_revision;
    session.edit_sequence = EDIT_SEQUENCE_BASE;
    session.accepted_edit_sequence = EDIT_SEQUENCE_BASE;
    session.engine.set_text(&authoritative_text);
    session.undo_stack.clear();
    session.redo_stack.clear();
    session.insert_group_open = false;
    Ok(())
}

fn validate_expected_session(
    session: &EditorSession,
    expected_document_revision: u64,
    expected_edit_sequence: u64,
) -> Result<(), EditorSessionError> {
    if session.document_revision != expected_document_revision {
        return Err(EditorSessionError::StaleDocumentRevision);
    }
    if session.edit_sequence != expected_edit_sequence {
        return Err(EditorSessionError::StaleEditSequence);
    }
    Ok(())
}

fn live_session_mut(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<&mut EditorSession, EditorSessionError> {
    registry
        .get_mut(&editor)
        .and_then(|slot| slot.session.as_mut())
        .ok_or(EditorSessionError::UnknownEditor)
}

fn advance_edit_sequence(session: &mut EditorSession) {
    session.edit_sequence = session
        .edit_sequence
        .checked_add(1)
        .expect("Editor edit sequence exhausted");
}

fn push_bounded(stack: &mut VecDeque<UndoSnapshot>, snapshot: UndoSnapshot) {
    if stack.len() == UNDO_GROUP_LIMIT {
        stack.pop_front();
    }
    stack.push_back(snapshot);
}

fn snapshot_of(engine: &mut ParleyEditorEngine) -> UndoSnapshot {
    let snapshot = engine.snapshot();
    UndoSnapshot {
        text: snapshot.text,
        cursor: snapshot.cursor,
        selection: snapshot.selection,
    }
}

fn restore(engine: &mut ParleyEditorEngine, snapshot: &UndoSnapshot) {
    engine.set_text(&snapshot.text);
    match &snapshot.selection {
        Some(range) => engine.select_byte_range(range.start, range.end),
        None => engine.move_to_byte(snapshot.cursor),
    }
}

fn local_edit_result(session: &mut EditorSession) -> EditorLocalEditResult {
    let snapshot = session.engine.snapshot();
    EditorLocalEditResult {
        document_revision: session.document_revision,
        edit_sequence: session.edit_sequence,
        text: snapshot.text,
        cursor: snapshot.cursor,
        selection: snapshot.selection,
    }
}

impl EditorSessionSlot {
    #[cfg(test)]
    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(super) fn session(&mut self) -> Option<(u64, u64, String)> {
        self.session.as_mut().map(|session| {
            let text = session.engine.snapshot().text;
            (session.document_revision, session.edit_sequence, text)
        })
    }

    #[cfg(test)]
    pub(super) fn last_declared_document_revision(&self) -> Option<u64> {
        self.session
            .as_ref()
            .map(|session| session.last_declared_document_revision)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use youth_tree::NodeId;

    use super::EDIT_SEQUENCE_BASE;
    use crate::{EditorLocalEdit, RuntimeErrorCategory, YouthApp};

    fn fixture() -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wasm32-wasip2/release/youth_editor_v006.wasm");
        assert!(
            path.exists(),
            "Editor component not found at {}; build it first with `cargo build -p youth-editor-v006 --target wasm32-wasip2 --release`",
            path.display()
        );
        path
    }

    fn capability_fixture() -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wasm32-wasip2/release/youth_editor_capability_v006.wasm");
        assert!(
            path.exists(),
            "Editor capability component not found at {}; build it first with `cargo build -p youth-editor-capability-v006 --target wasm32-wasip2 --release`",
            path.display()
        );
        path
    }

    fn id(value: u64) -> NodeId {
        NodeId::new(value).unwrap()
    }

    #[test]
    fn first_mount_creates_generation_one_session() {
        let mut app = YouthApp::load(fixture()).expect("Editor fixture loads");
        app.mount().expect("Editor fixture mounts");

        assert_eq!(
            app.editor_session_test_state(),
            vec![(
                id(2),
                1,
                42,
                EDIT_SEQUENCE_BASE,
                "Scratchpad draft".to_owned()
            )]
        );
    }

    #[test]
    fn local_insert_and_backspace_mutate_only_the_live_session() {
        let mut app = YouthApp::load(fixture()).expect("Editor fixture loads");
        app.mount().expect("Editor fixture mounts");

        let inserted = app
            .edit_editor_locally(id(2), EditorLocalEdit::InsertText("!".into()))
            .expect("local insert succeeds");
        assert_eq!(inserted.edit_sequence, 1);
        assert_eq!(inserted.text, "Scratchpad draft!");

        let deleted = app
            .edit_editor_locally(id(2), EditorLocalEdit::Backspace)
            .expect("local backspace succeeds");
        assert_eq!(deleted.edit_sequence, 2);
        assert_eq!(deleted.text, "Scratchpad draft");
        assert_eq!(app.inspect().guest_call_count, 1, "only mount called guest");
    }

    #[test]
    fn unchanged_resync_preserves_the_same_session() {
        let mut app = YouthApp::load(fixture()).expect("Editor fixture loads");
        app.mount().expect("Editor fixture mounts");
        let mounted = app.editor_session_test_state();

        app.resync().expect("unchanged Editor tree resyncs");

        assert_eq!(app.editor_session_test_state(), mounted);
    }

    #[test]
    fn removing_editor_on_resync_destroys_its_live_session() {
        let mut app = YouthApp::load(fixture()).expect("Editor fixture loads");
        app.mount().expect("Editor fixture mounts");
        app.resync().expect("unchanged Editor tree resyncs");

        app.resync().expect("tree without the Editor resyncs");

        assert!(
            app.editor_session_test_state().is_empty(),
            "removing the Editor must destroy its live session"
        );
        assert_eq!(app.editor_session_test_generation(id(2)), Some(1));
    }

    #[test]
    fn reintroduced_node_id_receives_a_new_generation() {
        let mut app = YouthApp::load(fixture()).expect("Editor fixture loads");
        app.mount().expect("Editor fixture mounts");
        app.resync().expect("unchanged Editor tree resyncs");
        app.resync().expect("tree without the Editor resyncs");

        app.resync().expect("Editors can be reintroduced");

        assert_eq!(
            app.editor_session_test_generation(id(2)),
            Some(2),
            "the remembered generation must advance instead of restarting"
        );
    }

    #[test]
    fn concurrent_editors_have_independent_sessions_and_generations() {
        let mut app = YouthApp::load(fixture()).expect("Editor fixture loads");
        app.mount().expect("Editor fixture mounts");
        app.resync().expect("unchanged Editor tree resyncs");
        app.resync().expect("tree without the Editor resyncs");
        app.resync().expect("Editors can be reintroduced");

        assert_eq!(
            app.editor_session_test_state(),
            vec![
                (
                    id(2),
                    2,
                    42,
                    EDIT_SEQUENCE_BASE,
                    "Scratchpad draft".to_owned()
                ),
                (
                    id(3),
                    1,
                    7,
                    EDIT_SEQUENCE_BASE,
                    "Independent document".to_owned()
                ),
            ],
            "reintroduced and independent Editor nodes need distinct session histories"
        );
    }

    #[test]
    fn out_of_band_document_revision_change_does_not_clobber_the_live_session() {
        let mut app = YouthApp::load(fixture()).expect("Editor fixture loads");
        app.mount().expect("Editor fixture mounts");
        app.resync().expect("unchanged Editor tree resyncs");
        app.resync().expect("tree without the Editor resyncs");
        app.resync().expect("Editors can be reintroduced");
        let before_conflict = app.editor_session_test_state();

        app.resync()
            .expect("out-of-band document revision does not panic or fault");

        assert_eq!(app.editor_session_test_state(), before_conflict);
        assert_eq!(
            app.editor_session_test_last_declared_revision(id(2)),
            Some(99),
            "the conflicting declaration is tracked without replacing the session"
        );
    }

    #[test]
    fn accept_then_trap_discards_the_staged_session_mutation() {
        let mut app = YouthApp::load(capability_fixture()).expect("capability fixture loads");
        app.mount().expect("capability fixture mounts");
        let before = app.editor_session_test_state();

        let error = app
            .activate(id(12))
            .expect_err("fixture traps after staging accept");

        assert_eq!(error.category(), RuntimeErrorCategory::GuestTrap);
        assert_eq!(
            app.editor_session_test_state(),
            before,
            "trapped turns must not commit revision, sequence, or text"
        );
    }
}
