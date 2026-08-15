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
    #[error("line {line} is out of bounds for {lines} lines")]
    LineOutOfBounds { line: usize, lines: usize },
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
}

#[derive(Debug, Clone)]
struct Transaction {
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
        self.validate(&edit.range)?;
        let removed = self.slice(edit.range.clone())?;
        self.rope.replace(edit.range.clone(), &edit.replacement);
        self.undo.push(Transaction {
            range: edit.range,
            removed,
            inserted: edit.replacement,
        });
        self.redo.clear();
        self.revision = self.revision.next();
        Ok(self.revision)
    }
    pub fn apply_batch(&mut self, mut edits: Vec<TextEdit>) -> Result<Revision, EditError> {
        edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        for edit in edits {
            self.apply(edit)?;
        }
        Ok(self.revision)
    }
    pub fn undo(&mut self) -> Result<Revision, EditError> {
        let t = self.undo.pop().ok_or(EditError::NothingToUndo)?;
        let end = t.range.start + t.inserted.len();
        self.validate(&(t.range.start..end))?;
        self.rope.replace(t.range.start..end, &t.removed);
        self.redo.push(t);
        self.revision = self.revision.next();
        Ok(self.revision)
    }
    pub fn redo(&mut self) -> Result<Revision, EditError> {
        let t = self.redo.pop().ok_or(EditError::NothingToRedo)?;
        self.validate(&t.range)?;
        self.rope.replace(t.range.clone(), &t.inserted);
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
    fn grapheme_helpers_keep_crlf_together() {
        let d = Document::from_text("a\r\nb");
        assert_eq!(d.next_grapheme_boundary(1).unwrap(), 3);
        assert_eq!(d.previous_grapheme_boundary(3).unwrap(), 1);
    }
    #[test]
    fn two_caret_batch_is_descending() {
        let mut d = Document::from_text("abc");
        d.apply_batch(vec![TextEdit::insert(1, "X"), TextEdit::insert(3, "X")])
            .unwrap();
        assert_eq!(d.as_string(), "aXbcX");
    }
}
