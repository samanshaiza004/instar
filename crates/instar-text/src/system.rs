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
