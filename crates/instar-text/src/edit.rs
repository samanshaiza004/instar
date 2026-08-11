//! Edits, and the journal that can take them back.

use std::ops::Range;

use crate::Revision;

/// One replacement of a byte range.
///
/// Insertion is an empty range; deletion is an empty replacement. One shape
/// rather than three, because the position-transform rules a view needs are
/// identical for all three and splitting them would mean writing those rules
/// three times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
}

impl TextEdit {
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self {
            range: at..at,
            replacement: text.into(),
        }
    }

    pub fn delete(range: Range<usize>) -> Self {
        Self {
            range,
            replacement: String::new(),
        }
    }

    pub fn replace(range: Range<usize>, text: impl Into<String>) -> Self {
        Self {
            range,
            replacement: text.into(),
        }
    }

    /// How many bytes the buffer grows by. Negative for a net deletion.
    pub fn delta(&self) -> isize {
        self.replacement.len() as isize - (self.range.end - self.range.start) as isize
    }

    /// Where the replacement ends once applied.
    pub fn resulting_end(&self) -> usize {
        self.range.start + self.replacement.len()
    }
}

/// What an applied edit was, and what it moved the buffer between.
///
/// The record the synchronization package will consume. It exists in package A
/// so that package C is a new consumer of an existing fact rather than a change
/// to how edits are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    pub base_revision: Revision,
    pub resulting_revision: Revision,
    pub edit: TextEdit,
}

/// One undoable step: what was replaced, and what replaced it.
///
/// Holds only the material the edit touched. That is the whole design — an
/// undo implementation that snapshots the document would defeat the rope
/// entirely, and it is the second thing `textbench` is built to catch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transaction {
    pub(crate) range: Range<usize>,
    /// What the range held before, so the edit can be reversed.
    pub(crate) removed: String,
    /// What the range holds now, so the reversal can be reversed.
    pub(crate) inserted: String,
}

impl Transaction {
    /// The edit that undoes this transaction.
    pub(crate) fn undo(&self) -> TextEdit {
        TextEdit {
            range: self.range.start..self.range.start + self.inserted.len(),
            replacement: self.removed.clone(),
        }
    }

    /// The edit that redoes it.
    pub(crate) fn redo(&self) -> TextEdit {
        TextEdit {
            range: self.range.start..self.range.start + self.removed.len(),
            replacement: self.inserted.clone(),
        }
    }
}

/// Undo and redo, as two stacks.
///
/// A new edit clears the redo stack, which is the ordinary rule: once history
/// diverges, the abandoned branch is not reachable and keeping it would be
/// retaining text nobody can get to.
#[derive(Debug, Clone, Default)]
pub struct EditJournal {
    done: Vec<Transaction>,
    undone: Vec<Transaction>,
}

impl EditJournal {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record(&mut self, transaction: Transaction) {
        self.done.push(transaction);
        self.undone.clear();
    }

    pub(crate) fn take_undo(&mut self) -> Option<Transaction> {
        self.done.pop()
    }

    pub(crate) fn push_undone(&mut self, transaction: Transaction) {
        self.undone.push(transaction);
    }

    pub(crate) fn take_redo(&mut self) -> Option<Transaction> {
        self.undone.pop()
    }

    pub(crate) fn push_done(&mut self, transaction: Transaction) {
        self.done.push(transaction);
    }

    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// Bytes of text the journal is holding.
    ///
    /// Instrumentation, and the number that says whether undo has quietly
    /// become a document-sized cost.
    pub fn retained_bytes(&self) -> usize {
        self.done
            .iter()
            .chain(&self.undone)
            .map(|t| t.removed.len() + t.inserted.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_reports_how_far_it_moves_what_follows() {
        assert_eq!(TextEdit::insert(5, "abc").delta(), 3);
        assert_eq!(TextEdit::delete(5..9).delta(), -4);
        assert_eq!(TextEdit::replace(5..9, "ab").delta(), -2);
        assert_eq!(TextEdit::insert(5, "abc").resulting_end(), 8);
    }

    #[test]
    fn a_transaction_reverses_and_reverses_back() {
        let transaction = Transaction {
            range: 4..7,
            removed: "old".to_string(),
            inserted: "brand new".to_string(),
        };
        assert_eq!(transaction.undo(), TextEdit::replace(4..13, "old"));
        assert_eq!(transaction.redo(), TextEdit::replace(4..7, "brand new"));
    }

    #[test]
    fn a_new_edit_abandons_the_redo_branch() {
        let mut journal = EditJournal::new();
        journal.record(Transaction {
            range: 0..0,
            removed: String::new(),
            inserted: "a".to_string(),
        });
        let undone = journal.take_undo().expect("one to undo");
        journal.push_undone(undone);
        assert!(journal.can_redo());

        journal.record(Transaction {
            range: 0..0,
            removed: String::new(),
            inserted: "b".to_string(),
        });
        assert!(
            !journal.can_redo(),
            "history diverged, so the abandoned branch is unreachable and not \
             worth retaining text for"
        );
    }
}
