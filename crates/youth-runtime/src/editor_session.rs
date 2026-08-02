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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;

use youth_editor_engine::{
    EditorEngine, EditorLayout, Movement, ParleyEditorEngine, TextPresentation,
};
use youth_tree::{NodeData, NodeId, Tree};

pub(super) type EditorSessionRegistry = HashMap<NodeId, EditorSessionSlot>;

/// Allocates the AccessKit IDs used by Parley's text-run children.
///
/// Youth semantic IDs are arbitrary nonzero `u64`s, so arithmetic namespace
/// tricks cannot make editor descendants disjoint. This allocator reserves
/// every currently installed semantic ID and then monotonically allocates
/// unused IDs for editor-owned objects. IDs are never reused for the lifetime
/// of the worker's accessibility generation.
#[derive(Default)]
pub(super) struct AccessibilityIdRegistry {
    next: u64,
    allocated: HashSet<u64>,
    forward: HashMap<AccessibilityKey, u64>,
    reverse: HashMap<u64, AccessibilityKey>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum AccessibilityKey {
    Semantic(NodeId),
    EditorTextRun { editor: NodeId, run: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AccessibilityIdError {
    SemanticCollision(u64),
    Exhausted,
}

impl AccessibilityIdRegistry {
    pub(super) fn reserve_semantic_ids(
        &mut self,
        ids: impl IntoIterator<Item = NodeId>,
    ) -> Result<(), AccessibilityIdError> {
        for id in ids {
            let value = id.get();
            let key = AccessibilityKey::Semantic(id);
            if let Some(existing) = self.reverse.get(&value)
                && *existing != key
            {
                return Err(AccessibilityIdError::SemanticCollision(value));
            }
            self.allocated.insert(value);
            self.forward.insert(key, value);
            self.reverse.insert(value, key);
        }
        Ok(())
    }

    fn allocate(
        &mut self,
        key: AccessibilityKey,
    ) -> Result<accesskit::NodeId, AccessibilityIdError> {
        if let Some(&value) = self.forward.get(&key) {
            return Ok(accesskit::NodeId(value));
        }
        loop {
            let candidate = self
                .next
                .checked_add(1)
                .ok_or(AccessibilityIdError::Exhausted)?;
            self.next = candidate;
            if candidate == 0 || !self.allocated.insert(candidate) {
                continue;
            }
            self.forward.insert(key, candidate);
            self.reverse.insert(candidate, key);
            return Ok(accesskit::NodeId(candidate));
        }
    }
}

/// Every newly created process-local session starts at this edit sequence.
pub(super) const EDIT_SEQUENCE_BASE: u64 = 0;

/// Maximum reversible edit groups retained per live Editor session.
///
/// 512 groups keeps ordinary scratchpad undo useful while placing a fixed
/// bound on retained delta history. The oldest group is discarded when the
/// limit is reached.
const UNDO_GROUP_LIMIT: usize = 512;
/// Aggregate UTF-8 budget for one undo or redo stack. The count limit alone
/// would permit hundreds of large replacement deltas to accumulate.
const UNDO_BYTES_LIMIT: usize = 64 * 1024 * 1024;

/// Metadata returned by one accepted host-local Editor operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorLocalEditResult {
    pub document_revision: u64,
    pub edit_sequence: u64,
    /// Whether accepted buffer content changed. Only this field advances the
    /// document sequence, undo history, and dirty-since-accept state.
    pub content_changed: bool,
    /// Whether cursor or selection state changed.
    pub interaction_changed: bool,
    /// Whether the renderer should refresh this editor's presentation.
    pub presentation_changed: bool,
    /// The UTF-8 byte range affected in the pre-edit document, when content
    /// changed. Consumers that only need metadata can avoid materializing the
    /// complete buffer.
    pub changed_range: Option<Range<usize>>,
    pub cursor: usize,
    pub selection: Option<Range<usize>>,
    pub locally_dirty: bool,
    pub dirty_changed: Option<bool>,
}

/// The local, guest-turn-free mutations supported against a host Editor
/// session.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorLocalEdit {
    /// Updates the host-local logical wrapping width for an Editor. This is
    /// presentation policy only and never enters the guest.
    SetViewportWidth {
        width: f32,
    },
    InsertText(String),
    Backspace,
    Undo,
    Redo,
    Paste,
    MoveCursor(Movement),
    ExtendSelection(Movement),
    /// Sets or replaces the IME preedit text. `cursor` is a byte range
    /// relative to `text`'s start, matching winit's `Ime::Preedit`.
    ImeSetCompose {
        text: String,
        cursor: Option<(usize, usize)>,
    },
    /// Cancels IME composition, discarding the preedit text.
    ImeClearCompose,
    /// Commits the current IME preedit as ordinary buffer content.
    ImeFinishCompose,
    /// Collapses the cursor to the text position nearest `(x, y)` in the
    /// engine's own content coordinate space (a pointer click).
    MoveToPoint {
        x: f32,
        y: f32,
    },
    /// Selects from `(anchor_x, anchor_y)` to `(x, y)`, both in the
    /// engine's own content coordinate space (a pointer drag).
    ExtendSelectionToPoint {
        anchor_x: f32,
        anchor_y: f32,
        x: f32,
        y: f32,
    },
    /// Applies a selection reported by an AccessKit `SetTextSelection`
    /// action, e.g. from a screen reader. `ReplaceSelectedText` (also an
    /// AccessKit action) needs no separate variant -- it is exactly
    /// `InsertText`, which already replaces any active selection.
    SetSelectionFromAccessKit(accesskit::TextSelection),
}

/// An explicit, owned editor snapshot. Local edits return metadata only;
/// callers use this request when they deliberately need the full buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorSnapshot {
    pub document_revision: u64,
    pub edit_sequence: u64,
    pub text: String,
    pub locally_dirty: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EditorSessionError {
    UnknownEditor,
    StaleDocumentRevision,
    StaleEditSequence,
    BufferTooLarge,
    PreeditTooLarge,
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
    text_document: Option<crate::DocumentHandle>,
    /// Host-owned ordering of live buffer changes. Pure cursor/selection
    /// movement does not advance this -- only operations that mutate
    /// content do, since this is what `accept`/`replace`'s staleness check
    /// (and the derived `locally_dirty` fact) cares about.
    edit_sequence: u64,
    /// Monotonic text generation for host-owned consumers. It advances only
    /// when accepted buffer content changes.
    text_generation: u64,
    /// Monotonic layout generation. Content and viewport changes invalidate
    /// layout; interaction-only changes do not.
    layout_generation: u64,
    /// Monotonic generation for the raster presentation cache.
    presentation_generation: u64,
    /// Monotonic generation for the accessibility subtree cache.
    accessibility_generation: u64,
    /// The latest host edit sequence acknowledged by a successful guest
    /// `accept`. Dirty state is derived rather than separately stored.
    accepted_edit_sequence: u64,
    /// The real Unicode-aware buffer, cursor, selection, and layout.
    engine: ParleyEditorEngine,
    /// Most recent revision declared for this still-live node. A mismatch is
    /// remembered without replacing the session's creation payload.
    last_declared_document_revision: u64,
    undo_stack: VecDeque<UndoEdit>,
    redo_stack: VecDeque<UndoEdit>,
    /// True only while the next `InsertText` may extend the newest
    /// insertion group. Every other operation closes the group.
    insert_group_open: bool,
    /// The pre-composition snapshot, captured on the first
    /// `ImeSetCompose` of a composition session. Composition is one undo
    /// group regardless of how many intermediate preedit updates occur;
    /// this is pushed onto `undo_stack` only on `ImeFinishCompose` (a
    /// cancelled composition via `ImeClearCompose` has nothing to undo).
    pending_compose_snapshot: Option<UndoSnapshot>,
}

impl Clone for EditorSession {
    fn clone(&self) -> Self {
        Self {
            document_revision: self.document_revision,
            text_document: self.text_document,
            edit_sequence: self.edit_sequence,
            text_generation: self.text_generation,
            layout_generation: self.layout_generation,
            presentation_generation: self.presentation_generation,
            accessibility_generation: self.accessibility_generation,
            accepted_edit_sequence: self.accepted_edit_sequence,
            engine: self.engine.clone(),
            last_declared_document_revision: self.last_declared_document_revision,
            undo_stack: self.undo_stack.clone(),
            redo_stack: self.redo_stack.clone(),
            insert_group_open: self.insert_group_open,
            pending_compose_snapshot: self.pending_compose_snapshot.clone(),
        }
    }
}

/// A reversible content change. History stores only the replaced bytes and
/// selection metadata, never a complete document copy.
#[derive(Clone, Debug, Eq, PartialEq)]
struct UndoEdit {
    range_before: Range<usize>,
    deleted: String,
    inserted: String,
    selection_before: Option<Range<usize>>,
    cursor_before: usize,
    selection_after: Option<Range<usize>>,
    cursor_after: usize,
}

/// A complete pre-composition state is retained only while an IME composition
/// is active. It is not placed in the undo/redo stacks unless the composition
/// commits, where it is immediately converted into an `UndoEdit`.
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
    text_document: Option<&crate::text_document::TextDocumentSession>,
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
        let (document_revision, text, binding) = match node.data {
            NodeData::Editor {
                document_revision,
                text,
            } => (document_revision, text, None),
            NodeData::TextDocumentEditor {
                document_id,
                document_generation,
                version_generation,
                ..
            } => {
                let handle = crate::DocumentHandle {
                    id: document_id,
                    generation: document_generation,
                };
                let Some((revision, text)) =
                    text_document.and_then(|document| document.editor_text(handle))
                else {
                    continue;
                };
                if revision != version_generation {
                    continue;
                }
                (revision, text.to_string(), Some(handle))
            }
            _ => continue,
        };

        match previous.get(&node.id) {
            Some(previous_slot)
                if previous_slot
                    .session
                    .as_ref()
                    .is_some_and(|session| session.text_document == binding) =>
            {
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
                        session: Some(new_session(document_revision, &text, binding)),
                    },
                );
            }
            None => {
                reconciled.insert(
                    node.id,
                    EditorSessionSlot {
                        generation: 1,
                        session: Some(new_session(document_revision, &text, binding)),
                    },
                );
            }
        }
    }

    reconciled
}

fn new_session(
    document_revision: u64,
    text: &str,
    text_document: Option<crate::DocumentHandle>,
) -> EditorSession {
    let mut engine = ParleyEditorEngine::with_text(text);
    // `with_text` collapses to the start (the correct behavior for an
    // authoritative `replace`, where the prior cursor position is
    // meaningless). A freshly mounted session instead starts the cursor at
    // the end of the guest's initial content, matching ordinary "continue
    // where the document left off" editor behavior.
    engine.move_to_byte(text.len());
    EditorSession {
        document_revision,
        text_document,
        edit_sequence: EDIT_SEQUENCE_BASE,
        text_generation: 0,
        layout_generation: 0,
        presentation_generation: 0,
        accessibility_generation: 0,
        accepted_edit_sequence: EDIT_SEQUENCE_BASE,
        engine,
        last_declared_document_revision: document_revision,
        undo_stack: VecDeque::new(),
        redo_stack: VecDeque::new(),
        insert_group_open: false,
        pending_compose_snapshot: None,
    }
}

/// Extracts a paintable presentation (glyph runs plus selection/cursor
/// geometry) for every live Editor session, for a desktop renderer to
/// consume synchronously without going through the guest or the tree at
/// all. Host-local only -- never exposed to the guest.
pub(super) fn editor_presentations(
    registry: &mut EditorSessionRegistry,
) -> HashMap<NodeId, TextPresentation> {
    registry
        .iter_mut()
        .filter_map(|(&node_id, slot)| {
            slot.session
                .as_mut()
                .map(|session| (node_id, session.engine.presentation()))
        })
        .collect()
}

pub(super) fn editor_presentation(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Option<TextPresentation> {
    registry
        .get_mut(&editor)
        .and_then(|slot| slot.session.as_mut())
        .map(|session| session.engine.presentation())
}

pub(super) fn editor_presentation_generation(
    registry: &EditorSessionRegistry,
    editor: NodeId,
) -> Option<u64> {
    registry.get(&editor).and_then(|slot| {
        slot.session.as_ref().map(|session| {
            slot.generation
                .wrapping_shl(32)
                .wrapping_add(session.presentation_generation)
        })
    })
}

pub(super) fn bump_editor_generations(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    content_changed: bool,
    interaction_changed: bool,
    presentation_changed: bool,
) {
    if let Some(session) = registry
        .get_mut(&editor)
        .and_then(|slot| slot.session.as_mut())
    {
        if content_changed {
            session.text_generation = session.text_generation.wrapping_add(1);
        }
        if content_changed || interaction_changed {
            session.accessibility_generation = session.accessibility_generation.wrapping_add(1);
        }
        if content_changed || presentation_changed {
            session.layout_generation = session.layout_generation.wrapping_add(1);
        }
        if presentation_changed {
            session.presentation_generation = session.presentation_generation.wrapping_add(1);
        }
    }
}

/// One Editor's accessibility contribution, computed once per worker cycle
/// (mirroring [`editor_presentations`]) since it requires mutable access to
/// the live engine that only the worker actor has.
///
/// Coordinates on `node` and `extra_nodes` are in the engine's own
/// rect/scroll-independent space -- exactly like `TextPresentation`'s glyph
/// positions -- so a desktop caller must apply its own window rect and
/// scroll translation before merging this into a real window-relative
/// accessibility tree.
#[derive(Clone, Debug)]
pub struct EditorAccessibility {
    /// This Editor's own node: role, the `Focus` and `SetTextSelection`
    /// actions, children (pointing at `extra_nodes`), and text selection
    /// are already populated. Bounds are the caller's responsibility, since
    /// only the caller knows the Editor's on-screen rect.
    pub node: accesskit::Node,
    /// Per-run `TextRun`-role child nodes Parley allocated while populating
    /// `node`.
    pub extra_nodes: Vec<(accesskit::NodeId, accesskit::Node)>,
}

/// Builds one [`EditorAccessibility`] snapshot per live Editor session.
///
/// Text-run IDs come from the installed accessibility generation's checked
/// allocator. They are not derived from semantic IDs or Parley run ordinals.
pub(super) fn editor_accessibility_snapshots(
    registry: &mut EditorSessionRegistry,
    ids: &mut AccessibilityIdRegistry,
) -> Result<HashMap<NodeId, EditorAccessibility>, AccessibilityIdError> {
    registry
        .iter_mut()
        .try_fold(HashMap::new(), |mut snapshots, (&node_id, slot)| {
            let Some(session) = slot.session.as_mut() else {
                return Ok(snapshots);
            };
            snapshots.insert(node_id, build_editor_accessibility(session, ids, node_id)?);
            Ok(snapshots)
        })
}

pub(super) fn editor_accessibility_snapshot(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    ids: &mut AccessibilityIdRegistry,
) -> Result<Option<EditorAccessibility>, AccessibilityIdError> {
    let Some(session) = registry
        .get_mut(&editor)
        .and_then(|slot| slot.session.as_mut())
    else {
        return Ok(None);
    };
    build_editor_accessibility(session, ids, editor).map(Some)
}

fn build_editor_accessibility(
    session: &mut EditorSession,
    ids: &mut AccessibilityIdRegistry,
    editor: NodeId,
) -> Result<EditorAccessibility, AccessibilityIdError> {
    let mut node = accesskit::Node::new(accesskit::Role::MultilineTextInput);
    node.add_action(accesskit::Action::Focus);
    let mut update = accesskit::TreeUpdate {
        nodes: Vec::new(),
        tree: None,
        tree_id: accesskit::TreeId::ROOT,
        focus: accesskit::NodeId(0),
    };
    let mut allocation_error = None;
    let mut run = 0_u32;
    session.engine.accessibility_update(
        &mut update,
        &mut node,
        || {
            let key = AccessibilityKey::EditorTextRun { editor, run };
            run = run.saturating_add(1);
            match ids.allocate(key) {
                Ok(id) => id,
                Err(error) => {
                    allocation_error = Some(error);
                    accesskit::NodeId(0)
                }
            }
        },
        0.0,
        0.0,
    );
    if let Some(error) = allocation_error {
        return Err(error);
    }
    Ok(EditorAccessibility {
        node,
        extra_nodes: update.nodes,
    })
}

pub(super) fn snapshot_editor_session(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorSnapshot, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let text = session.engine.state_snapshot().text;
    Ok(EditorSnapshot {
        document_revision: session.document_revision,
        edit_sequence: session.edit_sequence,
        text,
        locally_dirty: session.edit_sequence != session.accepted_edit_sequence,
    })
}

/// Inserts `text` at the cursor (replacing the selection, if any) without
/// entering the guest. Consecutive inserts merge into one undo group.
pub(super) fn local_insert_text(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    text: &str,
    max_editor_bytes: usize,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    if text.is_empty() {
        let (cursor, selection) = session.engine.selection_state();
        return Ok(local_edit_result_from_parts(
            session,
            false,
            None,
            cursor,
            selection.clone(),
            cursor,
            selection,
        ));
    }
    let (before_cursor, before_selection) = session.engine.selection_state();
    let range_before = before_selection
        .clone()
        .unwrap_or(before_cursor..before_cursor);
    let deleted = session.engine.raw_text()[range_before.clone()].to_owned();
    let resulting_bytes = session
        .engine
        .raw_text()
        .len()
        .saturating_sub(deleted.len())
        .saturating_add(text.len());
    if resulting_bytes > max_editor_bytes {
        return Err(EditorSessionError::BufferTooLarge);
    }
    session.engine.insert(text);
    let (after_cursor, after_selection) = session.engine.selection_state();
    let content_changed = deleted != text;
    if content_changed {
        session.redo_stack.clear();
        let edit = UndoEdit {
            range_before: range_before.clone(),
            deleted,
            inserted: text.to_owned(),
            selection_before: before_selection.clone(),
            cursor_before: before_cursor,
            selection_after: after_selection.clone(),
            cursor_after: after_cursor,
        };
        let merge = session.insert_group_open
            && edit.deleted.is_empty()
            && session.undo_stack.back().is_some_and(|last| {
                last.deleted.is_empty()
                    && last.range_before.start + last.inserted.len() == edit.range_before.start
            });
        if merge {
            let last = session.undo_stack.back_mut().expect("merge edit exists");
            last.inserted.push_str(&edit.inserted);
            last.selection_after = edit.selection_after;
            last.cursor_after = edit.cursor_after;
        } else {
            push_bounded(&mut session.undo_stack, edit);
        }
        session.insert_group_open = true;
        advance_edit_sequence(session);
    }
    Ok(local_edit_result_from_parts(
        session,
        content_changed,
        content_changed.then_some(range_before),
        before_cursor,
        before_selection,
        after_cursor,
        after_selection,
    ))
}

pub(super) fn local_set_viewport_width(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    width: f32,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let (before_cursor, before_selection) = session.engine.selection_state();
    session.engine.set_width(Some(width.max(0.0)));
    let (cursor, selection) = session.engine.selection_state();
    let mut result = local_edit_result_from_parts(
        session,
        false,
        None,
        before_cursor,
        before_selection,
        cursor,
        selection,
    );
    result.presentation_changed = true;
    Ok(result)
}

/// Deletes one Unicode-safe unit before the cursor (or the selection, if
/// any) without entering the guest. Always its own undo group.
pub(super) fn local_backspace(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    let (before_cursor, before_selection) = session.engine.selection_state();
    let range_before = before_selection.clone().unwrap_or_else(|| {
        previous_char_boundary(session.engine.raw_text(), before_cursor)..before_cursor
    });
    let deleted = session.engine.raw_text()[range_before.clone()].to_owned();
    session.engine.backspace();
    let (after_cursor, after_selection) = session.engine.selection_state();
    let content_changed = !deleted.is_empty();
    if content_changed {
        session.redo_stack.clear();
        push_bounded(
            &mut session.undo_stack,
            make_undo_edit(
                range_before.clone(),
                deleted,
                String::new(),
                before_cursor,
                before_selection.clone(),
                after_cursor,
                after_selection.clone(),
            ),
        );
        advance_edit_sequence(session);
    }
    Ok(local_edit_result_from_parts(
        session,
        content_changed,
        content_changed.then_some(range_before),
        before_cursor,
        before_selection,
        after_cursor,
        after_selection,
    ))
}

/// Inserts clipboard text as an isolated undo group. Empty clipboard text is
/// a safe no-op (no history entry, no sequence advance), but still closes
/// surrounding typing groups so later typing cannot merge across a paste.
pub(super) fn local_paste_text(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    text: &str,
    max_editor_bytes: usize,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    if text.is_empty() {
        let (cursor, selection) = session.engine.selection_state();
        return Ok(local_edit_result_from_parts(
            session,
            false,
            None,
            cursor,
            selection.clone(),
            cursor,
            selection,
        ));
    }
    let result = local_insert_text(registry, editor, text, max_editor_bytes)?;
    registry
        .get_mut(&editor)
        .and_then(|slot| slot.session.as_mut())
        .expect("editor session remains live")
        .insert_group_open = false;
    Ok(result)
}

pub(super) fn local_undo(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    let (before_cursor, before_selection) = session.engine.selection_state();
    let Some(edit) = session.undo_stack.pop_back() else {
        return Ok(local_edit_result_from_parts(
            session,
            false,
            None,
            before_cursor,
            before_selection.clone(),
            before_cursor,
            before_selection,
        ));
    };
    push_bounded(&mut session.redo_stack, edit.clone());
    apply_undo_edit(&mut session.engine, &edit, true);
    advance_edit_sequence(session);
    let (after_cursor, after_selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        true,
        Some(edit.range_before.clone()),
        before_cursor,
        before_selection,
        after_cursor,
        after_selection,
    ))
}

pub(super) fn local_redo(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    session.insert_group_open = false;
    let (before_cursor, before_selection) = session.engine.selection_state();
    let Some(edit) = session.redo_stack.pop_back() else {
        return Ok(local_edit_result_from_parts(
            session,
            false,
            None,
            before_cursor,
            before_selection.clone(),
            before_cursor,
            before_selection,
        ));
    };
    push_bounded(&mut session.undo_stack, edit.clone());
    apply_undo_edit(&mut session.engine, &edit, false);
    advance_edit_sequence(session);
    let (after_cursor, after_selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        true,
        Some(edit.range_before),
        before_cursor,
        before_selection,
        after_cursor,
        after_selection,
    ))
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
    let (before_cursor, before_selection) = session.engine.selection_state();
    session.insert_group_open = false;
    session.engine.move_cursor(movement);
    let (cursor, selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        false,
        None,
        before_cursor,
        before_selection,
        cursor,
        selection,
    ))
}

/// Extends the selection focus. Same group-boundary and non-content-mutating
/// behavior as [`local_move_cursor`].
pub(super) fn local_extend_selection(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    movement: Movement,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let (before_cursor, before_selection) = session.engine.selection_state();
    session.insert_group_open = false;
    session.engine.extend_selection(movement);
    let (cursor, selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        false,
        None,
        before_cursor,
        before_selection,
        cursor,
        selection,
    ))
}

/// Collapses the cursor to the text position nearest a pointer click. Same
/// group-boundary and non-content-mutating behavior as
/// [`local_move_cursor`].
pub(super) fn local_move_to_point(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    x: f32,
    y: f32,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let (before_cursor, before_selection) = session.engine.selection_state();
    session.insert_group_open = false;
    let offset = session.engine.hit_test(x, y);
    session.engine.move_to_byte(offset);
    let (cursor, selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        false,
        None,
        before_cursor,
        before_selection,
        cursor,
        selection,
    ))
}

/// Selects from `(anchor_x, anchor_y)` to `(x, y)`, the host-local effect
/// of a click-and-drag text selection. The anchor is re-hit-tested on every
/// call rather than cached as a byte offset, since the caller (native
/// pointer input) only knows the anchor as a content-space point, not a
/// byte position.
pub(super) fn local_extend_selection_to_point(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    anchor_x: f32,
    anchor_y: f32,
    x: f32,
    y: f32,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let (before_cursor, before_selection) = session.engine.selection_state();
    session.insert_group_open = false;
    let anchor = session.engine.hit_test(anchor_x, anchor_y);
    let focus = session.engine.hit_test(x, y);
    session.engine.select_byte_range(anchor, focus);
    let (cursor, selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        false,
        None,
        before_cursor,
        before_selection,
        cursor,
        selection,
    ))
}

/// Applies a selection reported by an AccessKit `SetTextSelection` action.
/// Same group-boundary and non-content-mutating behavior as
/// [`local_move_cursor`].
pub(super) fn local_set_selection_from_accesskit(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    selection: &accesskit::TextSelection,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let (before_cursor, before_selection) = session.engine.selection_state();
    session.insert_group_open = false;
    session.engine.select_from_accesskit(selection);
    let (cursor, selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        false,
        None,
        before_cursor,
        before_selection,
        cursor,
        selection,
    ))
}

/// Sets or replaces the in-progress IME preedit text. The first call of a
/// composition captures a pre-composition snapshot (so the whole composition
/// can later become one undo group); repeated calls while still composing
/// replace the preedit rather than accumulating snapshots. Never advances
/// `edit_sequence` -- preedit content is not yet accepted buffer content.
pub(super) fn local_ime_set_compose(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    text: &str,
    cursor: Option<(usize, usize)>,
    max_preedit_bytes: usize,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let (before_cursor, before_selection) = session.engine.selection_state();
    session.insert_group_open = false;
    // Mirrors `ParleyEditorEngine::ime_set_compose`'s own empty-text-clears
    // policy: an empty update never started a real composition, so it must
    // not capture (or leave dangling) a pending undo snapshot for
    // `ImeFinishCompose` to spuriously commit.
    if text.is_empty() {
        session.engine.ime_clear_compose();
        session.pending_compose_snapshot = None;
        let (cursor, selection) = session.engine.selection_state();
        return Ok(local_edit_result_from_parts(
            session,
            false,
            None,
            before_cursor,
            before_selection,
            cursor,
            selection,
        ));
    }
    if text.len() > max_preedit_bytes {
        return Err(EditorSessionError::PreeditTooLarge);
    }
    if !session.engine.is_composing() && session.pending_compose_snapshot.is_none() {
        let before = session.engine.state_snapshot();
        session.pending_compose_snapshot = Some(undo_snapshot(&before));
    }
    session.engine.ime_set_compose(text, cursor);
    let (cursor, selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        false,
        None,
        before_cursor,
        before_selection,
        cursor,
        selection,
    ))
}

/// Cancels IME composition, discarding the preedit text and the pending
/// undo snapshot -- a cancelled composition never touched accepted content,
/// so there is nothing to undo.
pub(super) fn local_ime_clear_compose(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let (before_cursor, before_selection) = session.engine.selection_state();
    session.insert_group_open = false;
    session.engine.ime_clear_compose();
    session.pending_compose_snapshot = None;
    let (cursor, selection) = session.engine.selection_state();
    Ok(local_edit_result_from_parts(
        session,
        false,
        None,
        before_cursor,
        before_selection,
        cursor,
        selection,
    ))
}

/// Commits the current IME preedit as ordinary buffer content. The whole
/// composition -- however many `ImeSetCompose` updates it took -- becomes
/// exactly one undo group, and this is the only IME operation that advances
/// `edit_sequence`.
pub(super) fn local_ime_finish_compose(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    max_editor_bytes: usize,
) -> Result<EditorLocalEditResult, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    let before_state = session.engine.state_snapshot();
    session.insert_group_open = false;
    if before_state.text.len() > max_editor_bytes {
        return Err(EditorSessionError::BufferTooLarge);
    }
    session.engine.ime_finish_compose();
    let after = session.engine.state_snapshot();
    if after.text.len() > max_editor_bytes {
        if let Some(compose_before) = session.pending_compose_snapshot.take() {
            restore(&mut session.engine, &compose_before);
        } else {
            restore(
                &mut session.engine,
                &UndoSnapshot {
                    text: before_state.text.clone(),
                    cursor: before_state.cursor,
                    selection: before_state.selection.clone(),
                },
            );
        }
        return Err(EditorSessionError::BufferTooLarge);
    }
    let content_changed = after.text != before_state.text;
    if let Some(undo_before) = session.pending_compose_snapshot.take()
        && content_changed
    {
        session.redo_stack.clear();
        let before_compose = youth_editor_engine::EngineSnapshot {
            text: undo_before.text,
            cursor: undo_before.cursor,
            selection: undo_before.selection,
        };
        push_bounded(&mut session.undo_stack, undo_edit(&before_compose, &after));
        advance_edit_sequence(session);
    }
    Ok(local_edit_result_with_snapshot(
        session,
        content_changed,
        Some(&before_state),
        after,
    ))
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

pub(super) fn accept_saved_text_document(
    registry: &mut EditorSessionRegistry,
    editor: NodeId,
    document: crate::DocumentHandle,
    saved_edit_sequence: u64,
    version: crate::DocumentVersion,
) -> Result<bool, EditorSessionError> {
    let session = live_session_mut(registry, editor)?;
    if session.text_document != Some(document) || saved_edit_sequence > session.edit_sequence {
        return Err(EditorSessionError::StaleEditSequence);
    }
    session.document_revision = version.generation;
    session.accepted_edit_sequence = saved_edit_sequence;
    Ok(session.edit_sequence != session.accepted_edit_sequence)
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
    max_editor_bytes: usize,
) -> Result<(), EditorSessionError> {
    if authoritative_text.len() > max_editor_bytes {
        return Err(EditorSessionError::BufferTooLarge);
    }
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

fn push_bounded(stack: &mut VecDeque<UndoEdit>, edit: UndoEdit) {
    stack.push_back(edit);
    while stack.len() > UNDO_GROUP_LIMIT
        || stack
            .iter()
            .map(|edit| edit.deleted.len().saturating_add(edit.inserted.len()))
            .sum::<usize>()
            > UNDO_BYTES_LIMIT
    {
        stack.pop_front();
    }
}

fn undo_snapshot(snapshot: &youth_editor_engine::EngineSnapshot) -> UndoSnapshot {
    UndoSnapshot {
        text: snapshot.text.clone(),
        cursor: snapshot.cursor,
        selection: snapshot.selection.clone(),
    }
}

fn undo_edit(
    before: &youth_editor_engine::EngineSnapshot,
    after: &youth_editor_engine::EngineSnapshot,
) -> UndoEdit {
    let range = changed_range(Some(before), after);
    let suffix = before.text.len().saturating_sub(range.end);
    make_undo_edit(
        range.clone(),
        before.text[range.clone()].to_owned(),
        after.text[range.start..after.text.len().saturating_sub(suffix)].to_owned(),
        before.cursor,
        before.selection.clone(),
        after.cursor,
        after.selection.clone(),
    )
}

fn make_undo_edit(
    range_before: Range<usize>,
    deleted: String,
    inserted: String,
    cursor_before: usize,
    selection_before: Option<Range<usize>>,
    cursor_after: usize,
    selection_after: Option<Range<usize>>,
) -> UndoEdit {
    UndoEdit {
        range_before,
        deleted,
        inserted,
        selection_before,
        cursor_before,
        selection_after,
        cursor_after,
    }
}

fn apply_undo_edit(engine: &mut ParleyEditorEngine, edit: &UndoEdit, undo: bool) {
    let (range, replacement, selection, cursor) = if undo {
        (
            edit.range_before.start..edit.range_before.start + edit.inserted.len(),
            edit.deleted.as_str(),
            edit.selection_before.clone(),
            edit.cursor_before,
        )
    } else {
        (
            edit.range_before.clone(),
            edit.inserted.as_str(),
            edit.selection_after.clone(),
            edit.cursor_after,
        )
    };
    engine.select_byte_range(range.start, range.end);
    engine.insert(replacement);
    match selection {
        Some(range) => engine.select_byte_range(range.start, range.end),
        None => engine.move_to_byte(cursor),
    }
}

fn restore(engine: &mut ParleyEditorEngine, snapshot: &UndoSnapshot) {
    engine.set_text(&snapshot.text);
    match &snapshot.selection {
        Some(range) => engine.select_byte_range(range.start, range.end),
        None => engine.move_to_byte(snapshot.cursor),
    }
}

fn local_edit_result_from_parts(
    session: &EditorSession,
    content_changed: bool,
    changed_range: Option<Range<usize>>,
    before_cursor: usize,
    before_selection: Option<Range<usize>>,
    cursor: usize,
    selection: Option<Range<usize>>,
) -> EditorLocalEditResult {
    EditorLocalEditResult {
        document_revision: session.document_revision,
        edit_sequence: session.edit_sequence,
        content_changed,
        interaction_changed: before_cursor != cursor || before_selection != selection,
        presentation_changed: content_changed
            || before_cursor != cursor
            || before_selection != selection,
        changed_range,
        cursor,
        selection,
        locally_dirty: session.edit_sequence != session.accepted_edit_sequence,
        dirty_changed: None,
    }
}

fn previous_char_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn local_edit_result_with_snapshot(
    session: &EditorSession,
    content_changed: bool,
    before: Option<&youth_editor_engine::EngineSnapshot>,
    snapshot: youth_editor_engine::EngineSnapshot,
) -> EditorLocalEditResult {
    let interaction_changed = before.is_some_and(|before| {
        before.cursor != snapshot.cursor || before.selection != snapshot.selection
    });
    EditorLocalEditResult {
        document_revision: session.document_revision,
        edit_sequence: session.edit_sequence,
        content_changed,
        interaction_changed,
        presentation_changed: content_changed || interaction_changed,
        changed_range: content_changed.then(|| changed_range(before, &snapshot)),
        cursor: snapshot.cursor,
        selection: snapshot.selection,
        locally_dirty: session.edit_sequence != session.accepted_edit_sequence,
        dirty_changed: None,
    }
}

fn changed_range(
    before: Option<&youth_editor_engine::EngineSnapshot>,
    after: &youth_editor_engine::EngineSnapshot,
) -> Range<usize> {
    let Some(before) = before else {
        return 0..after.text.len();
    };
    let mut prefix = before
        .text
        .bytes()
        .zip(after.text.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while prefix > 0 && !before.text.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let suffix = before.text[prefix..]
        .bytes()
        .rev()
        .zip(after.text[prefix..].bytes().rev())
        .take_while(|(left, right)| left == right)
        .count();
    prefix..before.text.len().saturating_sub(suffix)
}

impl EditorSessionSlot {
    pub(super) fn text_document_binding(&self) -> Option<crate::DocumentHandle> {
        self.session
            .as_ref()
            .and_then(|session| session.text_document)
    }

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
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    use youth_tree::NodeId;

    use super::{
        AccessibilityIdError, AccessibilityIdRegistry, AccessibilityKey, EDIT_SEQUENCE_BASE,
    };
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
    fn accessibility_ids_skip_semantic_nodes_and_retire_allocations() {
        let mut ids = AccessibilityIdRegistry::default();
        ids.reserve_semantic_ids([id(1), id(3)]).unwrap();
        assert_eq!(
            ids.allocate(AccessibilityKey::EditorTextRun {
                editor: id(1),
                run: 0,
            })
            .unwrap(),
            accesskit::NodeId(2)
        );
        assert_eq!(
            ids.allocate(AccessibilityKey::EditorTextRun {
                editor: id(1),
                run: 1,
            })
            .unwrap(),
            accesskit::NodeId(4)
        );
        assert_eq!(
            ids.reserve_semantic_ids([id(2)]),
            Err(AccessibilityIdError::SemanticCollision(2))
        );
    }

    #[test]
    fn accessibility_id_exhaustion_is_explicit() {
        let mut ids = AccessibilityIdRegistry {
            next: u64::MAX - 1,
            allocated: HashSet::new(),
            forward: HashMap::new(),
            reverse: HashMap::new(),
        };
        let key = AccessibilityKey::EditorTextRun {
            editor: id(1),
            run: 0,
        };
        assert_eq!(ids.allocate(key).unwrap(), accesskit::NodeId(u64::MAX));
        assert_eq!(
            ids.allocate(AccessibilityKey::EditorTextRun {
                editor: id(1),
                run: 1,
            }),
            Err(AccessibilityIdError::Exhausted)
        );
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
        assert_eq!(
            app.editor_snapshot(id(2)).expect("snapshot succeeds").text,
            "Scratchpad draft!"
        );

        let deleted = app
            .edit_editor_locally(id(2), EditorLocalEdit::Backspace)
            .expect("local backspace succeeds");
        assert_eq!(deleted.edit_sequence, 2);
        assert_eq!(
            app.editor_snapshot(id(2)).expect("snapshot succeeds").text,
            "Scratchpad draft"
        );
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
