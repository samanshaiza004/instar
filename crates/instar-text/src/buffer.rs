//! A document: text, a revision, and the history to take edits back.
//!
//! What is deliberately *not* here: anything about views. A buffer that knew
//! which views referenced it would be a buffer that knows about presentation,
//! and the cross-view relationship belongs to neither party — see
//! [`crate::TextSystem`].
//!
//! Also not here: synchronization state. `session_epoch`, `guest_ack_revision`
//! and a pending queue describe a synchronization *session*, not a document,
//! and in this package a pending queue could never drain.

use crate::edit::Transaction;
use crate::{AppliedEdit, EditJournal, Revision, TextEdit, TextError, TextSlice, TextStorage};

/// The host's replica of a document.
#[derive(Debug, Clone, Default)]
pub struct TextBuffer {
    text: TextStorage,
    revision: Revision,
    journal: EditJournal,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            text: TextStorage::from_text(text),
            revision: Revision::default(),
            journal: EditJournal::new(),
        }
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn len_bytes(&self) -> usize {
        self.text.len_bytes()
    }

    pub fn len_lines(&self) -> usize {
        self.text.len_lines()
    }

    pub fn text(&self) -> &TextStorage {
        &self.text
    }

    pub fn journal(&self) -> &EditJournal {
        &self.journal
    }

    /// A borrowed region. The only way to read text out of a buffer.
    pub fn slice(&self, range: std::ops::Range<usize>) -> Result<TextSlice<'_>, TextError> {
        self.text.slice(range)
    }

    /// Applies an edit, advancing the revision and recording an undo step.
    ///
    /// Ordering matters and is the reason a refused edit is never a partial
    /// one: the removed text is read *before* the storage is mutated, and the
    /// storage refuses a bad range before anything else has happened. Nothing
    /// between those two points can fail.
    pub(crate) fn apply(&mut self, edit: &TextEdit) -> Result<AppliedEdit, TextError> {
        let removed = self.text.slice(edit.range.clone())?.materialize();

        self.text.replace(edit.range.clone(), &edit.replacement)?;

        self.journal.record(Transaction {
            range: edit.range.clone(),
            removed,
            inserted: edit.replacement.clone(),
        });

        let base = self.revision;
        self.revision = self.revision.next();
        Ok(AppliedEdit {
            base_revision: base,
            resulting_revision: self.revision,
            edit: edit.clone(),
        })
    }

    /// The edit that undoes the last one, without applying it.
    ///
    /// Split from applying so [`crate::TextSystem`] can transform every view
    /// with the same code path an ordinary edit takes. An undo that moved
    /// carets by its own rules would be a second implementation of the thing
    /// most likely to be subtly wrong.
    pub(crate) fn take_undo(&mut self) -> Result<(TextEdit, Transaction), TextError> {
        let transaction = self.journal.take_undo().ok_or(TextError::NothingToUndo)?;
        Ok((transaction.undo(), transaction))
    }

    pub(crate) fn take_redo(&mut self) -> Result<(TextEdit, Transaction), TextError> {
        let transaction = self.journal.take_redo().ok_or(TextError::NothingToRedo)?;
        Ok((transaction.redo(), transaction))
    }

    /// Applies an undo/redo edit without recording new history.
    pub(crate) fn apply_without_recording(
        &mut self,
        edit: &TextEdit,
    ) -> Result<AppliedEdit, TextError> {
        self.text.replace(edit.range.clone(), &edit.replacement)?;
        let base = self.revision;
        self.revision = self.revision.next();
        Ok(AppliedEdit {
            base_revision: base,
            resulting_revision: self.revision,
            edit: edit.clone(),
        })
    }

    pub(crate) fn push_undone(&mut self, transaction: Transaction) {
        self.journal.push_undone(transaction);
    }

    pub(crate) fn push_done(&mut self, transaction: Transaction) {
        self.journal.push_done(transaction);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_advances_the_revision_and_reports_both_ends() {
        let mut buffer = TextBuffer::from_text("hello");
        assert_eq!(buffer.revision(), Revision(0));

        let applied = buffer.apply(&TextEdit::insert(5, " world")).expect("valid");
        assert_eq!(applied.base_revision, Revision(0));
        assert_eq!(applied.resulting_revision, Revision(1));
        assert_eq!(buffer.revision(), Revision(1));
        assert_eq!(buffer.slice(0..11).unwrap().materialize(), "hello world");
    }

    /// A refused edit is not a partial one.
    #[test]
    fn a_refused_edit_leaves_the_revision_and_text_alone() {
        let mut buffer = TextBuffer::from_text("héllo");
        let before = buffer.revision();

        // Byte 2 is inside 'é'.
        assert!(matches!(
            buffer.apply(&TextEdit::replace(2..3, "x")),
            Err(TextError::NotACharBoundary { byte: 2 })
        ));
        assert!(matches!(
            buffer.apply(&TextEdit::replace(0..99, "x")),
            Err(TextError::RangeOutOfBounds { .. })
        ));

        assert_eq!(buffer.revision(), before, "no revision was spent");
        assert_eq!(buffer.slice(0..6).unwrap().materialize(), "héllo");
    }

    /// The crate's stated invariant, held by a test rather than only by a
    /// benchmark.
    ///
    /// `textbench` measures this across three documents and eight operations,
    /// but a benchmark nobody runs on a Tuesday is not a regression lock. If a
    /// `to_string()` ever appears on the editing path, this goes red first.
    #[test]
    fn an_ordinary_edit_never_asks_for_the_document_contiguously() {
        let mut buffer = TextBuffer::from_text(&"x".repeat(1_000_000));
        crate::instrument::reset();

        buffer
            .apply(&TextEdit::replace(500_000..500_100, "short"))
            .expect("valid");

        let counts = crate::instrument::snapshot();
        assert_eq!(
            counts.whole_buffer_materializations, 0,
            "a hundred bytes changed in a megabyte, and nothing asked for the \
             megabyte"
        );
        assert_eq!(
            counts.materialized_bytes, 100,
            "the only copy an edit makes is the material undo has to keep"
        );
    }

    /// The undo journal holds the material an edit touched, and nothing else.
    ///
    /// The number that matters is what a small edit in a large document costs:
    /// a journal that snapshots the document would defeat the rope entirely.
    #[test]
    fn undo_retains_the_edited_material_not_the_document() {
        let mut buffer = TextBuffer::from_text(&"x".repeat(1_000_000));
        buffer
            .apply(&TextEdit::replace(500..600, "short"))
            .expect("valid");

        let retained = buffer.journal().retained_bytes();
        assert_eq!(
            retained, 105,
            "100 bytes removed plus 5 inserted -- undo cost follows the edit, \
             not the megabyte it happened inside"
        );
    }
}
