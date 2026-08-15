//! Small, guest-safe editing mechanisms for first-party applications.
//!
//! `instar-editor-core` is a first-party userland library, not part of
//! Instar's semantic contract. Applications may replace any or all of it.
//! The document and all editing policy remain in the guest; the host has no
//! replica of this state.

#![forbid(unsafe_code)]

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Revision(pub u64);

impl Revision {
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Affinity {
    #[default]
    Downstream,
    Upstream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    pub byte: usize,
    pub affinity: Affinity,
}

impl Position {
    pub fn at(byte: usize) -> Self {
        Self {
            byte,
            affinity: Affinity::Downstream,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: Position,
    pub head: Position,
}

impl Selection {
    pub fn at(byte: usize) -> Self {
        Self {
            anchor: Position::at(byte),
            head: Position::at(byte),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.anchor.byte == self.head.byte
    }
    pub fn range(&self) -> Range<usize> {
        self.anchor.byte.min(self.head.byte)..self.anchor.byte.max(self.head.byte)
    }
    pub fn extend_to(self, head: Position) -> Self {
        Self {
            anchor: self.anchor,
            head,
        }
    }
}

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
}

/// Maps positions from the document before a transaction to the document
/// after it.
///
/// Positions at an insertion or replacement boundary use their affinity:
/// [`Affinity::Upstream`] stays before inserted content and
/// [`Affinity::Downstream`] moves after it. Positions inside a replaced range
/// collapse to the corresponding side of the replacement. A position exactly
/// at the end of an edited range is after that edit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PositionMap {
    edits: Vec<MappingEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MappingEdit {
    range: Range<usize>,
    inserted_len: usize,
}

impl PositionMap {
    pub fn map_position(&self, position: Position) -> Position {
        let original = position.byte;
        let mut delta = 0isize;

        for edit in &self.edits {
            let start = edit.range.start;
            let end = edit.range.end;

            if original < start {
                break;
            }

            if original == start {
                let offset = match position.affinity {
                    Affinity::Upstream => 0,
                    Affinity::Downstream => edit.inserted_len,
                };
                return Position {
                    byte: shifted(start + offset, delta),
                    affinity: position.affinity,
                };
            }

            if original < end {
                let offset = match position.affinity {
                    Affinity::Upstream => 0,
                    Affinity::Downstream => edit.inserted_len,
                };
                return Position {
                    byte: shifted(start + offset, delta),
                    affinity: position.affinity,
                };
            }

            delta += edit.inserted_len as isize - (end - start) as isize;
        }

        Position {
            byte: shifted(original, delta),
            affinity: position.affinity,
        }
    }

    pub fn map_selection(&self, selection: Selection) -> Selection {
        Selection {
            anchor: self.map_position(selection.anchor),
            head: self.map_position(selection.head),
        }
    }
}

fn shifted(byte: usize, delta: isize) -> usize {
    if delta.is_negative() {
        byte - delta.unsigned_abs()
    } else {
        byte + delta as usize
    }
}

/// The observable result of one successful transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionResult {
    revision: Revision,
    position_map: PositionMap,
}

impl TransactionResult {
    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn position_map(&self) -> &PositionMap {
        &self.position_map
    }

    pub fn map_position(&self, position: Position) -> Position {
        self.position_map.map_position(position)
    }

    pub fn map_selection(&self, selection: Selection) -> Selection {
        self.position_map.map_selection(selection)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("byte range {start}..{end} is inverted")]
    InvertedRange { start: usize, end: usize },
    #[error("byte range {start}..{end} is out of bounds for a {len}-byte document")]
    OutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("byte {byte} is not a UTF-8 boundary")]
    NotCharBoundary { byte: usize },
    #[error("byte {byte} is not an extended grapheme boundary")]
    NotGraphemeBoundary { byte: usize },
    #[error(
        "edit {first} ({first_start}..{first_end}) overlaps edit {second} ({second_start}..{second_end})"
    )]
    OverlappingEdits {
        first: usize,
        first_start: usize,
        first_end: usize,
        second: usize,
        second_start: usize,
        second_end: usize,
    },
    #[error("line {line} is out of bounds for {lines} lines")]
    LineOutOfBounds { line: usize, lines: usize },
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
}

#[derive(Debug, Clone)]
struct Transaction {
    edits: Vec<AppliedEdit>,
}

#[derive(Debug, Clone)]
struct AppliedEdit {
    range: Range<usize>,
    removed: String,
    inserted: String,
}

#[derive(Debug, Clone, Default)]
pub struct Document {
    rope: crop::Rope,
    revision: Revision,
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
}

impl Document {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from_text(text: &str) -> Self {
        Self {
            rope: crop::Rope::from(text),
            ..Self::default()
        }
    }
    pub fn revision(&self) -> Revision {
        self.revision
    }
    pub fn len_bytes(&self) -> usize {
        self.rope.byte_len()
    }
    pub fn len_lines(&self) -> usize {
        self.rope.line_len()
    }
    pub fn is_empty(&self) -> bool {
        self.len_bytes() == 0
    }
    pub fn as_string(&self) -> String {
        self.rope.chunks().collect()
    }
    pub fn slice(&self, range: Range<usize>) -> Result<String, EditError> {
        self.validate(&range)?;
        Ok(self.rope.byte_slice(range).chunks().collect())
    }
    pub fn byte_of_line(&self, line: usize) -> Result<usize, EditError> {
        if line > self.len_lines() {
            return Err(EditError::LineOutOfBounds {
                line,
                lines: self.len_lines(),
            });
        }
        Ok(self.rope.byte_of_line(line))
    }
    pub fn line_of_byte(&self, byte: usize) -> Result<usize, EditError> {
        self.validate(&(byte..byte))?;
        Ok(self.rope.line_of_byte(byte))
    }

    /// Returns one hard-line byte range without materializing neighboring
    /// lines or the document. A trailing CRLF remains part of its line.
    pub fn line_range(&self, line: usize) -> Result<Range<usize>, EditError> {
        if line >= self.len_lines() {
            return Err(EditError::LineOutOfBounds {
                line,
                lines: self.len_lines(),
            });
        }
        let start = self.rope.byte_of_line(line);
        let end = if line + 1 < self.len_lines() {
            self.rope.byte_of_line(line + 1)
        } else {
            self.len_bytes()
        };
        Ok(start..end)
    }
    pub fn is_char_boundary(&self, byte: usize) -> bool {
        byte <= self.len_bytes() && self.rope.is_char_boundary(byte)
    }
    pub fn is_grapheme_boundary(&self, byte: usize) -> bool {
        byte <= self.len_bytes() && self.rope.is_grapheme_boundary(byte)
    }
    pub fn next_grapheme_boundary(&self, byte: usize) -> Result<usize, EditError> {
        self.validate_grapheme(byte)?;
        if byte == self.len_bytes() {
            return Ok(byte);
        }
        Ok(byte
            + self
                .rope
                .byte_slice(byte..self.len_bytes())
                .graphemes()
                .next()
                .expect("non-empty")
                .len())
    }
    pub fn previous_grapheme_boundary(&self, byte: usize) -> Result<usize, EditError> {
        self.validate_grapheme(byte)?;
        if byte == 0 {
            return Ok(0);
        }
        let mut at = 0;
        for grapheme in self.rope.byte_slice(0..byte).graphemes() {
            let next = at + grapheme.len();
            if next >= byte {
                return Ok(at);
            }
            at = next;
        }
        Ok(at)
    }
    pub fn apply(&mut self, edit: TextEdit) -> Result<Revision, EditError> {
        self.apply_transaction(vec![edit])
            .map(|result| result.revision())
    }

    pub fn apply_batch(&mut self, edits: Vec<TextEdit>) -> Result<Revision, EditError> {
        self.apply_transaction(edits)
            .map(|result| result.revision())
    }

    /// Applies non-overlapping edits against one shared pre-transaction
    /// snapshot. Validation and removed-text capture finish before the rope,
    /// revision, or history are changed.
    pub fn apply_transaction(
        &mut self,
        edits: Vec<TextEdit>,
    ) -> Result<TransactionResult, EditError> {
        let mut prepared = Vec::with_capacity(edits.len());

        for (index, edit) in edits.into_iter().enumerate() {
            self.validate(&edit.range)?;
            prepared.push((
                index,
                AppliedEdit {
                    removed: self.slice(edit.range.clone())?,
                    range: edit.range,
                    inserted: edit.replacement,
                },
            ));
        }

        prepared.sort_by_key(|(index, edit)| (edit.range.start, edit.range.end, *index));
        for pair in prepared.windows(2) {
            let (first_index, first) = &pair[0];
            let (second_index, second) = &pair[1];
            let ranges_overlap =
                first.range.end > second.range.start || first.range.start == second.range.start;
            if ranges_overlap {
                return Err(EditError::OverlappingEdits {
                    first: *first_index,
                    first_start: first.range.start,
                    first_end: first.range.end,
                    second: *second_index,
                    second_start: second.range.start,
                    second_end: second.range.end,
                });
            }
        }

        if prepared.is_empty() {
            return Ok(TransactionResult {
                revision: self.revision,
                position_map: PositionMap::default(),
            });
        }

        let edits: Vec<AppliedEdit> = prepared.into_iter().map(|(_, edit)| edit).collect();
        let position_map = PositionMap {
            edits: edits
                .iter()
                .map(|edit| MappingEdit {
                    range: edit.range.clone(),
                    inserted_len: edit.inserted.len(),
                })
                .collect(),
        };

        for edit in edits.iter().rev() {
            self.rope.replace(edit.range.clone(), &edit.inserted);
        }
        self.undo.push(Transaction { edits });
        self.redo.clear();
        self.revision = self.revision.next();

        Ok(TransactionResult {
            revision: self.revision,
            position_map,
        })
    }

    pub fn undo(&mut self) -> Result<Revision, EditError> {
        let t = self.undo.last().cloned().ok_or(EditError::NothingToUndo)?;
        self.validate_undo(&t)?;
        let t = self.undo.pop().expect("undo entry was just checked");
        for edit in &t.edits {
            let start = edit.range.start;
            self.rope
                .replace(start..start + edit.inserted.len(), &edit.removed);
        }
        self.redo.push(t);
        self.revision = self.revision.next();
        Ok(self.revision)
    }

    pub fn redo(&mut self) -> Result<Revision, EditError> {
        let t = self.redo.last().cloned().ok_or(EditError::NothingToRedo)?;
        for edit in &t.edits {
            self.validate(&edit.range)?;
        }
        let t = self.redo.pop().expect("redo entry was just checked");
        for edit in t.edits.iter().rev() {
            self.rope.replace(edit.range.clone(), &edit.inserted);
        }
        self.undo.push(t);
        self.revision = self.revision.next();
        Ok(self.revision)
    }
    fn validate(&self, range: &Range<usize>) -> Result<(), EditError> {
        let len = self.len_bytes();
        if range.start > range.end {
            return Err(EditError::InvertedRange {
                start: range.start,
                end: range.end,
            });
        }
        if range.end > len {
            return Err(EditError::OutOfBounds {
                start: range.start,
                end: range.end,
                len,
            });
        }
        for &byte in &[range.start, range.end] {
            if !self.rope.is_char_boundary(byte) {
                return Err(EditError::NotCharBoundary { byte });
            }
        }
        Ok(())
    }

    fn validate_undo(&self, transaction: &Transaction) -> Result<(), EditError> {
        for edit in &transaction.edits {
            self.validate(&(edit.range.start..edit.range.start + edit.inserted.len()))?;
        }
        Ok(())
    }
    fn validate_grapheme(&self, byte: usize) -> Result<(), EditError> {
        self.validate(&(byte..byte))?;
        if !self.rope.is_grapheme_boundary(byte) {
            return Err(EditError::NotGraphemeBoundary { byte });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_are_guest_local_and_revisioned() {
        let mut d = Document::from_text("abc");
        d.apply(TextEdit::insert(1, "X")).unwrap();
        assert_eq!(d.as_string(), "aXbc");
        assert_eq!(d.revision(), Revision(1));
        d.undo().unwrap();
        assert_eq!(d.as_string(), "abc");
        d.redo().unwrap();
        assert_eq!(d.as_string(), "aXbc");
    }

    #[test]
    fn transaction_with_two_insertions_is_one_revision_and_one_undo_entry() {
        let mut d = Document::from_text("abc");
        d.apply_batch(vec![TextEdit::insert(1, "X"), TextEdit::insert(3, "Y")])
            .unwrap();

        assert_eq!(d.as_string(), "aXbcY");
        assert_eq!(d.revision(), Revision(1));

        d.undo().unwrap();
        assert_eq!(d.as_string(), "abc");
        assert_eq!(d.revision(), Revision(2));
        assert_eq!(d.undo(), Err(EditError::NothingToUndo));

        d.redo().unwrap();
        assert_eq!(d.as_string(), "aXbcY");
        assert_eq!(d.revision(), Revision(3));
    }

    #[test]
    fn late_invalid_edit_is_atomic_and_preserves_redo() {
        let mut d = Document::from_text("abc");
        d.apply(TextEdit::insert(1, "X")).unwrap();
        d.undo().unwrap();
        let before_revision = d.revision();

        // Kills mutant: apply the first edit before validating the second.
        // Kills mutant: transaction failure clears redo or changes revision.
        let error = d.apply_transaction(vec![TextEdit::insert(1, "Y"), TextEdit::insert(99, "Z")]);

        assert_eq!(
            error,
            Err(EditError::OutOfBounds {
                start: 99,
                end: 99,
                len: 3,
            })
        );
        assert_eq!(d.as_string(), "abc");
        assert_eq!(d.revision(), before_revision);
        d.redo().unwrap();
        assert_eq!(d.as_string(), "aXbc");
    }

    #[test]
    fn overlapping_edits_are_refused_without_mutation() {
        let mut d = Document::from_text("abcd");
        let error = d.apply_transaction(vec![TextEdit::replace(1..3, "X"), TextEdit::delete(2..4)]);

        // Kills mutant: accept overlap and invent an application order.
        assert_eq!(
            error,
            Err(EditError::OverlappingEdits {
                first: 0,
                first_start: 1,
                first_end: 3,
                second: 1,
                second_start: 2,
                second_end: 4,
            })
        );
        assert_eq!(d.as_string(), "abcd");
        assert_eq!(d.revision(), Revision(0));
    }

    #[test]
    fn position_map_accounts_for_multiple_edits_and_affinity() {
        let mut d = Document::from_text("012345");
        let result = d
            .apply_transaction(vec![TextEdit::insert(1, "XX"), TextEdit::insert(4, "Y")])
            .unwrap();

        assert_eq!(d.as_string(), "0XX123Y45");
        assert_eq!(result.map_position(Position::at(5)), Position::at(8));
        assert_eq!(
            result.map_position(Position {
                byte: 1,
                affinity: Affinity::Upstream,
            }),
            Position {
                byte: 1,
                affinity: Affinity::Upstream,
            }
        );
        assert_eq!(
            result.map_position(Position {
                byte: 1,
                affinity: Affinity::Downstream,
            }),
            Position {
                byte: 3,
                affinity: Affinity::Downstream,
            }
        );

        // Kills mutant: position mapping ignores an earlier insertion.
        assert_eq!(result.map_selection(Selection::at(5)), Selection::at(8));
    }

    #[test]
    fn position_map_collapses_deletion_and_replacement() {
        let mut d = Document::from_text("abcdef");
        let deletion = d.apply_transaction(vec![TextEdit::delete(1..4)]).unwrap();
        assert_eq!(deletion.map_position(Position::at(5)), Position::at(2));
        assert_eq!(
            deletion.map_position(Position {
                byte: 2,
                affinity: Affinity::Downstream,
            }),
            Position::at(1)
        );

        let mut d = Document::from_text("abcdef");
        let replacement = d
            .apply_transaction(vec![TextEdit::replace(1..4, "WXYZ")])
            .unwrap();
        assert_eq!(
            replacement.map_position(Position {
                byte: 2,
                affinity: Affinity::Upstream,
            }),
            Position {
                byte: 1,
                affinity: Affinity::Upstream,
            }
        );
        assert_eq!(
            replacement.map_position(Position {
                byte: 2,
                affinity: Affinity::Downstream,
            }),
            Position::at(5)
        );
        assert_eq!(replacement.map_position(Position::at(4)), Position::at(5));
    }

    #[test]
    fn invalid_utf8_boundary_is_rejected_atomically() {
        let mut d = Document::from_text("é");
        let error = d.apply_transaction(vec![TextEdit::insert(1, "x")]);

        assert_eq!(error, Err(EditError::NotCharBoundary { byte: 1 }));
        assert_eq!(d.as_string(), "é");
        assert_eq!(d.revision(), Revision(0));
    }

    #[test]
    fn grapheme_helpers_keep_crlf_together_after_transaction() {
        let mut d = Document::from_text("a\r\nb");
        d.apply_transaction(vec![TextEdit::insert(0, "x"), TextEdit::insert(4, "y")])
            .unwrap();

        assert_eq!(d.as_string(), "xa\r\nby");
        assert_eq!(d.next_grapheme_boundary(2).unwrap(), 4);
        assert_eq!(d.previous_grapheme_boundary(4).unwrap(), 2);
    }

    #[test]
    fn grapheme_helpers_keep_crlf_together() {
        let d = Document::from_text("a\r\nb");
        assert_eq!(d.next_grapheme_boundary(1).unwrap(), 3);
        assert_eq!(d.previous_grapheme_boundary(3).unwrap(), 1);
    }
}
