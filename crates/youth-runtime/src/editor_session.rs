//! Host-owned Editor session bookkeeping.
//!
//! The guest declares an Editor node's initial document, but the host owns the
//! corresponding live session for as long as that stable node remains in the
//! installed tree. A slot remains after its session is destroyed so that a
//! later Editor with the same node ID receives a distinct generation.

use std::collections::{HashMap, VecDeque};

use youth_tree::{NodeData, NodeId, Tree};

pub(super) type EditorSessionRegistry = HashMap<NodeId, EditorSessionSlot>;

/// Every newly created process-local session starts at this edit sequence.
pub(super) const EDIT_SEQUENCE_BASE: u64 = 0;

/// Maximum reversible edit groups retained per live Editor session.
///
/// 512 groups keeps ordinary scratchpad undo useful while placing a fixed
/// bound on the number of retained edit deltas. The oldest group is discarded
/// when the limit is reached.
const UNDO_GROUP_LIMIT: usize = 512;

/// The host-owned Editor state after one accepted local edit operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorLocalEditResult {
    pub document_revision: u64,
    pub edit_sequence: u64,
    pub text: String,
}

/// The cursor-free local mutations supported by Scratchpad Gate A4.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorLocalEdit {
    InsertText(String),
    Backspace,
    Undo,
    Redo,
    Paste,
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EditorSessionSlot {
    generation: u64,
    session: Option<EditorSession>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditorSession {
    /// The guest declaration from which this generation was first created.
    /// Later declarations never overwrite host-owned session state.
    document_revision: u64,
    /// Host-owned ordering of live buffer changes.
    edit_sequence: u64,
    /// The latest host edit sequence acknowledged by a successful guest
    /// `accept`. Dirty state is derived rather than separately stored.
    accepted_edit_sequence: u64,
    text: String,
    /// Most recent revision declared for this still-live node. A mismatch is
    /// remembered without replacing the session's creation payload.
    last_declared_document_revision: u64,
    undo_stack: VecDeque<UndoGroup>,
    redo_stack: VecDeque<UndoGroup>,
    /// True only while the next `InsertText` may extend the newest insertion
    /// group. Every other operation closes the group.
    insert_group_open: bool,
}

/// A reversible end-of-buffer delta. Whole buffers are deliberately not
/// copied into history.
#[derive(Clone, Debug, Eq, PartialEq)]
enum UndoGroup {
    InsertText(String),
    Backspace(char),
    Paste(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertKind {
    Typing,
    Paste,
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
    let mut reconciled = previous.clone();

    for slot in reconciled.values_mut() {
        slot.session = None;
    }

    for node in tree.to_snapshot().nodes {
        let NodeData::Editor {
            document_revision,
            text,
        } = node.data
        else {
            continue;
        };

        match reconciled.get_mut(&node.id) {
            Some(slot) if previous[&node.id].session.is_some() => {
                // Stable node identity owns the live session. In particular,
                // an out-of-band declaration revision change must not clobber
                // host-owned state; replacement is only authorized through
                // the youth:editor capability boundary.
                let mut session = previous[&node.id]
                    .session
                    .clone()
                    .expect("live Editor slot has a session");
                session.last_declared_document_revision = document_revision;
                slot.session = Some(session);
            }
            Some(slot) => {
                slot.generation = slot
                    .generation
                    .checked_add(1)
                    .expect("Editor session generation exhausted");
                slot.session = Some(EditorSession {
                    document_revision,
                    edit_sequence: EDIT_SEQUENCE_BASE,
                    accepted_edit_sequence: EDIT_SEQUENCE_BASE,
                    text,
                    last_declared_document_revision: document_revision,
                    undo_stack: VecDeque::new(),
                    redo_stack: VecDeque::new(),
                    insert_group_open: false,
                });
            }
            None => {
                reconciled.insert(
                    node.id,
                    EditorSessionSlot {
                        generation: 1,
                        session: Some(EditorSession {
                            document_revision,
                            edit_sequence: EDIT_SEQUENCE_BASE,
                            accepted_edit_sequence: EDIT_SEQUENCE_BASE,
                            text,
                            last_declared_document_revision: document_revision,
                            undo_stack: VecDeque::new(),
                            redo_stack: VecDeque::new(),
                            insert_group_open: false,
                        }),
                    },
                );
            }
        }
    }

    reconciled
}

pub(super) fn snapshot_editor_session(
    registry: &EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorSessionSnapshot, EditorSessionError> {
    let session = registry
        .get(&editor)
        .and_then(|slot| slot.session.as_ref())
        .ok_or(EditorSessionError::UnknownEditor)?;
    Ok(EditorSessionSnapshot {
        document_revision: session.document_revision,
        edit_sequence: session.edit_sequence,
        text: session.text.clone(),
    })
}

/// Appends text to a live host-owned session without entering the guest.
///
/// Each accepted operation advances `edit_sequence` exactly once, including
/// insertion of an empty string.
pub(super) fn local_insert_text(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    text: &str,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    local_insert_text_with_kind(registry, editor, text, InsertKind::Typing)
}

/// Removes the final Unicode scalar from a live host-owned session without
/// entering the guest.
///
/// A backspace against an empty buffer is accepted and still advances
/// `edit_sequence`; cursor-aware deletion belongs to A5.
pub(super) fn local_backspace(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    session.redo_stack.clear();
    if let Some(removed) = session.text.pop() {
        push_bounded(&mut session.undo_stack, UndoGroup::Backspace(removed));
    }
    advance_edit_sequence(session);
    Ok(local_edit_result(session))
}

/// Inserts clipboard text as an isolated undo group. Empty clipboard text is
/// a safe no-op, but still closes surrounding typing groups so later typing
/// cannot merge across a paste command.
pub(super) fn local_paste_text(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    text: &str,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    local_insert_text_with_kind(registry, editor, text, InsertKind::Paste)
}

fn local_insert_text_with_kind(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    text: &str,
    kind: InsertKind,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    match kind {
        InsertKind::Typing => {
            session.redo_stack.clear();
            if !text.is_empty() {
                if session.insert_group_open {
                    let Some(UndoGroup::InsertText(group_text)) = session.undo_stack.back_mut()
                    else {
                        unreachable!("an open insertion group must be the newest undo group");
                    };
                    group_text.push_str(text);
                } else {
                    push_bounded(
                        &mut session.undo_stack,
                        UndoGroup::InsertText(text.to_owned()),
                    );
                    session.insert_group_open = true;
                }
            }
        }
        InsertKind::Paste => {
            session.insert_group_open = false;
            if text.is_empty() {
                return Ok(local_edit_result(session));
            }
            session.redo_stack.clear();
            push_bounded(&mut session.undo_stack, UndoGroup::Paste(text.to_owned()));
        }
    }
    session.text.push_str(text);
    advance_edit_sequence(session);
    Ok(local_edit_result(session))
}

pub(super) fn local_undo(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    let Some(group) = session.undo_stack.pop_back() else {
        return Ok(local_edit_result(session));
    };
    apply_undo(session, &group);
    push_bounded(&mut session.redo_stack, group);
    advance_edit_sequence(session);
    Ok(local_edit_result(session))
}

pub(super) fn local_redo(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    let Some(group) = session.redo_stack.pop_back() else {
        return Ok(local_edit_result(session));
    };
    apply_redo(session, &group);
    push_bounded(&mut session.undo_stack, group);
    advance_edit_sequence(session);
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
    let session = registry
        .get_mut(&editor)
        .and_then(|slot| slot.session.as_mut())
        .ok_or(EditorSessionError::UnknownEditor)?;
    validate_expected_session(session, expected_document_revision, expected_edit_sequence)?;
    session.document_revision = new_document_revision;
    session.accepted_edit_sequence = session.edit_sequence;
    Ok(())
}

pub(super) fn replace_editor_session(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    expected_document_revision: u64,
    expected_edit_sequence: u64,
    new_document_revision: u64,
    authoritative_text: String,
) -> Result<(), EditorSessionError> {
    let session = registry
        .get_mut(&editor)
        .and_then(|slot| slot.session.as_mut())
        .ok_or(EditorSessionError::UnknownEditor)?;
    validate_expected_session(session, expected_document_revision, expected_edit_sequence)?;
    session.document_revision = new_document_revision;
    session.edit_sequence = EDIT_SEQUENCE_BASE;
    session.accepted_edit_sequence = EDIT_SEQUENCE_BASE;
    session.text = authoritative_text;
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

fn push_bounded(stack: &mut VecDeque<UndoGroup>, group: UndoGroup) {
    if stack.len() == UNDO_GROUP_LIMIT {
        stack.pop_front();
    }
    stack.push_back(group);
}

fn apply_undo(session: &mut EditorSession, group: &UndoGroup) {
    match group {
        UndoGroup::InsertText(text) | UndoGroup::Paste(text) => {
            debug_assert!(session.text.ends_with(text));
            session.text.truncate(session.text.len() - text.len());
        }
        UndoGroup::Backspace(removed) => session.text.push(*removed),
    }
}

fn apply_redo(session: &mut EditorSession, group: &UndoGroup) {
    match group {
        UndoGroup::InsertText(text) | UndoGroup::Paste(text) => session.text.push_str(text),
        UndoGroup::Backspace(removed) => {
            let redone = session.text.pop();
            debug_assert_eq!(redone, Some(*removed));
        }
    }
}

fn local_edit_result(session: &EditorSession) -> EditorLocalEditResult {
    EditorLocalEditResult {
        document_revision: session.document_revision,
        edit_sequence: session.edit_sequence,
        text: session.text.clone(),
    }
}

impl EditorSessionSlot {
    #[cfg(test)]
    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(super) fn session(&self) -> Option<(u64, u64, &str)> {
        self.session.as_ref().map(|session| {
            (
                session.document_revision,
                session.edit_sequence,
                session.text.as_str(),
            )
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
