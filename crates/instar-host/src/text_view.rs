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
use instar_text::{
    Revision, Selection, ShapingWindow, TextAffinity, TextPosition, TextStorage, TextViewId,
    TextViewport,
};
use instar_ui::{Affinity, CaretGeometry, ShapingStyle, TextContext, TextLayout};

/// A document position's affinity, as the shaping layer names it.
///
/// The only conversion between the two, and the reason `instar-ui` does not
/// depend on `instar-text` for an enum: `instar-text` owns document positions,
/// `instar-ui` owns the Parley-facing projection, and this function is the
/// seam. Three lines is a cheaper boundary than a crate dependency.
fn layout_affinity(affinity: TextAffinity) -> Affinity {
    match affinity {
        TextAffinity::Downstream => Affinity::Downstream,
        TextAffinity::Upstream => Affinity::Upstream,
    }
}

fn document_affinity(affinity: Affinity) -> TextAffinity {
    match affinity {
        Affinity::Downstream => TextAffinity::Downstream,
        Affinity::Upstream => TextAffinity::Upstream,
    }
}

/// Counters for the two questions B2 has to be able to answer.
///
/// Not an optimization, and not a promise of one. The target a caret move
/// should eventually reach is zero reshaping — but at ~200 µs for a whole
/// visible window there is no case for building a cache yet, and building one
/// before pointer and caret behaviour exist would mean inventing invalidation
/// semantics with no evidence about what invalidates them.
///
/// So: measure now, decide later. If B2c shows every pointer move reshaping
/// twenty-three rows, that is evidence. If it does not, the cache was never
/// worth its invalidation bugs.
pub mod instrument {
    use std::cell::Cell;

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Counts {
        /// Calls to [`super::PresentedText::caret_geometry`].
        pub caret_geometry_queries: u64,
        /// Calls to [`super::present`], each of which reshapes a whole window.
        pub presentation_reshapes: u64,
    }

    thread_local! {
        static COUNTS: Cell<Counts> = const {
            Cell::new(Counts {
                caret_geometry_queries: 0,
                presentation_reshapes: 0,
            })
        };
    }

    pub fn snapshot() -> Counts {
        COUNTS.with(|counts| counts.get())
    }

    pub fn reset() {
        COUNTS.with(|counts| counts.set(Counts::default()));
    }

    pub(super) fn record_caret_query() {
        COUNTS.with(|counts| {
            let mut current = counts.get();
            current.caret_geometry_queries += 1;
            counts.set(current);
        });
    }

    pub(super) fn record_reshape() {
        COUNTS.with(|counts| {
            let mut current = counts.get();
            current.presentation_reshapes += 1;
            counts.set(current);
        });
    }
}

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
            affinity: document_affinity(cursor.affinity),
        })
    }

    /// Where to draw the caret for a document position, in view coordinates.
    ///
    /// `None` when the position is not currently presented — the honest answer
    /// for a caret scrolled off screen — and `None` when the segment was shaped
    /// from a different `revision`.
    ///
    /// The revision check is what makes [`PresentedSegment::buffer_revision`]
    /// load-bearing rather than decorative. A caret positioned from geometry
    /// that describes text which has since changed is worse than no caret: it
    /// is confidently in the wrong place, and it will be at its most wrong
    /// exactly when an edit has just happened.
    pub fn caret_geometry(
        &self,
        position: TextPosition,
        width: f32,
        revision: Revision,
    ) -> Option<CaretGeometry> {
        instrument::record_caret_query();
        let segment = self.segment_at(position.byte)?;
        if segment.buffer_revision != revision {
            return None;
        }
        let local = segment.buffer_to_local(position.byte)?;
        let cursor = segment
            .layout
            .cursor_from_byte_index(local, layout_affinity(position.affinity));
        let geometry = segment.layout.caret_geometry(cursor, width);
        Some(CaretGeometry {
            x: geometry.x + segment.origin_x,
            y: geometry.y + segment.origin_y,
            ..geometry
        })
    }

    /// The rectangles a document selection covers within one segment.
    ///
    /// The projection, and the only place it happens:
    ///
    /// ```text
    /// absolute selection
    ///   -> intersect with this segment's buffer_range
    ///   -> translate both endpoints to layout-local offsets
    ///   -> a Parley Selection inside this one layout
    ///   -> geometry_with, straight into the callback
    /// ```
    ///
    /// Nothing is collected: the rectangles go to `f` as Parley produces them.
    pub fn selection_geometry_with(
        &self,
        segment: &PresentedSegment,
        selection: Selection,
        mut f: impl FnMut(CaretGeometry),
    ) {
        if selection.is_empty() {
            return;
        }
        let selected = selection.range();
        let start = selected.start.max(segment.buffer_range.start);
        let end = selected.end.min(segment.buffer_range.end);
        if start >= end {
            return;
        }

        let (Some(local_start), Some(local_end)) =
            (segment.buffer_to_local(start), segment.buffer_to_local(end))
        else {
            return;
        };

        // Affinity is taken from the real endpoints only where the selection
        // actually begins or ends in this segment. A row in the middle of a
        // multi-row drag is entered and left at its own boundaries, and
        // borrowing a distant endpoint's affinity there would be describing a
        // position that is not in this layout.
        let anchor_affinity = if start == selection.range().start {
            endpoint_affinity(&selection, start)
        } else {
            TextAffinity::Downstream
        };
        let focus_affinity = if end == selection.range().end {
            endpoint_affinity(&selection, end)
        } else {
            TextAffinity::Upstream
        };

        segment.layout.selection_geometry_with(
            instar_ui::TextCursor {
                index: local_start,
                affinity: layout_affinity(anchor_affinity),
            },
            instar_ui::TextCursor {
                index: local_end,
                affinity: layout_affinity(focus_affinity),
            },
            |geometry| {
                f(CaretGeometry {
                    x: geometry.x + segment.origin_x,
                    y: geometry.y + segment.origin_y,
                    ..geometry
                });
            },
        );
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
    instrument::record_reshape();
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

/// A pointer drag that belongs to one text view until it ends.
///
/// # Logical capture, not OS capture
///
/// Once a drag begins, pointer movement inside the window belongs to the
/// originating view until release or cancellation — even if the pointer
/// crosses another view. That is the Phase 2 rule for scrollbar thumbs,
/// unchanged.
///
/// Deliberately **not** `Window::set_cursor_grab`. Winit's grab modes are
/// materially platform-dependent — `Confined` is unsupported on macOS,
/// `Locked` on X11 — so enforcing capture through the OS would make identical
/// code behave differently per platform. Host-side transient state is
/// deterministic everywhere, and it is the same shape of state the scroll
/// subsystem already keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextDrag {
    view: TextViewId,
    anchor: TextPosition,
    /// The revision the presentation was built from when the drag began.
    revision: Revision,
}

/// The host's transient text-pointer state: at most one drag at a time.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextInteraction {
    drag: Option<TextDrag>,
}

impl TextInteraction {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The view that owns the current drag, if any.
    pub fn captured_view(&self) -> Option<TextViewId> {
        self.drag.map(|drag| drag.view)
    }

    /// A press: collapses the selection and takes capture.
    pub fn press(
        &mut self,
        view: TextViewId,
        position: TextPosition,
        revision: Revision,
    ) -> Selection {
        self.drag = Some(TextDrag {
            view,
            anchor: position,
            revision,
        });
        Selection::from_position(position)
    }

    /// A pointer move.
    ///
    /// `over` is the view the pointer is currently above, and is deliberately
    /// *not* used to decide which view is being edited: a drag begun in view A
    /// keeps extending A's selection while the pointer passes over B. Returns
    /// `None` when no drag is active.
    pub fn drag_to(&mut self, position: TextPosition, revision: Revision) -> Option<Selection> {
        let drag = self.drag?;
        // An edit during a drag retires it. The presented segments the drag is
        // producing positions from describe text that has changed, and
        // transforming a live capture across an edit is a synchronization
        // problem that does not need solving before keyboard input exists.
        if drag.revision != revision {
            self.drag = None;
            return None;
        }
        Some(Selection {
            anchor: drag.anchor,
            head: position,
        })
    }

    /// The pointer came up. Capture ends; the selection stands.
    pub fn release(&mut self) {
        self.drag = None;
    }

    /// The drag is over and produced nothing further.
    ///
    /// Focus loss, the cursor leaving the window, the view being retired: the
    /// same lifecycle events Phase 2 already cancels a scrollbar drag on. No
    /// new special case, and no outside-window autoscroll — that is an editor
    /// behaviour to add when an application asks for it.
    pub fn cancel(&mut self) {
        self.drag = None;
    }

    /// Cancels a drag that belongs to a view which is no longer live.
    pub fn retire_view(&mut self, view: TextViewId) {
        if self.drag.is_some_and(|drag| drag.view == view) {
            self.drag = None;
        }
    }
}

/// Which endpoint of a selection sits at `byte`, and with what affinity.
fn endpoint_affinity(selection: &Selection, byte: usize) -> TextAffinity {
    if selection.anchor.byte == byte {
        selection.anchor.affinity
    } else {
        selection.head.affinity
    }
}

/// The caret's width, in logical pixels.
///
/// Host chrome policy, like the focus ring and the scrollbar thumb — not wire
/// vocabulary. A guest describes an editor; it does not describe how wide the
/// insertion point is on this machine. Logical because everything in
/// `instar-ui` is: physicalization happens once, here, at lowering.
pub const CARET_WIDTH: f32 = 1.0;

/// One frame of one text view.
///
/// Grouped rather than passed loose for the same reason [`Presentation`] is:
/// this is already eight values, several of them `f32`s and `Color`s that
/// would be silently swappable.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub surface: PhysicalSize,
    pub scale: f32,
    /// The view's logical size. Glyphs *and* caret are clipped to it, so a
    /// caret belonging to a row scrolled out of view cannot leak into the
    /// frame — one coordinate system and one clip stack, not an independent
    /// caret path.
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub background: Color,
    pub ink: Color,
    pub caret_color: Color,
    /// Where the caret is, when the view has one.
    pub caret: Option<TextPosition>,
    /// What is selected, when anything is.
    pub selection: Option<Selection>,
    pub selection_color: Color,
    /// The buffer revision this frame is drawing. A segment shaped from a
    /// different revision cannot position a caret: its geometry describes text
    /// that has since changed.
    pub revision: Revision,
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
pub fn lower(presented: &mut PresentedText, frame: &Frame) -> PaintScene {
    let mut commands = vec![
        PaintCommand::Clear {
            color: frame.background,
        },
        // Everything the view draws lives inside this. The caret is emitted
        // between the same push and pop as the glyphs, because a caret with
        // its own clip is a second coordinate system waiting to disagree.
        PaintCommand::PushClip {
            rect: physical(
                0.0,
                0.0,
                frame.viewport_width,
                frame.viewport_height,
                frame.scale,
            ),
        },
    ];
    // Selection first, so it sits *behind* the glyphs. The order here is the
    // whole of the focus-ring lesson: a command that exists is not a thing
    // that is visible, and a highlight painted over its own text is a
    // highlight that hides it.
    if let Some(selection) = frame.selection {
        let scale = frame.scale;
        let colour = frame.selection_color;
        let mut rects = Vec::new();
        for segment in &presented.segments {
            presented.selection_geometry_with(segment, selection, |geometry| {
                rects.push(physical(
                    geometry.x,
                    geometry.y,
                    geometry.width,
                    geometry.height,
                    scale,
                ));
            });
        }
        commands.extend(rects.into_iter().map(|rect| PaintCommand::FillRect {
            rect,
            color: colour,
        }));
    }

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
            frame.scale,
            frame.ink,
        );
    }

    // After the glyphs, so a caret inside a glyph's ink is visible rather than
    // painted under it. The focus ring cost this project a package by being
    // emitted first and then covered.
    if let Some(position) = frame.caret
        && let Some(geometry) = presented.caret_geometry(position, CARET_WIDTH, frame.revision)
    {
        commands.push(PaintCommand::FillRect {
            rect: physical(
                geometry.x,
                geometry.y,
                geometry.width,
                geometry.height,
                frame.scale,
            ),
            color: frame.caret_color,
        });
    }

    commands.push(PaintCommand::PopClip);

    PaintScene {
        size: frame.surface,
        commands,
        masks: Vec::new(),
        fonts,
        images: Vec::new(),
    }
}

/// A logical rectangle in physical pixels, never narrower than one.
///
/// A one-logical-pixel caret at a fractional scale can round to zero width,
/// which draws nothing at all — the caret disappears on some displays and not
/// others, which is exactly the class of defect that is hardest to reproduce.
fn physical(x: f32, y: f32, width: f32, height: f32, scale: f32) -> instar_paint::Rect {
    instar_paint::Rect {
        x: (x * scale).round() as i32,
        y: (y * scale).round() as i32,
        width: (width * scale).round().max(1.0) as u32,
        height: (height * scale).round().max(1.0) as u32,
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
        let downstream = TextPosition::with_affinity(boundary, TextAffinity::Downstream);
        let upstream = TextPosition::with_affinity(boundary, TextAffinity::Upstream);

        let a = presented
            .caret_geometry(downstream, 1.0, Revision::default())
            .expect("presented");
        let b = presented
            .caret_geometry(upstream, 1.0, Revision::default())
            .expect("presented");

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
            .caret_geometry(position, 1.0, Revision::default())
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
                .caret_geometry(TextPosition::at(0), 1.0, Revision::default())
                .is_none(),
            "byte 0 is far above this window"
        );
    }

    // -------------------------------------------------- B2b: the caret

    /// Parley is authoritative for which cursor states exist inside a layout.
    ///
    /// `from_byte_index` snaps to a cluster start, forces `Downstream` at byte
    /// 0 because there is no upstream cluster there, and resolves anything past
    /// the end to the layout end with `Upstream`. So the invariant Instar can
    /// claim is *not* that an arbitrary `(byte, affinity)` pair survives — it is
    /// that Instar preserves affinity across its own seam and lets Parley
    /// normalize inside the layout.
    #[test]
    fn a_caret_has_geometry_at_every_edge_of_a_segment() {
        let text = "hello world\nsecond line\n";
        let presented = present_text(text, 0);
        let segment = &presented.segments[0];

        for (label, byte) in [
            ("document start", segment.buffer_range.start),
            ("mid-text", segment.buffer_range.start + 5),
            ("segment end", segment.buffer_range.end),
        ] {
            for affinity in [TextAffinity::Downstream, TextAffinity::Upstream] {
                let geometry = presented
                    .caret_geometry(
                        TextPosition::with_affinity(byte, affinity),
                        CARET_WIDTH,
                        Revision::default(),
                    )
                    .unwrap_or_else(|| panic!("{label} should have caret geometry"));
                assert!(
                    geometry.height > 0.0,
                    "{label} produced a caret with no height, which draws nothing"
                );
                assert!(geometry.x >= 0.0, "{label} produced a negative x");
            }
        }
    }

    /// A caret advances along a line rather than sitting at its start.
    #[test]
    fn a_caret_moves_right_as_its_byte_offset_grows() {
        let presented = present_text("hello world\n", 0);
        let start = presented.segments[0].buffer_range.start;

        let first = presented
            .caret_geometry(TextPosition::at(start), CARET_WIDTH, Revision::default())
            .expect("presented");
        let later = presented
            .caret_geometry(
                TextPosition::at(start + 8),
                CARET_WIDTH,
                Revision::default(),
            )
            .expect("presented");

        assert!(
            later.x > first.x,
            "eight characters in, the caret is still at x={}",
            later.x
        );
    }

    /// `buffer_revision` made load-bearing.
    #[test]
    fn a_stale_segment_cannot_position_a_caret() {
        let presented = present_text("hello world\n", 0);
        let position = TextPosition::at(3);

        assert!(
            presented
                .caret_geometry(position, CARET_WIDTH, Revision::default())
                .is_some(),
            "the revision it was shaped from works"
        );
        assert!(
            presented
                .caret_geometry(position, CARET_WIDTH, Revision::default().next())
                .is_none(),
            "a caret positioned from geometry describing text that has since \
             changed is worse than no caret -- it is confidently wrong, exactly \
             when an edit has just happened"
        );
    }

    /// Moving a caret inside a presented window does not reshape it.
    ///
    /// Measured rather than required: the point is that B2 can answer the
    /// question, not that the answer is already zero.
    #[test]
    fn moving_a_caret_within_a_window_queries_geometry_without_reshaping() {
        let presented = present_text("hello world\nsecond line\n", 0);

        instrument::reset();
        for byte in 0..10 {
            presented.caret_geometry(TextPosition::at(byte), CARET_WIDTH, Revision::default());
        }

        let counts = instrument::snapshot();
        assert_eq!(counts.caret_geometry_queries, 10);
        assert_eq!(
            counts.presentation_reshapes, 0,
            "ten caret moves reshaped the window {} times",
            counts.presentation_reshapes
        );
    }

    /// And the counter is not vacuous: presenting does count as a reshape.
    #[test]
    fn presenting_a_window_counts_as_a_reshape() {
        instrument::reset();
        let _ = present_text("hello\n", 0);
        assert_eq!(instrument::snapshot().presentation_reshapes, 1);
    }

    // ------------------------------------------- B2c: selection and capture

    fn view(id: u32) -> TextViewId {
        TextViewId { id, generation: 0 }
    }

    fn at(byte: usize) -> TextPosition {
        TextPosition::at(byte)
    }

    /// A drag begun in one view keeps extending that view's selection while
    /// the pointer is somewhere else entirely.
    #[test]
    fn a_drag_belongs_to_the_view_it_started_in() {
        let mut interaction = TextInteraction::new();
        let a = view(1);
        let b = view(2);

        interaction.press(a, at(10), Revision::default());
        assert_eq!(interaction.captured_view(), Some(a));

        // The pointer is now over b. The selection is still a's.
        let selection = interaction
            .drag_to(at(40), Revision::default())
            .expect("a drag in progress");
        assert_eq!(selection.anchor, at(10));
        assert_eq!(selection.head, at(40));
        assert_eq!(
            interaction.captured_view(),
            Some(a),
            "crossing view {b:?} handed the drag over, which is not what \
             capture means"
        );
    }

    /// Direction survives, because dragging backwards is a different gesture
    /// from dragging forwards even when the bytes match.
    #[test]
    fn a_reverse_drag_selects_the_same_bytes_and_keeps_its_direction() {
        let mut interaction = TextInteraction::new();
        interaction.press(view(1), at(40), Revision::default());
        let backwards = interaction
            .drag_to(at(10), Revision::default())
            .expect("dragging");

        assert_eq!(
            backwards.range(),
            10..40,
            "the same bytes as a forward drag"
        );
        assert_eq!(backwards.anchor, at(40), "but the anchor is where it began");
        assert_eq!(backwards.head, at(10));
        assert_ne!(
            backwards,
            Selection {
                anchor: at(10),
                head: at(40)
            },
            "canonicalizing to min..max would lose which end is moving"
        );
    }

    /// Every lifecycle event Phase 2 cancels a scrollbar drag on.
    #[test]
    fn a_drag_is_cancelled_by_the_lifecycle_rather_than_by_a_special_case() {
        let mut interaction = TextInteraction::new();

        // Focus loss / cursor left.
        interaction.press(view(1), at(10), Revision::default());
        interaction.cancel();
        assert!(!interaction.is_dragging());
        assert!(interaction.drag_to(at(20), Revision::default()).is_none());

        // The view goes away.
        interaction.press(view(1), at(10), Revision::default());
        interaction.retire_view(view(2));
        assert!(
            interaction.is_dragging(),
            "another view's retirement is not ours"
        );
        interaction.retire_view(view(1));
        assert!(!interaction.is_dragging());

        // Release.
        interaction.press(view(1), at(10), Revision::default());
        interaction.release();
        assert!(!interaction.is_dragging());
    }

    /// An edit during a drag retires it.
    #[test]
    fn an_edit_during_a_drag_retires_the_capture() {
        let mut interaction = TextInteraction::new();
        interaction.press(view(1), at(10), Revision::default());

        assert!(
            interaction
                .drag_to(at(20), Revision::default().next())
                .is_none(),
            "the presented segments this drag reads positions from describe \
             text that has changed"
        );
        assert!(
            !interaction.is_dragging(),
            "and the drag is gone rather than merely refused once"
        );
    }

    /// The projection, on the case that distinguishes it from painting whole
    /// rows: a selection starting midway through one row and ending midway
    /// through another.
    #[test]
    fn a_cross_row_selection_projects_onto_each_row_it_touches() {
        let presented = present_text("aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc\ndddddddddd\n", 0);
        let rows: Vec<_> = presented
            .segments
            .iter()
            .map(|s| s.buffer_range.clone())
            .collect();
        assert!(
            rows.len() >= 4,
            "the fixture needs a row past the selection to prove it is left \
             alone, and crop does not count a trailing newline as a line"
        );

        // Start five characters into row 0, end five into row 2.
        let selection = Selection {
            anchor: at(rows[0].start + 5),
            head: at(rows[2].start + 5),
        };

        let mut per_row = Vec::new();
        for segment in &presented.segments {
            let mut widest: f32 = 0.0;
            let mut any = false;
            presented.selection_geometry_with(segment, selection, |g| {
                any = true;
                widest = widest.max(g.width);
            });
            per_row.push((segment.row, any, widest));
        }

        assert!(per_row[0].1, "row 0 is partly selected");
        assert!(per_row[1].1, "row 1 is entirely selected");
        assert!(per_row[2].1, "row 2 is partly selected");
        assert!(
            !per_row[3].1,
            "row 3 is past the selection and must not be highlighted"
        );
        assert!(
            per_row[1].2 > per_row[0].2 && per_row[1].2 > per_row[2].2,
            "the middle row is fully covered and the first and last are not: \
             widths {:?} -- an implementation that highlighted every touched \
             row whole would make these equal",
            per_row.iter().map(|r| r.2).collect::<Vec<_>>()
        );
    }

    /// A collapsed selection paints nothing at all.
    #[test]
    fn a_collapsed_selection_has_no_geometry() {
        let presented = present_text("hello world\n", 0);
        let mut rects = 0;
        presented.selection_geometry_with(
            &presented.segments[0],
            Selection::from_position(at(3)),
            |_| rects += 1,
        );
        assert_eq!(rects, 0, "a caret is not a highlight");
    }

    /// The deep case, for the same reason every other seam test has one.
    #[test]
    fn a_deep_selection_projects_through_the_window_origin() {
        let storage = document(100_000);
        let presented = present_at(&storage, 1_900_000);
        let first = presented.segments[0].buffer_range.clone();
        let third = presented.segments[2].buffer_range.clone();
        assert!(first.start > 4_000_000, "deep enough to matter");

        let selection = Selection {
            anchor: at(first.start + 3),
            head: at(third.start + 3),
        };

        let mut rows_highlighted = 0;
        for segment in &presented.segments {
            let mut any = false;
            presented.selection_geometry_with(segment, selection, |_| any = true);
            if any {
                rows_highlighted += 1;
            }
        }
        assert_eq!(
            rows_highlighted, 3,
            "three rows are touched by a selection spanning rows 0..2"
        );
    }

    /// The synthetic row, through the whole presentation path.
    ///
    /// `crop` reports zero lines for an empty rope, so before B2c's fix this
    /// produced no segments at all and a caret in a new document had nowhere
    /// to be drawn — which B3 would have hit on its very first keystroke.
    #[test]
    fn an_empty_document_can_still_hold_a_caret() {
        let presented = present_text("", 0);

        assert_eq!(presented.segments.len(), 1, "one row to put a caret in");
        assert_eq!(presented.bytes_shaped(), 0, "and no text in it");

        let caret = presented
            .caret_geometry(TextPosition::at(0), CARET_WIDTH, Revision::default())
            .expect("a caret at byte 0 of an empty document has geometry");
        assert!(caret.height > 0.0, "and a line box to occupy");
        assert_eq!(caret.y, 0.0);
    }
}

// ------------------------------------------------- the host text-input seam

/// A live text view the host is presenting, and everything a pointer needs to
/// reach it.
///
/// # What this is not, yet
///
/// It is **not** attached to the semantic UI tree. No wire vocabulary declares
/// an editor surface, so nothing a guest commits can produce one of these.
/// That is deliberate: `NODE_TEXT_VIEW` would drag in who creates a
/// `TextViewId`, how a guest obtains one, whether removing a node detaches or
/// destroys the view, whether it destroys the *buffer* (it must not), and
/// whether focus identity is a `NodeKey` or a `TextViewId`. That is guest-
/// facing architecture, not plumbing, and package B2e is where it gets decided.
///
/// What this *is* is the real route a future attachment will feed. The pointer
/// path below is production code that B2e will call with a view it looked up
/// from a node, rather than test-only editor logic that would then have to be
/// written twice.
pub struct HostTextSurface {
    pub view: TextViewId,
    pub viewport: TextViewport,
    /// Where the surface sits in the window, in logical pixels.
    pub origin_x: f32,
    pub origin_y: f32,
    pub presentation: PresentedText,
    /// The revision `presentation` was built from.
    pub revision: Revision,
}

/// What a pointer event did to a text surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextPointerOutcome {
    /// Nothing here: the event was not for this surface, or there was no drag.
    Ignored,
    /// The selection changed and the view needs repainting.
    SelectionChanged(Selection),
    /// A drag ended, or was cancelled by the lifecycle.
    CaptureReleased,
}

/// Routes one translated window event to a text surface.
///
/// The whole seam, and deliberately the *only* way a pointer reaches text:
///
/// ```text
/// winit::WindowEvent
///   -> instar-window::winit_adapter::translate
///   -> WindowOutput
///   -> here
///   -> instar-text::Selection
/// ```
///
/// Every lifecycle event that ends a drag is handled by the same rules Phase 2
/// established for a scrollbar thumb: `PointerLeft` and losing window focus
/// cancel, a release ends it, and nothing else is a special case.
pub fn handle_pointer(
    interaction: &mut TextInteraction,
    surface: &HostTextSurface,
    output: &instar_window::WindowOutput,
) -> TextPointerOutcome {
    use instar_window::{PointerState, WindowOutput};

    match output {
        WindowOutput::Pointer(event) if event.state == PointerState::Pressed => {
            let Some(position) = surface.position_at(event.logical_pos) else {
                return TextPointerOutcome::Ignored;
            };
            let selection = interaction.press(surface.view, position, surface.revision);
            TextPointerOutcome::SelectionChanged(selection)
        }
        WindowOutput::Pointer(event) if event.state == PointerState::Released => {
            if interaction.is_dragging() {
                interaction.release();
                TextPointerOutcome::CaptureReleased
            } else {
                TextPointerOutcome::Ignored
            }
        }
        WindowOutput::PointerMoved(event) => {
            // Deliberately does not check whether the pointer is still over
            // this surface. Capture means the drag owns the view until it
            // ends, and a selection that stopped extending when the pointer
            // wandered would be a drag that gives up halfway.
            let Some(position) = surface.position_at(event.logical_pos) else {
                return TextPointerOutcome::Ignored;
            };
            match interaction.drag_to(position, surface.revision) {
                Some(selection) => TextPointerOutcome::SelectionChanged(selection),
                None => TextPointerOutcome::Ignored,
            }
        }
        WindowOutput::PointerLeft { .. } => {
            if interaction.is_dragging() {
                interaction.cancel();
                TextPointerOutcome::CaptureReleased
            } else {
                TextPointerOutcome::Ignored
            }
        }
        WindowOutput::WindowFocusChanged { focused: false, .. } => {
            if interaction.is_dragging() {
                interaction.cancel();
                TextPointerOutcome::CaptureReleased
            } else {
                TextPointerOutcome::Ignored
            }
        }
        _ => TextPointerOutcome::Ignored,
    }
}

impl HostTextSurface {
    /// A window-space point as a document position, or `None` when the point
    /// is outside this surface.
    fn position_at(&self, point: instar_window::LogicalPoint) -> Option<TextPosition> {
        let x = point.x as f32 - self.origin_x;
        let y = point.y as f32 - self.origin_y;
        if x < 0.0 || y < 0.0 || y > self.viewport.height {
            return None;
        }
        self.presentation
            .position_at(x, y, self.viewport.row_height)
    }
}
