//! The registry, and the only thing that knows a buffer has more than one view.
//!
//! # Why this exists rather than putting it on `TextBuffer`
//!
//! An edit through one view has to move every *other* view of the same buffer.
//! That relationship belongs to neither party: a buffer holding a list of its
//! views would be a document that knows about presentation, and a view cannot
//! reach its siblings without becoming a registry itself.
//!
//! It also keeps `instar-host` from becoming the place that knows
//! position-transform rules. The host composes this subsystem with the UI tree;
//! it should not have to understand what an insertion does to a caret.
//!
//! # Generational handles
//!
//! Ids are `{ id, generation }` from the first line of this crate, not because
//! package A needs it but because package C will. A guest holding
//! `TextViewId(12)` after view 12 is dropped and its slot reused would mutate
//! whatever took its place — the ABA hole `NodeKey` already had to grow a
//! generation to close. Adding it later would be a protocol break rather than
//! a field.

use std::collections::HashMap;

/// How many buffers may be live at once.
///
/// A bound on live resources, which is a different question from a bound on
/// historical identities — `MAX_NODE_IDS` exists because a guest chooses node
/// ids and can burn them forever. Here the host allocates, slots are reused,
/// and generations advance per slot, so the historical ledger is bounded by
/// this number rather than growing with time. No second 65k ledger is needed.
///
/// Generous, because the failure this prevents is a leak rather than a
/// legitimate workload: an editor with a thousand documents open has a
/// different problem than a bound.
pub const MAX_TEXT_BUFFERS: usize = 4_096;

/// How many views may be live at once. Higher than buffers because splits and
/// panes multiply views over a fixed set of documents.
pub const MAX_TEXT_VIEWS: usize = 8_192;

use crate::{
    AppliedEdit, Revision, TextBuffer, TextBufferId, TextEdit, TextError, TextView, TextViewId,
};

/// Every buffer and every view, and the edits that move between them.
#[derive(Debug, Default)]
pub struct TextSystem {
    buffers: HashMap<TextBufferId, TextBuffer>,
    views: HashMap<TextViewId, TextView>,
    /// Slots are reused, and that is what makes the generation load-bearing.
    ///
    /// Handing out a fresh id every time would leave `generation` permanently
    /// zero — a field advertising a hazard that cannot occur, which this
    /// project has removed once already. Reuse is also the behaviour a
    /// long-lived editor wants: closing and opening views all session should
    /// not walk an id counter toward its ceiling.
    next_buffer: u32,
    next_view: u32,
    free_buffers: Vec<u32>,
    free_views: Vec<u32>,
    buffer_generations: HashMap<u32, u32>,
    view_generations: HashMap<u32, u32>,
}

impl TextSystem {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a buffer, or refuses if [`MAX_TEXT_BUFFERS`] are already live.
    pub fn open_buffer(&mut self, text: &str) -> Result<TextBufferId, TextError> {
        if self.buffers.len() >= MAX_TEXT_BUFFERS {
            return Err(TextError::TooManyBuffers {
                limit: MAX_TEXT_BUFFERS,
            });
        }
        let id = self.free_buffers.pop().unwrap_or_else(|| {
            let id = self.next_buffer;
            self.next_buffer += 1;
            id
        });
        let generation = *self.buffer_generations.entry(id).or_insert(0);
        let handle = TextBufferId { id, generation };
        self.buffers.insert(handle, TextBuffer::from_text(text));
        Ok(handle)
    }

    /// Drops a buffer and burns its slot's generation.
    ///
    /// Returns whether anything was there. Views of it are left alone: they
    /// will fail with [`TextError::NoSuchBuffer`], which is the honest answer
    /// and better than silently retargeting them.
    pub fn close_buffer(&mut self, buffer: TextBufferId) -> bool {
        let removed = self.buffers.remove(&buffer).is_some();
        if removed {
            *self.buffer_generations.entry(buffer.id).or_insert(0) += 1;
            self.free_buffers.push(buffer.id);
        }
        removed
    }

    pub fn open_view(&mut self, buffer: TextBufferId) -> Result<TextViewId, TextError> {
        if !self.buffers.contains_key(&buffer) {
            return Err(TextError::NoSuchBuffer(buffer));
        }
        if self.views.len() >= MAX_TEXT_VIEWS {
            return Err(TextError::TooManyViews {
                limit: MAX_TEXT_VIEWS,
            });
        }
        let id = self.free_views.pop().unwrap_or_else(|| {
            let id = self.next_view;
            self.next_view += 1;
            id
        });
        let generation = *self.view_generations.entry(id).or_insert(0);
        let handle = TextViewId { id, generation };
        self.views.insert(handle, TextView::new(buffer));
        Ok(handle)
    }

    pub fn close_view(&mut self, view: TextViewId) -> bool {
        let removed = self.views.remove(&view).is_some();
        if removed {
            *self.view_generations.entry(view.id).or_insert(0) += 1;
            self.free_views.push(view.id);
        }
        removed
    }

    pub fn buffer(&self, buffer: TextBufferId) -> Result<&TextBuffer, TextError> {
        self.buffers
            .get(&buffer)
            .ok_or(TextError::NoSuchBuffer(buffer))
    }

    pub fn view(&self, view: TextViewId) -> Result<&TextView, TextError> {
        self.views.get(&view).ok_or(TextError::NoSuchView(view))
    }

    pub fn view_mut(&mut self, view: TextViewId) -> Result<&mut TextView, TextError> {
        self.views.get_mut(&view).ok_or(TextError::NoSuchView(view))
    }

    pub fn revision(&self, buffer: TextBufferId) -> Result<Revision, TextError> {
        Ok(self.buffer(buffer)?.revision())
    }

    /// Applies an edit through one view.
    ///
    /// The order is the contract, and steps 4 and 5 are the reason this
    /// function exists at all:
    ///
    /// ```text
    /// 1  resolve the view, and the buffer it names
    /// 2  mutate the buffer, advancing the revision
    /// 3  record the undo transaction
    /// 4  transform the originating view
    /// 5  transform every other view of that buffer
    /// ```
    ///
    /// Nothing is transformed until the buffer has actually changed, so a
    /// refused edit leaves every view exactly where it was.
    pub fn apply_edit(
        &mut self,
        view: TextViewId,
        edit: TextEdit,
    ) -> Result<AppliedEdit, TextError> {
        let buffer_id = self
            .views
            .get(&view)
            .ok_or(TextError::NoSuchView(view))?
            .buffer();
        let buffer = self
            .buffers
            .get_mut(&buffer_id)
            .ok_or(TextError::NoSuchBuffer(buffer_id))?;

        let applied = buffer.apply(&edit)?;
        self.transform_views(buffer_id, &applied.edit, Some(view));
        Ok(applied)
    }

    /// Applies an edit directly to a buffer, transforming every view of it.
    ///
    /// The counterpart to [`Self::apply_edit`] for edits that did not
    /// originate from any particular view. A guest's own `apply-edits`
    /// reports canonical changes to a *document* -- it has no caret, no
    /// selection, no notion of "the view that made this edit" at all, since
    /// views are a host-side presentation concept the guest's document model
    /// never sees. So there is no view to exclude from transformation the way
    /// a host-local edit excludes the one that produced it: every view of the
    /// buffer moves by exactly the same rule.
    pub fn apply_edit_to_buffer(
        &mut self,
        buffer_id: TextBufferId,
        edit: TextEdit,
    ) -> Result<AppliedEdit, TextError> {
        let buffer = self
            .buffers
            .get_mut(&buffer_id)
            .ok_or(TextError::NoSuchBuffer(buffer_id))?;

        let applied = buffer.apply(&edit)?;
        self.transform_views(buffer_id, &applied.edit, None);
        Ok(applied)
    }

    /// Applies a whole batch of edits to a buffer, sequentially, or none of
    /// them.
    ///
    /// This is `apply-edits`' atomicity claim (Package C), and the mechanism
    /// is a clone rather than a rollback. Undo was tempting and wrong: undoing
    /// N edits by reversing them is N *more* edits as far as `TextBuffer` is
    /// concerned, so the revision after "rolling back" is not the revision
    /// before the batch, it is `before + 2N` -- observably mutated, and every
    /// one of those 2N steps would otherwise get reported to every other
    /// generation watching the buffer through `next-edit`, which is a
    /// document that was never supposed to have changed at all.
    ///
    /// A clone sidesteps the question entirely:
    ///
    /// ```text
    /// clone the buffer                    crop::Rope is O(1), see C0
    /// apply every edit to the clone        the real buffer is untouched so far
    /// any edit fails -> stop, discard the clone, return the error
    /// all edits succeed -> swap the clone in, one assignment
    /// ```
    ///
    /// `TextBuffer::apply`'s own contract already guarantees a single refused
    /// edit is never partial (the removed text is read before the storage is
    /// mutated); this extends the same guarantee across the whole sequence by
    /// construction, because nothing about a discarded clone can be observed.
    pub fn apply_edits_to_buffer(
        &mut self,
        buffer_id: TextBufferId,
        edits: &[TextEdit],
    ) -> Result<Vec<AppliedEdit>, TextError> {
        let original = self
            .buffers
            .get(&buffer_id)
            .ok_or(TextError::NoSuchBuffer(buffer_id))?;
        let mut scratch = original.clone();

        let mut applied = Vec::with_capacity(edits.len());
        for edit in edits {
            applied.push(scratch.apply(edit)?);
        }

        // Every edit in the batch validated against the exact sequence it
        // actually produced. Commit: swap the proven buffer in, then move
        // every view through the same sequence, in order -- the same
        // transform a real one-at-a-time application would have produced.
        *self
            .buffers
            .get_mut(&buffer_id)
            .expect("resolved above; nothing between there and here can remove it") = scratch;
        for applied_edit in &applied {
            self.transform_views(buffer_id, &applied_edit.edit, None);
        }
        Ok(applied)
    }

    /// Undoes the last edit to a buffer, moving every view of it.
    ///
    /// Routed through the same transform as an ordinary edit rather than
    /// restoring saved positions. An undo that moved carets by its own rules
    /// would be a second implementation of the part most likely to be subtly
    /// wrong.
    pub fn undo(&mut self, view: TextViewId) -> Result<AppliedEdit, TextError> {
        let buffer_id = self
            .views
            .get(&view)
            .ok_or(TextError::NoSuchView(view))?
            .buffer();
        let buffer = self
            .buffers
            .get_mut(&buffer_id)
            .ok_or(TextError::NoSuchBuffer(buffer_id))?;

        let (edit, transaction) = buffer.take_undo()?;
        let applied = buffer.apply_without_recording(&edit)?;
        buffer.push_undone(transaction);
        self.transform_views(buffer_id, &applied.edit, Some(view));
        Ok(applied)
    }

    pub fn redo(&mut self, view: TextViewId) -> Result<AppliedEdit, TextError> {
        let buffer_id = self
            .views
            .get(&view)
            .ok_or(TextError::NoSuchView(view))?
            .buffer();
        let buffer = self
            .buffers
            .get_mut(&buffer_id)
            .ok_or(TextError::NoSuchBuffer(buffer_id))?;

        let (edit, transaction) = buffer.take_redo()?;
        let applied = buffer.apply_without_recording(&edit)?;
        buffer.push_done(transaction);
        self.transform_views(buffer_id, &applied.edit, Some(view));
        Ok(applied)
    }

    /// How many buffers are live. Diagnostics, and what B2e's lease teardown
    /// tests assert a return to baseline against.
    pub fn live_buffers(&self) -> usize {
        self.buffers.len()
    }

    pub fn live_views(&self) -> usize {
        self.views.len()
    }

    /// Every live buffer.
    ///
    /// For a caller that has to decide which buffers are still reachable, and
    /// cannot answer that from a handle it was given.
    pub fn buffers(&self) -> impl Iterator<Item = TextBufferId> + '_ {
        self.buffers.keys().copied()
    }

    /// Every live view.
    pub fn views(&self) -> impl Iterator<Item = TextViewId> + '_ {
        self.views.keys().copied()
    }

    /// How many views a buffer has. Diagnostics, and what `textbench` counts.
    pub fn views_of(&self, buffer: TextBufferId) -> usize {
        self.views.values().filter(|v| v.buffer() == buffer).count()
    }

    fn transform_views(
        &mut self,
        buffer: TextBufferId,
        edit: &TextEdit,
        origin: Option<TextViewId>,
    ) {
        for (id, view) in self.views.iter_mut() {
            if view.buffer() != buffer {
                continue;
            }
            view.transform(edit, Some(*id) == origin);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Selection, TextPosition};

    /// Every test wants a buffer, and none of them is testing the bound.
    fn open(text: &mut TextSystem, content: &str) -> TextBufferId {
        text.open_buffer(content).expect("under the buffer limit")
    }

    #[test]
    fn an_edit_through_one_view_moves_the_other() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "hello world");
        let a = text.open_view(buffer).expect("live buffer");
        let b = text.open_view(buffer).expect("live buffer");

        text.view_mut(b).unwrap().set_caret(9);
        text.apply_edit(a, TextEdit::insert(0, ">> "))
            .expect("valid");

        assert_eq!(
            text.view(b).unwrap().caret(),
            12,
            "an edit through A moves B, or two views of one buffer is a lie"
        );
        assert_eq!(text.revision(buffer).unwrap(), Revision(1));
    }

    /// The affinity policy, at the level that matters: through the system,
    /// with two real views.
    #[test]
    fn only_the_editing_view_follows_its_own_insertion() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "0123456789abcdef");
        let a = text.open_view(buffer).unwrap();
        let b = text.open_view(buffer).unwrap();

        text.view_mut(a).unwrap().set_caret(10);
        text.view_mut(b).unwrap().set_caret(10);
        text.apply_edit(a, TextEdit::insert(10, "XYZ"))
            .expect("valid");

        assert_eq!(text.view(a).unwrap().caret(), 13, "the typist follows");
        assert_eq!(text.view(b).unwrap().caret(), 10, "the observer does not");
    }

    /// The contrast the test above draws: with no originating view at all,
    /// nothing is privileged as the typist. `transform_position`'s policy for
    /// a pure insertion exactly at a caret is "follow it" for the originating
    /// view and "hold position" for every other -- and with `origin: None`,
    /// every view gets the second treatment, including the one whose caret
    /// would have followed had this gone through `apply_edit` instead. A
    /// guest's own `apply-edits` has no caret to privilege in the first
    /// place: it reports a document change, not an interaction.
    #[test]
    fn apply_edit_to_buffer_exempts_no_view() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "0123456789abcdef");
        let a = text.open_view(buffer).unwrap();
        let b = text.open_view(buffer).unwrap();

        text.view_mut(a).unwrap().set_caret(10);
        text.view_mut(b).unwrap().set_caret(10);
        text.apply_edit_to_buffer(buffer, TextEdit::insert(10, "XYZ"))
            .expect("valid");

        assert_eq!(
            text.view(a).unwrap().caret(),
            10,
            "A held its position exactly as B did -- the same treatment \
             apply_edit gives only to the non-originating view, and here \
             there is no originating view at all"
        );
        assert_eq!(
            text.view(b).unwrap().caret(),
            10,
            "and B is identical to A: neither view is distinguished from \
             the other"
        );
        assert_eq!(text.revision(buffer).unwrap(), Revision(1));
    }

    #[test]
    fn a_refused_edit_moves_no_view_at_all() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "héllo");
        let a = text.open_view(buffer).unwrap();
        let b = text.open_view(buffer).unwrap();
        text.view_mut(b).unwrap().set_caret(5);

        // Byte 2 is inside 'é'.
        assert!(text.apply_edit(a, TextEdit::replace(2..3, "x")).is_err());

        assert_eq!(text.view(b).unwrap().caret(), 5);
        assert_eq!(text.revision(buffer).unwrap(), Revision(0));
    }

    #[test]
    fn a_batch_applies_sequentially_with_each_edit_against_the_last() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "hello");

        let applied = text
            .apply_edits_to_buffer(
                buffer,
                &[
                    TextEdit::insert(5, " world"),
                    // This range only exists because the first edit already
                    // ran: "hello world" -> delete "world" at 6..11.
                    TextEdit::delete(6..11),
                ],
            )
            .expect("both edits are valid against the sequence they produce");

        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].base_revision, Revision(0));
        assert_eq!(applied[0].resulting_revision, Revision(1));
        assert_eq!(
            applied[1].base_revision,
            Revision(1),
            "the second edit's base is the first edit's result, not the \
             batch's starting revision"
        );
        assert_eq!(applied[1].resulting_revision, Revision(2));
        assert_eq!(text.revision(buffer).unwrap(), Revision(2));
        assert_eq!(
            text.buffer(buffer)
                .unwrap()
                .slice(0..6)
                .unwrap()
                .materialize(),
            "hello "
        );
    }

    /// The atomicity claim itself: a batch whose second edit is invalid
    /// leaves the buffer exactly as it was, not one edit into the sequence.
    /// This is the mutant the frozen order names directly -- "conflict
    /// applies a prefix" -- proven here one level below the conflict check,
    /// against a batch that passed the revision gate and failed partway
    /// through anyway.
    #[test]
    fn a_batch_that_fails_partway_leaves_the_buffer_completely_unchanged() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "hello");
        let view = text.open_view(buffer).unwrap();
        text.view_mut(view).unwrap().set_caret(3);

        let result = text.apply_edits_to_buffer(
            buffer,
            &[
                TextEdit::insert(5, " world"), // valid
                TextEdit::delete(100..200),    // out of range once applied
            ],
        );

        assert!(result.is_err(), "the batch as a whole must be refused");
        assert_eq!(
            text.revision(buffer).unwrap(),
            Revision(0),
            "not even the first, individually-valid edit may survive"
        );
        assert_eq!(
            text.buffer(buffer)
                .unwrap()
                .slice(0..5)
                .unwrap()
                .materialize(),
            "hello",
            "the content is byte-for-byte what it was before the batch"
        );
        assert_eq!(
            text.view(view).unwrap().caret(),
            3,
            "and no view moved -- the first edit's transform never happened, \
             because the first edit was never actually applied to the real \
             buffer, only to a clone that was discarded"
        );
    }

    #[test]
    fn undo_and_redo_move_every_view_through_the_same_transform() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "hello");
        let a = text.open_view(buffer).unwrap();
        let b = text.open_view(buffer).unwrap();
        text.view_mut(b).unwrap().set_caret(5);

        text.apply_edit(a, TextEdit::insert(0, "say "))
            .expect("valid");
        assert_eq!(text.view(b).unwrap().caret(), 9);

        text.undo(a).expect("one edit to undo");
        assert_eq!(
            text.buffer(buffer)
                .unwrap()
                .slice(0..5)
                .unwrap()
                .materialize(),
            "hello"
        );
        assert_eq!(
            text.view(b).unwrap().caret(),
            5,
            "undo returns the other view's caret too"
        );

        text.redo(a).expect("one edit to redo");
        assert_eq!(text.view(b).unwrap().caret(), 9);
    }

    /// The ABA case, closed before it can reach a guest.
    #[test]
    fn a_reused_slot_does_not_answer_to_the_handle_it_replaced() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "one");
        let stale = text.open_view(buffer).unwrap();

        assert!(text.close_view(stale));
        let fresh = text.open_view(buffer).unwrap();

        assert_eq!(fresh.id, stale.id, "the slot really was reused");
        assert_ne!(
            fresh.generation, stale.generation,
            "and the generation is what tells them apart"
        );
        assert!(matches!(
            text.apply_edit(stale, TextEdit::insert(0, "x")),
            Err(TextError::NoSuchView(_))
        ));
        assert_eq!(
            text.buffer(buffer).unwrap().len_bytes(),
            3,
            "the stale handle mutated nothing"
        );
    }

    #[test]
    fn a_closed_buffer_is_gone_rather_than_retargeted() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "one");
        let view = text.open_view(buffer).unwrap();
        assert!(text.close_buffer(buffer));

        let replacement = open(&mut text, "two");
        assert_eq!(replacement.id, buffer.id);
        assert_ne!(replacement.generation, buffer.generation);
        assert!(matches!(
            text.apply_edit(view, TextEdit::insert(0, "x")),
            Err(TextError::NoSuchBuffer(_))
        ));
    }

    #[test]
    fn a_selection_replace_leaves_the_caret_after_what_was_typed() {
        let mut text = TextSystem::new();
        let buffer = open(&mut text, "hello world");
        let a = text.open_view(buffer).unwrap();
        text.view_mut(a).unwrap().set_selection(Selection {
            anchor: TextPosition::at(6),
            head: TextPosition::at(11),
        });

        text.apply_edit(a, TextEdit::replace(6..11, "there"))
            .unwrap();
        assert_eq!(text.view(a).unwrap().caret(), 11);
        assert_eq!(
            text.buffer(buffer)
                .unwrap()
                .slice(0..11)
                .unwrap()
                .materialize(),
            "hello there"
        );
    }
}

#[cfg(test)]
mod limits {
    use super::*;

    /// The bound is on what is *live*, and closing gives the slot back.
    ///
    /// This is the distinction `NodeKey` had to learn separately: `MAX_NODES`
    /// bounds simultaneous nodes, `MAX_NODE_IDS` bounds identities burned over
    /// time. Here the host allocates and reuses slots, so one bound answers
    /// both — but only if closing actually returns capacity, which is what
    /// this asserts.
    #[test]
    fn the_bound_is_on_live_resources_and_closing_returns_capacity() {
        let mut text = TextSystem::new();
        let mut open = Vec::new();
        for _ in 0..MAX_TEXT_BUFFERS {
            open.push(text.open_buffer("").expect("under the limit"));
        }
        assert!(matches!(
            text.open_buffer(""),
            Err(TextError::TooManyBuffers { .. })
        ));

        assert!(text.close_buffer(open.pop().expect("one to close")));
        assert!(
            text.open_buffer("").is_ok(),
            "a closed buffer returns its slot; the ceiling is not a lifetime \
             budget"
        );
    }

    #[test]
    fn views_have_their_own_ceiling() {
        let mut text = TextSystem::new();
        let buffer = text.open_buffer("").expect("under the limit");
        for _ in 0..MAX_TEXT_VIEWS {
            text.open_view(buffer).expect("under the limit");
        }
        assert!(matches!(
            text.open_view(buffer),
            Err(TextError::TooManyViews { .. })
        ));
    }
}
