//! Composing a shaping window with the shared font stack.
//!
//! This is the seam Phase 3 set out to discover, and it is three crates wide
//! on purpose:
//!
//! ```text
//! instar-text   which bytes: row 135,298 is 8_381_002..8_382_552
//! instar-host   slices those bytes out of the rope, asks for them to be
//!               shaped, and remembers where they came from
//! instar-ui     "shape this 1,550-byte string" -- and nothing else. It does
//!               not know a TextBuffer exists
//! ```
//!
//! `instar-ui -> instar-text` stays absent, held by a layering test, because
//! what the shaping side needed was never the semantic tree: it was the
//! `FontContext`, which Parley intends to be long-lived and roughly one per
//! application.
//!
//! # The thing this module exists to remember
//!
//! [`PresentedSegment::buffer_range`]. Parley's cursor indices are offsets
//! into the string a layout was built from, not into the document, so a window
//! that begins eight megabytes in makes every position wrong by exactly that
//! much unless something carries the origin.
//!
//! It is written and tested here before there is a caret, because the defect
//! it prevents is invisible at the top of a file: a fixture that never scrolls
//! passes with the origin dropped entirely.

use std::collections::HashMap;
use std::ops::Range;

use instar_paint::{Color, PaintCommand, PaintScene, PhysicalSize};
use instar_text::{ShapingWindow, TextStorage};
use instar_ui::{ShapingStyle, TextContext, TextLayout};

/// One shaped row of a document, and where in the document it came from.
pub struct PresentedSegment {
    /// The byte range of the buffer this was shaped from.
    ///
    /// Not merely diagnostic: `buffer_range.start` is the offset every Parley
    /// position has to be adjusted by.
    pub buffer_range: Range<usize>,
    /// The document row this is.
    pub row: usize,
    /// The paragraph was too long to shape whole and this is a bounded segment
    /// of it. Positions past its end are not addressable from this layout.
    pub truncated: bool,
    /// Where it sits in the view, in logical pixels.
    pub origin_x: f32,
    pub origin_y: f32,
    pub layout: TextLayout,
}

impl PresentedSegment {
    /// The offset in this layout's text of a document position.
    ///
    /// `None` when the position is not in this segment — including when it is
    /// inside the paragraph but past a truncation, which is a real answer
    /// rather than a near miss.
    pub fn buffer_to_local(&self, buffer: usize) -> Option<usize> {
        (self.buffer_range.start..=self.buffer_range.end)
            .contains(&buffer)
            .then(|| buffer - self.buffer_range.start)
    }

    /// The document position of an offset in this layout's text.
    ///
    /// Inclusive of the end, because a caret may sit after the last character
    /// of a segment.
    pub fn local_to_buffer(&self, local: usize) -> Option<usize> {
        (local <= self.buffer_range.len()).then(|| self.buffer_range.start + local)
    }
}

/// Everything one frame of one view draws.
pub struct PresentedText {
    pub segments: Vec<PresentedSegment>,
}

impl PresentedText {
    /// Total bytes handed to the shaper, and what `textbench` reports.
    pub fn bytes_shaped(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| segment.buffer_range.len())
            .sum()
    }

    /// Total glyphs the renderer will be asked to draw.
    pub fn glyphs(&mut self) -> usize {
        self.segments
            .iter_mut()
            .map(|segment| {
                segment
                    .layout
                    .shaped()
                    .runs
                    .iter()
                    .map(|run| run.glyphs.len())
                    .sum::<usize>()
            })
            .sum()
    }

    /// The segment containing a document position.
    pub fn segment_at(&self, buffer: usize) -> Option<&PresentedSegment> {
        self.segments
            .iter()
            .find(|segment| segment.buffer_to_local(buffer).is_some())
    }
}

/// Shapes exactly the window `instar-text` chose, and nothing else.
///
/// `text` is borrowed one segment at a time and never assembled into a single
/// string: a window is bounded, but concatenating twenty-five rows to shape
/// them together would be a contiguous copy for no reason, and would lose the
/// paragraph boundaries Parley wants anyway.
pub fn present(
    context: &mut TextContext,
    storage: &TextStorage,
    window: &ShapingWindow,
    style: ShapingStyle,
    row_height: f32,
    wrap_width: Option<f32>,
) -> Result<PresentedText, instar_text::TextError> {
    let mut segments = Vec::with_capacity(window.paragraphs.len());

    for paragraph in &window.paragraphs {
        // The trailing newline is not shaped: Parley would give it a cluster,
        // and a caret placed after it would land on a glyph that is not there.
        let bytes = without_trailing_newline(storage, paragraph.bytes.clone())?;
        let text = storage.slice(bytes.clone())?.materialize();

        let mut layout = context.shape_keyless(&text, style);
        layout.break_lines(wrap_width);

        segments.push(PresentedSegment {
            row: paragraph.row,
            truncated: paragraph.truncated,
            origin_x: 0.0,
            // Relative to the window's first row, so a view scrolled to row
            // 135,298 draws its first segment at the top of the viewport
            // rather than 2.7 million pixels below it.
            origin_y: (paragraph.row - window.rows.start) as f32 * row_height,
            buffer_range: bytes,
            layout,
        });
    }

    Ok(PresentedText { segments })
}

/// Lowers a presented window to drawing commands.
///
/// Reuses `present::push_shaped`, which is the same routine every `Text` node
/// in the semantic tree goes through — font deduplication by
/// `FontFace::key`, logical-to-physical scaling at the glyph, `Arc` font bytes
/// never copied. A second lowering path would be a second place for the scale
/// factor to be applied once too often.
///
/// Shaping stays in logical space at `scale = 1.0`; physicalization happens
/// here, at the glyph, because Vello selects bitmap and colour strikes from
/// the font size and a global transform would not be equivalent.
pub fn lower(
    presented: &mut PresentedText,
    size: PhysicalSize,
    scale: f32,
    background: Color,
    ink: Color,
) -> PaintScene {
    let mut commands = vec![PaintCommand::Clear { color: background }];
    let mut fonts = Vec::new();
    let mut font_ids = HashMap::new();

    for segment in &mut presented.segments {
        let origin = (segment.origin_x, segment.origin_y);
        crate::present::push_shaped(
            &mut commands,
            &mut fonts,
            &mut font_ids,
            segment.layout.shaped(),
            origin,
            scale,
            ink,
        );
    }

    PaintScene {
        size,
        commands,
        masks: Vec::new(),
        fonts,
        images: Vec::new(),
    }
}

/// Trims a paragraph's trailing `\n`, and the `\r` of a `\r\n` with it.
fn without_trailing_newline(
    storage: &TextStorage,
    range: Range<usize>,
) -> Result<Range<usize>, instar_text::TextError> {
    if range.is_empty() {
        return Ok(range);
    }
    let tail = storage.slice(range.start..range.end)?;
    let mut end = range.end;
    // At most two bytes, so this cannot become a scan.
    let text = tail.materialize();
    if text.ends_with('\n') {
        end -= 1;
        if text[..text.len() - 1].ends_with('\r') {
            end -= 1;
        }
    }
    Ok(range.start..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_text::TextViewport;

    fn document(rows: usize) -> TextStorage {
        TextStorage::from_text(&"the quick brown fox jumps over the lazy dog!!\n".repeat(rows))
    }

    fn present_at(storage: &TextStorage, scroll: i32) -> PresentedText {
        let viewport = TextViewport::new(400.0, 20.0);
        let window = viewport.visible(storage, scroll).expect("in bounds");
        let mut context = TextContext::new();
        present(
            &mut context,
            storage,
            &window,
            ShapingStyle::default(),
            20.0,
            None,
        )
        .expect("a window is always shapeable")
    }

    /// The claim B1 exists for, now with real glyphs behind it.
    #[test]
    fn shaping_tracks_the_viewport_rather_than_the_document() {
        let small = document(1_000);
        let large = document(100_000);

        let mut near = present_at(&small, 2_000);
        let mut far = present_at(&large, 2_000);

        assert_eq!(near.bytes_shaped(), far.bytes_shaped());
        assert_eq!(
            near.glyphs(),
            far.glyphs(),
            "a hundredfold document, the same screen, the same glyphs"
        );
        assert!(near.glyphs() > 0, "and glyphs were actually produced");
    }

    /// The origin, at a depth where dropping it is a defect rather than a
    /// rounding error.
    #[test]
    fn a_deep_segment_round_trips_between_local_and_buffer_offsets() {
        let storage = document(100_000);
        let presented = present_at(&storage, 1_900_000);

        let segment = &presented.segments[3];
        assert!(
            segment.buffer_range.start > 4_000_000,
            "the fixture has to be deep enough for the origin to matter: {:?}",
            segment.buffer_range
        );

        for local in [0usize, 7, segment.buffer_range.len()] {
            let buffer = segment.local_to_buffer(local).expect("inside the segment");
            assert_eq!(buffer, segment.buffer_range.start + local);
            assert_eq!(segment.buffer_to_local(buffer), Some(local));
        }

        assert_eq!(
            presented
                .segment_at(segment.buffer_range.start + 7)
                .map(|s| s.row),
            Some(segment.row)
        );
    }

    /// The reason the deep fixture above exists, stated as its own test.
    ///
    /// At the top of a document the first segment starts at byte 0, so
    /// `buffer_range.start` is zero and dropping it entirely changes nothing.
    /// This passes with the origin removed; the deep test does not. Every
    /// fixture in a suite starting at the top of a file would agree that a
    /// broken translation works.
    #[test]
    fn at_the_top_of_a_document_the_origin_is_zero_and_proves_nothing() {
        let storage = document(1_000);
        let presented = present_at(&storage, 0);

        let first = &presented.segments[0];
        assert_eq!(first.buffer_range.start, 0);
        assert_eq!(first.local_to_buffer(7), Some(7), "identity, not a mapping");
    }

    /// One long-lived font stack, not one per frame.
    ///
    /// Parley intends `FontContext` to be roughly one per application; a fresh
    /// one enumerates the system's faces. Counted rather than timed: the first
    /// version of this test compared twenty frames against one context
    /// construction, and it failed under an unrelated fault because a warm
    /// construction is fast enough that the margin was not real. A count has
    /// no margin to get wrong.
    #[test]
    fn the_font_stack_is_built_once_rather_than_per_frame() {
        let storage = document(1_000);
        let viewport = TextViewport::new(400.0, 20.0);
        let window = viewport.visible(&storage, 2_000).expect("in bounds");
        let mut context = TextContext::new();

        let before = TextContext::constructed_on_this_thread();
        for _ in 0..20 {
            present(
                &mut context,
                &storage,
                &window,
                ShapingStyle::default(),
                20.0,
                None,
            )
            .expect("a window is always shapeable");
        }

        assert_eq!(
            TextContext::constructed_on_this_thread(),
            before,
            "twenty frames built a font stack -- Parley's FontContext is meant \
             to be roughly one per application, and rebuilding it enumerates \
             every face on the system"
        );
    }

    /// A position above the window belongs to no segment, rather than to the
    /// first one.
    #[test]
    fn a_position_outside_the_window_has_no_segment() {
        let storage = document(100_000);
        let presented = present_at(&storage, 1_900_000);

        assert!(presented.segment_at(0).is_none());
        assert!(presented.segments[0].buffer_to_local(0).is_none());
    }

    /// The first visible row draws at the top of the viewport, not at its
    /// absolute position in a two-million-pixel document.
    #[test]
    fn origins_are_relative_to_the_window() {
        let storage = document(100_000);
        let presented = present_at(&storage, 1_900_000);

        assert_eq!(presented.segments[0].origin_y, 0.0);
        assert_eq!(presented.segments[1].origin_y, 20.0);
    }

    /// A trailing newline is not a glyph.
    #[test]
    fn a_paragraph_is_shaped_without_its_line_break() {
        let storage = TextStorage::from_text("ab\ncd\n");
        let presented = present_at(&storage, 0);

        assert_eq!(presented.segments[0].buffer_range, 0..2);
        assert_eq!(presented.segments[1].buffer_range, 3..5);
    }

    /// The enormous line, through the whole path.
    #[test]
    fn an_enormous_paragraph_is_shaped_only_up_to_its_cap() {
        let storage = TextStorage::from_text(&"x".repeat(5 * 1024 * 1024));
        let presented = present_at(&storage, 0);

        assert_eq!(presented.segments.len(), 1);
        assert!(presented.segments[0].truncated);
        assert_eq!(
            presented.bytes_shaped(),
            instar_text::MAX_SHAPED_PARAGRAPH_BYTES,
            "five megabytes on one row, and 64 KiB of it shaped"
        );
    }
}
