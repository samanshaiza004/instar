//! Which bytes a view actually has to shape.
//!
//! # The invariant this file exists for
//!
//! > A `TextView` may not shape the whole document merely because its
//! > `TextBuffer` is large.
//!
//! Package A's invariant was about materializing; this is its presentation
//! twin, and the one a rope does nothing to protect. Shaping is downstream of
//! storage, so a view that hands Parley everything between two newlines has
//! defeated the rope without ever calling `materialize`.
//!
//! Nothing here shapes anything or knows what a glyph is. It answers one
//! question — *which bytes* — and the answer is the number `textbench`
//! measures. Keeping it separate from the shaping call is deliberate: the
//! window is the architectural claim, and it can be tested against a rope with
//! no font stack anywhere near it.
//!
//! # Scope: uniform rows, no wrapping
//!
//! B1 assumes one paragraph occupies one row of a fixed height. That is not a
//! simplification for its own sake, it is where the cheap answer stops being
//! available:
//!
//! ```text
//! unwrapped   row N starts at byte_of_line(N)         O(log n) from the rope
//! wrapped     row N depends on how every paragraph
//!             before it broke, which depends on shaping them
//! ```
//!
//! Wrapping needs an incrementally maintained row index — every editor that
//! wraps large documents has one — and building that before a caret has ever
//! moved would be inventing machinery for a feature nothing has asked for. The
//! boundary is stated rather than hidden: [`TextViewport::visible`] takes a row
//! height and assumes it applies to every row.
//!
//! # Enormous paragraphs
//!
//! A rope holds five megabytes on one line without complaint. A view still
//! destroys the architecture by treating that line as one indivisible
//! paragraph, and it would look correct on every other fixture.
//!
//! So a paragraph longer than [`MAX_SHAPED_PARAGRAPH_BYTES`] contributes a
//! bounded segment instead of all of itself, and says so. What this file does
//! *not* do is pretend to know which part of such a line is horizontally
//! visible: that needs the advance width of everything before it, which is
//! O(bytes before it) however the shaping is chunked. The segment starts at the
//! paragraph start, and [`ParagraphWindow::truncated`] is how a caller finds
//! out it is not the whole story.

use std::ops::Range;

use crate::{TextError, TextStorage};

/// How much of one paragraph may be shaped.
///
/// Far above any prose or code line, because the approximation this bound
/// forces — shaping context does not cross the cut, so a ligature or a bidi run
/// spanning it renders as though the text ended there — should apply only where
/// the alternative is shaping megabytes to draw eighty columns. A 64 KiB line
/// is already not something anyone wrote by hand.
pub const MAX_SHAPED_PARAGRAPH_BYTES: usize = 64 * 1024;

/// What a view is looking at.
///
/// Rows rather than pixels for the overscan, because the whole mapping is row
/// arithmetic and expressing the margin in pixels would mean converting it back
/// immediately.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextViewport {
    /// Visible height in logical pixels.
    pub height: f32,
    /// The height of one row, in logical pixels.
    pub row_height: f32,
    /// Rows shaped beyond each edge, so a scroll does not shape at the moment
    /// it is least affordable.
    pub overscan_rows: usize,
}

impl TextViewport {
    pub fn new(height: f32, row_height: f32) -> Self {
        Self {
            height,
            row_height,
            overscan_rows: 2,
        }
    }

    /// The rows a scroll offset puts on screen, plus overscan.
    ///
    /// Clamped to the document, so the range is always one a storage can
    /// answer for.
    pub fn rows(&self, scroll_y: i32, total_rows: usize) -> Range<usize> {
        if self.row_height <= 0.0 || total_rows == 0 {
            return 0..0;
        }
        let top = (scroll_y.max(0) as f32 / self.row_height).floor() as usize;
        let visible = (self.height / self.row_height).ceil() as usize + 1;

        let first = top.saturating_sub(self.overscan_rows);
        let last = top
            .saturating_add(visible)
            .saturating_add(self.overscan_rows)
            .min(total_rows);
        first.min(total_rows)..last
    }

    /// Every byte a view has to shape, and nothing else.
    ///
    /// The cost is `O(rows * log n)` in the document: one rope lookup per
    /// visible row boundary, and no walk of anything before the viewport.
    pub fn visible(
        &self,
        storage: &TextStorage,
        scroll_y: i32,
    ) -> Result<ShapingWindow, TextError> {
        let total_rows = storage.len_lines();

        // An empty document has *zero* lines in `crop`, not one empty line —
        // and that is the upstream contract, not a bug to work around in
        // storage. But an editor still needs somewhere to put a caret before
        // anything has been typed, so presentation supplies one row that the
        // document does not have.
        //
        // Deliberately not a newline invented in the buffer: the synthetic row
        // is a line box, not text. Its range is `0..0`, so a caret in it is at
        // byte 0 of a zero-byte document, which is exactly true.
        if total_rows == 0 {
            return Ok(ShapingWindow {
                rows: 0..1,
                paragraphs: vec![ParagraphWindow {
                    row: 0,
                    bytes: 0..0,
                    truncated: false,
                }],
            });
        }

        let rows = self.rows(scroll_y, total_rows);

        let mut paragraphs = Vec::with_capacity(rows.len());
        for row in rows.clone() {
            let start = storage.byte_of_line(row)?;
            // The last row runs to the end of the document rather than to the
            // start of a row that does not exist.
            let end = if row + 1 < total_rows {
                storage.byte_of_line(row + 1)?
            } else {
                storage.len_bytes()
            };

            let (bytes, truncated) = bound(storage, start..end)?;
            paragraphs.push(ParagraphWindow {
                row,
                bytes,
                truncated,
            });
        }

        Ok(ShapingWindow { rows, paragraphs })
    }
}

/// Caps a paragraph at [`MAX_SHAPED_PARAGRAPH_BYTES`], on a character boundary.
///
/// Landing mid-character would hand a shaper an invalid `&str`, so the cut
/// walks back to a boundary. It walks back at most three bytes — UTF-8's
/// longest sequence is four — so this cannot become a scan.
fn bound(storage: &TextStorage, range: Range<usize>) -> Result<(Range<usize>, bool), TextError> {
    if range.end - range.start <= MAX_SHAPED_PARAGRAPH_BYTES {
        return Ok((range, false));
    }
    let mut end = range.start + MAX_SHAPED_PARAGRAPH_BYTES;
    while end > range.start && !storage.is_char_boundary(end) {
        end -= 1;
    }
    Ok((range.start..end, true))
}

/// The bytes one frame of one view has to shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapingWindow {
    /// The rows this covers, including overscan.
    pub rows: Range<usize>,
    /// One entry per row, in row order.
    ///
    /// Per paragraph rather than one flat byte range, because a hard line break
    /// is where shaping restarts anyway — and because an enormous paragraph
    /// contributes a segment while the rows after it contribute all of
    /// themselves, which no single range can express.
    pub paragraphs: Vec<ParagraphWindow>,
}

impl ShapingWindow {
    /// What this window costs, and the number `textbench` reports.
    pub fn bytes_shaped(&self) -> usize {
        self.paragraphs
            .iter()
            .map(|paragraph| paragraph.bytes.end - paragraph.bytes.start)
            .sum()
    }

    /// Whether any paragraph was too long to shape whole.
    pub fn any_truncated(&self) -> bool {
        self.paragraphs.iter().any(|paragraph| paragraph.truncated)
    }

    /// The paragraph containing a buffer offset, if this window covers it.
    ///
    /// The translation seam. Parley indexes into the text it was given, so a
    /// caret at a buffer offset has to become an offset within one paragraph's
    /// shaped text before it means anything to a layout — and back again
    /// afterwards. Getting this wrong produces a caret that lands correctly
    /// until the view is scrolled, which is exactly the defect a unit suite
    /// full of unscrolled fixtures would not find.
    pub fn paragraph_at(&self, byte: usize) -> Option<&ParagraphWindow> {
        self.paragraphs
            .iter()
            .find(|paragraph| paragraph.bytes.contains(&byte))
    }
}

/// One row's contribution to a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParagraphWindow {
    pub row: usize,
    /// The byte range of this paragraph that will be shaped, including its
    /// trailing newline if it has one.
    pub bytes: Range<usize>,
    /// The paragraph was longer than [`MAX_SHAPED_PARAGRAPH_BYTES`] and this is
    /// a bounded segment of it starting at the paragraph's start.
    pub truncated: bool,
}

impl ParagraphWindow {
    /// A buffer offset as an offset into this paragraph's shaped text.
    pub fn local(&self, byte: usize) -> Option<usize> {
        self.bytes.contains(&byte).then(|| byte - self.bytes.start)
    }

    /// An offset within this paragraph's shaped text as a buffer offset.
    pub fn buffer(&self, local: usize) -> Option<usize> {
        (local <= self.bytes.end - self.bytes.start).then(|| self.bytes.start + local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64-byte rows, so row N starts at byte 64N and the arithmetic is
    /// checkable by hand.
    fn document(rows: usize) -> TextStorage {
        let line = "the quick brown fox jumps over the lazy dog, and again, twice\n";
        assert_eq!(line.len(), 62);
        TextStorage::from_text(&line.repeat(rows))
    }

    /// The claim, at the only scale that can distinguish it from a document
    /// scan: a tenfold document with the same viewport shapes the same bytes.
    #[test]
    fn work_tracks_the_viewport_rather_than_the_document() {
        let viewport = TextViewport::new(400.0, 20.0);

        let small = document(1_000);
        let large = document(100_000);
        assert!(large.len_bytes() > 6_000_000, "a document worth the claim");

        let near = viewport.visible(&small, 0).expect("in bounds");
        let far = viewport.visible(&large, 0).expect("in bounds");
        assert_eq!(
            near.bytes_shaped(),
            far.bytes_shaped(),
            "a hundredfold document, the same viewport, the same work"
        );
        assert!(
            near.bytes_shaped() < 2_000,
            "and the work is viewport-sized: {} bytes",
            near.bytes_shaped()
        );
    }

    /// Scrolling to the far end of a large document is not more expensive than
    /// sitting near the start of it.
    ///
    /// Both positions are interior. The very top is deliberately not one of
    /// them: it has no rows above it to overscan, so its window is legitimately
    /// two rows smaller, and comparing against it would be measuring the clamp
    /// rather than the depth.
    #[test]
    fn scrolling_deep_does_not_shape_what_was_scrolled_past() {
        let storage = document(100_000);
        let viewport = TextViewport::new(400.0, 20.0);

        let near = viewport.visible(&storage, 2_000).expect("in bounds");
        let deep = viewport.visible(&storage, 1_900_000).expect("in bounds");

        assert_eq!(
            near.bytes_shaped(),
            deep.bytes_shaped(),
            "row 100 and row 95,000 cost the same, or something walked there"
        );
        assert!(
            deep.rows.start > 90_000,
            "the deep window really is deep: {:?}",
            deep.rows
        );
        assert!(
            deep.paragraphs[0].bytes.start > 5_000_000,
            "and it starts millions of bytes in, without having walked there"
        );
    }

    /// The top of a document is the one place a window is smaller, and it is
    /// the clamp rather than a defect.
    #[test]
    fn the_top_of_a_document_has_nothing_above_it_to_overscan() {
        let storage = document(1_000);
        let viewport = TextViewport::new(400.0, 20.0);

        let top = viewport.visible(&storage, 0).expect("in bounds");
        let interior = viewport.visible(&storage, 2_000).expect("in bounds");

        assert_eq!(top.rows.start, 0);
        assert_eq!(
            interior.rows.len() - top.rows.len(),
            viewport.overscan_rows,
            "exactly the overscan that has nowhere to go"
        );
    }

    /// The case the whole segmentation policy exists for.
    #[test]
    fn an_enormous_paragraph_contributes_a_bounded_segment() {
        let storage = TextStorage::from_text(&"x".repeat(5 * 1024 * 1024));
        let viewport = TextViewport::new(400.0, 20.0);

        let window = viewport.visible(&storage, 0).expect("in bounds");

        assert_eq!(window.paragraphs.len(), 1, "five megabytes on one row");
        assert_eq!(
            window.bytes_shaped(),
            MAX_SHAPED_PARAGRAPH_BYTES,
            "the whole point: a bounded segment, not five megabytes"
        );
        assert!(
            window.any_truncated(),
            "and the caller is told it is not the whole paragraph, rather than \
             being handed a silent lie"
        );
    }

    /// The cut cannot land inside a character, or the shaper is handed bytes
    /// that are not a string.
    #[test]
    fn a_segment_ends_on_a_character_boundary() {
        // Three-byte characters do not divide the 64 KiB cap evenly, so the
        // naive cut lands mid-character.
        let storage = TextStorage::from_text(&"あ".repeat(100_000));
        let viewport = TextViewport::new(400.0, 20.0);

        let window = viewport.visible(&storage, 0).expect("in bounds");
        let segment = &window.paragraphs[0];

        assert!(segment.truncated);
        assert!(
            segment.bytes.end < MAX_SHAPED_PARAGRAPH_BYTES,
            "the cut moved back off the character it landed inside"
        );
        assert_eq!(segment.bytes.end % 3, 0, "and back to a whole character");
        storage
            .slice(segment.bytes.clone())
            .expect("a segment is always sliceable, which is the point");
    }

    /// The translation seam, in both directions.
    #[test]
    fn a_buffer_offset_and_a_paragraph_offset_convert_both_ways() {
        let storage = document(100_000);
        let viewport = TextViewport::new(400.0, 20.0);
        let window = viewport.visible(&storage, 1_900_000).expect("in bounds");

        let paragraph = &window.paragraphs[3];
        let byte = paragraph.bytes.start + 10;

        assert_eq!(
            paragraph.local(byte),
            Some(10),
            "a caret deep in a document is ten bytes into its own row"
        );
        assert_eq!(paragraph.buffer(10), Some(byte));
        assert_eq!(
            window.paragraph_at(byte).map(|p| p.row),
            Some(paragraph.row)
        );
    }

    /// A position outside the window has no paragraph, rather than the nearest
    /// one.
    #[test]
    fn a_position_outside_the_window_is_not_silently_reassigned() {
        let storage = document(100_000);
        let viewport = TextViewport::new(400.0, 20.0);
        let window = viewport.visible(&storage, 1_900_000).expect("in bounds");

        assert_eq!(
            window.paragraph_at(0),
            None,
            "byte 0 is far above this window, and answering with its first row \
             would put a caret on screen that belongs thousands of rows up"
        );
        assert_eq!(window.paragraphs[0].local(0), None);
    }

    /// An empty document presents one row, so a caret has somewhere to be.
    ///
    /// `crop` reports zero lines for an empty rope, which is its contract and
    /// not something to correct in storage. The row is supplied here, in
    /// presentation, and it is a line box rather than text: nothing invents a
    /// newline, and its range is `0..0`.
    #[test]
    fn an_empty_document_presents_one_row_to_put_a_caret_in() {
        let storage = TextStorage::new();
        let window = TextViewport::new(400.0, 20.0)
            .visible(&storage, 0)
            .expect("in bounds");

        assert_eq!(window.bytes_shaped(), 0, "there is no text to shape");
        assert_eq!(
            window.paragraphs.len(),
            1,
            "but there is a row, or a caret in an empty document has nowhere \
             to be drawn"
        );
        assert_eq!(window.paragraphs[0].bytes, 0..0);
        assert_eq!(window.paragraph_at(0).map(|p| p.row), None);
        assert!(!window.any_truncated());
    }

    /// The three transitions around the synthetic row.
    #[test]
    fn the_synthetic_row_appears_and_disappears_with_the_document() {
        let viewport = TextViewport::new(400.0, 20.0);

        let empty = TextStorage::new();
        assert_eq!(viewport.visible(&empty, 0).unwrap().paragraphs.len(), 1);

        // Type the first character: an ordinary row replaces the synthetic one.
        let typed = TextStorage::from_text("a");
        let window = viewport.visible(&typed, 0).unwrap();
        assert_eq!(window.paragraphs.len(), 1);
        assert_eq!(
            window.paragraphs[0].bytes,
            0..1,
            "the row is now real, and carries the byte that was typed"
        );

        // Delete it again: the synthetic row returns.
        let emptied = TextStorage::from_text("");
        assert_eq!(
            viewport.visible(&emptied, 0).unwrap().paragraphs[0].bytes,
            0..0
        );
    }

    /// Scrolling past the end clamps rather than asking the rope for a row it
    /// does not have.
    #[test]
    fn scrolling_past_the_end_clamps() {
        let storage = document(10);
        let window = TextViewport::new(400.0, 20.0)
            .visible(&storage, 100_000)
            .expect("a clamped window is still a valid one");
        assert!(window.rows.end <= storage.len_lines());
    }
}
