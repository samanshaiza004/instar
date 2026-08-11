//! The text itself, and the only place a rope appears.
//!
//! # Why this wrapper exists at all
//!
//! Two reasons, and the second is the load-bearing one.
//!
//! **It is replaceable.** `crop` is a good fit for Instar — a B-tree rope
//! whose editing API speaks byte offsets, which is what the revision protocol
//! carries — but it is a choice made before `textbench` has said anything. No
//! `crop` type appears in this crate's public API, so replacing it is a change
//! to this file rather than to `TextBuffer`.
//!
//! **It is fallible, and `crop` is not.** `Rope::replace` and `Rope::byte_slice`
//! panic on an out-of-bounds range and on an offset that is not a UTF-8
//! code-point boundary. That is a reasonable contract for a rope and the wrong
//! one at a resource boundary: these offsets will eventually arrive from a
//! guest, and a panic there takes the host down with it. So every method here
//! validates before delegating, and `crop` never sees an argument it would
//! panic on.
//!
//! # The invariant this file is guarding
//!
//! > No latency-sensitive operation may require materializing the entire
//! > buffer contiguously.
//!
//! A rope does not prevent `buffer.to_string()` before every render or parse;
//! it only makes that mistake avoidable. Nothing here returns an owned `String`
//! of the whole document, and the one method that could — [`TextStorage::slice`]
//! — hands back a borrowed view. `textbench` measures whether the rest of the
//! system takes the offer.

use std::ops::Range;

use crate::TextError;

/// Instar's text storage: a rope, behind an API that cannot panic.
#[derive(Debug, Clone, Default)]
pub struct TextStorage {
    rope: crop::Rope,
}

/// A borrowed region of a [`TextStorage`].
///
/// Deliberately not a `String`. A slice of a 10 MB document is a view into it,
/// and the moment this owned bytes it would become the easiest way to write the
/// mistake this crate exists to avoid.
#[derive(Debug, Clone, Copy)]
pub struct TextSlice<'a> {
    slice: crop::RopeSlice<'a>,
}

impl<'a> TextSlice<'a> {
    pub fn len_bytes(&self) -> usize {
        self.slice.byte_len()
    }

    pub fn is_empty(&self) -> bool {
        self.len_bytes() == 0
    }

    /// The region in chunks, never as one allocation.
    pub fn chunks(&self) -> impl Iterator<Item = &'a str> {
        self.slice.chunks()
    }

    /// The region as an owned `String`.
    ///
    /// Named for what it costs. Every caller is a place `textbench` should be
    /// able to find, which is why this is not `to_string` or `Display`.
    pub fn materialize(&self) -> String {
        self.slice.chunks().collect()
    }
}

impl TextStorage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            rope: crop::Rope::from(text),
        }
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.byte_len()
    }

    pub fn is_empty(&self) -> bool {
        self.len_bytes() == 0
    }

    /// Total lines, counting a trailing newline's empty last line.
    pub fn len_lines(&self) -> usize {
        self.rope.line_len()
    }

    /// Replaces a byte range with `text`.
    ///
    /// The whole of the validation this crate needs is here: a range that is
    /// out of bounds, inverted, or lands mid-character is refused and the
    /// storage is untouched. `crop` would panic on all three.
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Result<(), TextError> {
        self.check(range.clone())?;
        self.rope.replace(range, text);
        Ok(())
    }

    /// A borrowed view of a byte range.
    pub fn slice(&self, range: Range<usize>) -> Result<TextSlice<'_>, TextError> {
        self.check(range.clone())?;
        Ok(TextSlice {
            slice: self.rope.byte_slice(range),
        })
    }

    /// A byte range in chunks, never as one allocation.
    pub fn chunks(&self, range: Range<usize>) -> Result<impl Iterator<Item = &str>, TextError> {
        self.check(range.clone())?;
        Ok(self.rope.byte_slice(range).chunks())
    }

    /// The byte offset a line starts at.
    pub fn byte_of_line(&self, line: usize) -> Result<usize, TextError> {
        if line > self.rope.line_len() {
            return Err(TextError::LineOutOfBounds {
                line,
                lines: self.rope.line_len(),
            });
        }
        Ok(self.rope.byte_of_line(line))
    }

    /// The line a byte offset falls on.
    pub fn line_of_byte(&self, byte: usize) -> Result<usize, TextError> {
        self.check(byte..byte)?;
        Ok(self.rope.line_of_byte(byte))
    }

    /// Every way a byte range can be unusable, checked before `crop` sees it.
    fn check(&self, range: Range<usize>) -> Result<(), TextError> {
        let len = self.len_bytes();
        if range.start > range.end {
            return Err(TextError::InvertedRange {
                start: range.start,
                end: range.end,
            });
        }
        if range.end > len {
            return Err(TextError::RangeOutOfBounds {
                start: range.start,
                end: range.end,
                len,
            });
        }
        for offset in [range.start, range.end] {
            if !self.rope.is_char_boundary(offset) {
                return Err(TextError::NotACharBoundary { byte: offset });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_a_byte_range() {
        let mut storage = TextStorage::from_text("hello world");
        storage.replace(6..11, "there").expect("in bounds");
        assert_eq!(
            storage.slice(0..storage.len_bytes()).unwrap().materialize(),
            "hello there"
        );
    }

    /// The three arguments `crop` would panic on, refused instead.
    ///
    /// A panic here is not a crash in a rope; it is a crash in the host, on
    /// offsets that will eventually be derived from a guest.
    #[test]
    fn a_bad_range_is_an_error_and_changes_nothing() {
        let mut storage = TextStorage::from_text("héllo");
        let before = storage.len_bytes();

        assert!(matches!(
            storage.replace(0..99, "x"),
            Err(TextError::RangeOutOfBounds { .. })
        ));
        // Built rather than written literally: clippy correctly points out
        // that `4..2` is an empty range, and the point here is that an
        // *inverted* one is refused rather than silently treated as empty.
        let inverted = std::ops::Range { start: 4, end: 2 };
        assert!(matches!(
            storage.replace(inverted, "x"),
            Err(TextError::InvertedRange { .. })
        ));
        // 'é' occupies bytes 1..3, so 2 is inside it.
        assert!(matches!(
            storage.replace(2..3, "x"),
            Err(TextError::NotACharBoundary { byte: 2 })
        ));
        assert!(matches!(
            storage.slice(2..3),
            Err(TextError::NotACharBoundary { byte: 2 })
        ));

        assert_eq!(
            storage.len_bytes(),
            before,
            "a refused edit leaves the storage exactly as it was"
        );
    }

    #[test]
    fn line_and_byte_convert_both_ways() {
        let storage = TextStorage::from_text("one\ntwo\nthree");
        assert_eq!(storage.byte_of_line(0).unwrap(), 0);
        assert_eq!(storage.byte_of_line(1).unwrap(), 4);
        assert_eq!(storage.byte_of_line(2).unwrap(), 8);
        assert_eq!(storage.line_of_byte(0).unwrap(), 0);
        assert_eq!(storage.line_of_byte(5).unwrap(), 1);
        assert_eq!(storage.line_of_byte(9).unwrap(), 2);

        assert!(matches!(
            storage.byte_of_line(99),
            Err(TextError::LineOutOfBounds { .. })
        ));
        assert!(matches!(
            storage.line_of_byte(99),
            Err(TextError::RangeOutOfBounds { .. })
        ));
    }

    /// One line five megabytes long, which is the case that finds a line index
    /// quietly assuming ordinary lines.
    #[test]
    fn a_single_enormous_line_still_indexes() {
        let line = "x".repeat(5 * 1024 * 1024);
        let storage = TextStorage::from_text(&line);
        assert_eq!(storage.len_lines(), 1);
        assert_eq!(storage.byte_of_line(0).unwrap(), 0);
        assert_eq!(storage.line_of_byte(4 * 1024 * 1024).unwrap(), 0);
        assert_eq!(storage.slice(10..20).unwrap().len_bytes(), 10);
    }

    /// A slice of a large document is a view, not a copy.
    #[test]
    fn a_slice_borrows_rather_than_materializing() {
        let storage = TextStorage::from_text(&"abcdefghij".repeat(100_000));
        let slice = storage.slice(500_000..500_010).expect("in bounds");
        assert_eq!(slice.len_bytes(), 10);
        assert!(
            slice.chunks().map(str::len).sum::<usize>() == 10,
            "chunks cover the slice and nothing else"
        );
    }
}
