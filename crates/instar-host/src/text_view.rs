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
//! it prevents has a blind spot exactly one row wide. Row 0 starts at byte 0,
//! so adding the origin and forgetting to add it give the same answer there —
//! and nowhere else, not even elsewhere in an unscrolled view. A suite whose
//! only click fixture is the first row would agree that a broken translation
//! works.

use std::collections::HashMap;
use std::ops::Range;

use instar_paint::{Color, PaintCommand, PaintScene, PhysicalSize};
use instar_text::{Revision, ShapingWindow, TextStorage};
use instar_ui::{Affinity, CaretGeometry, ShapingStyle, TextContext, TextLayout};

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
    /// The buffer revision this was shaped from.
    ///
    /// Carried before anything caches, because it is what makes a stale
    /// segment detectable rather than merely unlikely. A `TextLayout` is
    /// presentation state; the moment one outlives the buffer state it was
    /// built from, hit-testing against it answers a question about a document
    /// that no longer exists.
    pub buffer_revision: Revision,
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

/// A caret position in a document.
///
/// Byte offset plus affinity, together, everywhere. Parley states that
/// affinity affects a cursor's visual location, so a document position that
/// dropped it would be a position that cannot be drawn back correctly — the
/// same byte sits in two different places at a bidi boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPosition {
    pub byte: usize,
    pub affinity: Affinity,
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

    /// A point in the view, as a position in the document.
    ///
    /// The whole coordinate path, in one place rather than scattered across
    /// callers:
    ///
    /// ```text
    /// viewport point
    ///   -> the segment whose rows contain it
    ///   -> segment-local point
    ///   -> TextLayout::cursor_from_point   (Parley: cluster, line, direction)
    ///   -> local byte + affinity
    ///   -> + buffer_range.start
    ///   -> document position
    /// ```
    ///
    /// Hit-testing goes through the layout that produced the visible glyphs.
    /// Shaping the same string again to answer a click would give the view two
    /// layouts that can disagree about where text is.
    pub fn position_at(&self, x: f32, y: f32, row_height: f32) -> Option<TextPosition> {
        let segment = self.segment_for_point(y, row_height)?;
        let cursor = segment
            .layout
            .cursor_from_point(x - segment.origin_x, y - segment.origin_y);
        Some(TextPosition {
            // `cursor_from_point` clamps into the layout, so this is always
            // inside the segment -- but it is translated through the same
            // helper the tests exercise rather than by adding the origin here.
            byte: segment.local_to_buffer(cursor.index)?,
            affinity: cursor.affinity,
        })
    }

    /// Where to draw the caret for a document position, in view coordinates.
    ///
    /// `None` when the position is not currently presented, which is the
    /// honest answer for a caret scrolled off screen.
    pub fn caret_geometry(&self, position: TextPosition, width: f32) -> Option<CaretGeometry> {
        let segment = self.segment_at(position.byte)?;
        let local = segment.buffer_to_local(position.byte)?;
        let cursor = segment
            .layout
            .cursor_from_byte_index(local, position.affinity);
        let geometry = segment.layout.caret_geometry(cursor, width);
        Some(CaretGeometry {
            x: geometry.x + segment.origin_x,
            y: geometry.y + segment.origin_y,
            ..geometry
        })
    }

    /// The segment a vertical position falls in, clamped to the window.
    ///
    /// Clamped rather than `None` outside, because a drag that leaves the top
    /// or bottom of the view should extend to the first or last visible
    /// position, not stop responding.
    fn segment_for_point(&self, y: f32, row_height: f32) -> Option<&PresentedSegment> {
        if self.segments.is_empty() {
            return None;
        }
        let row = (y / row_height).floor().max(0.0) as usize;
        Some(
            self.segments
                .get(row)
                .unwrap_or_else(|| self.segments.last().expect("checked non-empty")),
        )
    }
}

/// How a view is presented, as distinct from what it is showing.
///
/// Grouped rather than passed as loose parameters: `present` was at seven
/// arguments, which is clippy's limit, and B2 adds more. A parameter list that
/// long is also one where two `f32`s can be swapped silently.
#[derive(Debug, Clone, Copy)]
pub struct Presentation {
    pub style: ShapingStyle,
    /// Logical pixels per row.
    pub row_height: f32,
    /// Width to wrap at, or `None` for one line per paragraph.
    pub wrap_width: Option<f32>,
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
    presentation: &Presentation,
    buffer_revision: Revision,
) -> Result<PresentedText, instar_text::TextError> {
    let mut segments = Vec::with_capacity(window.paragraphs.len());

    for paragraph in &window.paragraphs {
        // The trailing newline is not shaped: Parley would give it a cluster,
        // and a caret placed after it would land on a glyph that is not there.
        let bytes = without_trailing_newline(storage, paragraph.bytes.clone())?;
        let text = storage.slice(bytes.clone())?.materialize();

        let mut layout = context.shape_keyless(&text, presentation.style);
        layout.break_lines(presentation.wrap_width);

        segments.push(PresentedSegment {
            row: paragraph.row,
            truncated: paragraph.truncated,
            origin_x: 0.0,
            // Relative to the window's first row, so a view scrolled to row
            // 135,298 draws its first segment at the top of the viewport
            // rather than 2.7 million pixels below it.
            origin_y: (paragraph.row - window.rows.start) as f32 * presentation.row_height,
            buffer_range: bytes,
            buffer_revision,
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
            &Presentation {
                style: ShapingStyle::default(),
                row_height: 20.0,
                wrap_width: None,
            },
            Revision::default(),
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
    /// The first segment starts at byte 0, so `buffer_range.start` is zero and
    /// dropping it entirely changes nothing *here*. This passes with the origin
    /// removed; the deep test does not.
    #[test]
    fn on_the_first_row_the_origin_is_zero_and_proves_nothing() {
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
                &Presentation {
                    style: ShapingStyle::default(),
                    row_height: 20.0,
                    wrap_width: None,
                },
                Revision::default(),
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

    // ------------------------------------------- B2a: the coordinate seam

    /// Presents an arbitrary document so a hit-test has something to land in.
    fn present_text(text: &str, scroll: i32) -> PresentedText {
        let storage = TextStorage::from_text(text);
        let viewport = TextViewport::new(400.0, 20.0);
        let window = viewport.visible(&storage, scroll).expect("in bounds");
        let mut context = TextContext::new();
        present(
            &mut context,
            &storage,
            &window,
            &Presentation {
                style: ShapingStyle::default(),
                row_height: 20.0,
                wrap_width: None,
            },
            Revision::default(),
        )
        .expect("a window is always shapeable")
    }

    /// Failure class 1: the window origin.
    ///
    /// A click twelve rows down a view scrolled to row 90,000 must produce a
    /// byte millions deep, not a byte twelve rows into the file.
    #[test]
    fn a_click_deep_in_a_document_produces_a_deep_buffer_position() {
        let storage = document(100_000);
        let presented = present_at(&storage, 1_900_000);
        let first_row = presented.segments[0].row;

        let position = presented
            .position_at(30.0, 12.0 * 20.0 + 5.0, 20.0)
            .expect("a point inside the window hits a row");

        assert!(
            position.byte > 4_000_000,
            "a click at row {} landed at byte {}, which is near the top of the \
             file -- the window origin was lost",
            first_row + 12,
            position.byte
        );
        let segment = presented
            .segment_at(position.byte)
            .expect("the position came from a presented row");
        assert_eq!(segment.row, first_row + 12);
    }

    /// The control that makes the previous test mean something — and it is
    /// narrower than "the top of the file".
    ///
    /// Only **row 0** has origin zero. Row 12 of an unscrolled view still
    /// starts several hundred bytes in, so a lost origin is detectable there.
    /// The blind spot is exactly one row wide, and this test occupies it: a
    /// suite whose only click fixture is the first row would agree that a
    /// broken translation works.
    #[test]
    fn a_click_on_the_first_row_cannot_detect_a_lost_origin() {
        let storage = document(1_000);
        let presented = present_at(&storage, 0);
        assert_eq!(presented.segments[0].buffer_range.start, 0);

        let position = presented
            .position_at(30.0, 5.0, 20.0)
            .expect("a point on the first row");

        assert!(
            position.byte < 50,
            "row 0 is the one place adding the origin and forgetting to add it \
             give the same answer"
        );
    }

    /// Failure class 2: UTF-8.
    ///
    /// Every position a click can produce has to be a byte a rope will accept.
    /// Landing inside a multi-byte character would be an error the moment the
    /// caret was used to edit.
    #[test]
    fn every_click_across_multibyte_text_lands_on_a_character_boundary() {
        let text = "héllo wörld — ünïcode\nそして日本語のテキスト\n";
        let storage = TextStorage::from_text(text);
        let presented = present_text(text, 0);

        for x in 0..200 {
            for row in 0..2 {
                let position = presented
                    .position_at(x as f32, row as f32 * 20.0 + 5.0, 20.0)
                    .expect("inside the window");
                assert!(
                    storage.is_char_boundary(position.byte),
                    "clicking at x={x} on row {row} produced byte {}, which is \
                     inside a character",
                    position.byte
                );
            }
        }
    }

    /// Failure class 3: affinity survives the translation.
    ///
    /// Parley resolves the same byte offset to different visual places
    /// depending on affinity. A document position that dropped it could not be
    /// drawn back correctly, so the round trip has to preserve it.
    #[test]
    fn affinity_is_carried_across_the_seam_rather_than_discarded() {
        let text = "abc \u{05d0}\u{05d1}\u{05d2} def\n";
        let presented = present_text(text, 0);
        let segment = &presented.segments[0];

        let boundary = segment.buffer_range.start + 4;
        let downstream = TextPosition {
            byte: boundary,
            affinity: Affinity::Downstream,
        };
        let upstream = TextPosition {
            byte: boundary,
            affinity: Affinity::Upstream,
        };

        let a = presented
            .caret_geometry(downstream, 1.0)
            .expect("presented");
        let b = presented.caret_geometry(upstream, 1.0).expect("presented");

        assert_ne!(
            (a.x, a.y),
            (b.x, b.y),
            "the same byte at a direction boundary drew in the same place for \
             both affinities, which means affinity is being dropped somewhere \
             between the position and the geometry"
        );
    }

    /// A caret round-trips: click, then ask where to draw it, and get back the
    /// row that was clicked.
    #[test]
    fn a_clicked_position_draws_its_caret_on_the_row_that_was_clicked() {
        let storage = document(100_000);
        let presented = present_at(&storage, 1_900_000);

        let position = presented
            .position_at(40.0, 5.0 * 20.0 + 5.0, 20.0)
            .expect("inside the window");
        let caret = presented
            .caret_geometry(position, 1.0)
            .expect("a presented position has geometry");

        assert_eq!(
            caret.y, 100.0,
            "row 5 of the window sits at y=100 in view coordinates"
        );
        assert!(caret.height > 0.0, "a caret with no height draws nothing");
    }

    /// A caret scrolled off screen has no geometry, rather than geometry at
    /// the nearest visible row.
    #[test]
    fn a_position_outside_the_window_has_no_caret_geometry() {
        let storage = document(100_000);
        let presented = present_at(&storage, 1_900_000);

        assert!(
            presented
                .caret_geometry(
                    TextPosition {
                        byte: 0,
                        affinity: Affinity::Downstream
                    },
                    1.0
                )
                .is_none(),
            "byte 0 is far above this window"
        );
    }
}
