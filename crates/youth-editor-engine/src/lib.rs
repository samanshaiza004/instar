//! Unicode-correct, cursor-and-selection-aware text editing and layout,
//! built on Parley's [`parley::PlainEditor`].
//!
//! This crate is the sole place in the Youth workspace that depends on
//! `parley` (and transitively `fontique`/`skrifa`/etc). `youth-runtime` and
//! `youth-desktop` interact with editing only through the [`EditorEngine`]
//! and [`EditorLayout`] traits and the plain data types here, so neither
//! gains a direct Parley dependency.

#![forbid(unsafe_code)]

use std::ops::Range;

use parley::{Cursor, FontContext, LayoutContext, PlainEditor};

/// Movement granularity for cursor and selection operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Movement {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    WordLeft,
    WordRight,
}

/// A cursor-and-selection-aware snapshot of the live buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineSnapshot {
    pub text: String,
    /// Byte offset of the collapsed cursor / selection focus.
    pub cursor: usize,
    /// Byte range of the current selection, if not collapsed.
    pub selection: Option<Range<usize>>,
}

/// A rectangle in layout-local coordinates (logical pixels).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayoutRect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl From<parley::BoundingBox> for LayoutRect {
    fn from(value: parley::BoundingBox) -> Self {
        Self {
            x0: value.x0,
            y0: value.y0,
            x1: value.x1,
            y1: value.y1,
        }
    }
}

/// One glyph positioned within a [`GlyphRun`], in the run's font's units at
/// `GlyphRun::font_size`. `x`/`y` are the pen (baseline) position, absolute
/// within the layout's coordinate space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    pub id: u32,
    pub x: f32,
    pub y: f32,
}

/// A run of glyphs sharing one font, size, and color, ready for a
/// rasterizer to consume. `font` is a cheaply-`Clone`-able, `Send + Sync`
/// shared handle to the font's bytes (a
/// [`parley::FontData`]/`linebender_resource_handle::FontData`, re-exported
/// here so downstream crates never need to depend on `parley` themselves
/// just to hold this type) -- rasterization crates should key their glyph
/// cache on `font.data.id()` rather than hashing font bytes.
#[derive(Clone, Debug)]
pub struct GlyphRun {
    pub font: FontHandle,
    pub font_size: f32,
    pub glyphs: Vec<PositionedGlyph>,
}

/// A shared, `Send + Sync` handle to one font's bytes plus its collection
/// index, with a stable identity (`.data.id()`) suitable for cache keys.
pub type FontHandle = parley::FontData;

/// Enough structured layout data for a renderer to paint text, selection,
/// and a cursor -- glyph positions rather than raw text, so no renderer
/// needs its own text-shaping logic. Produced by [`EditorLayout::presentation`].
#[derive(Clone, Debug, Default)]
pub struct TextPresentation {
    pub runs: Vec<GlyphRun>,
    pub selection: Vec<LayoutRect>,
    pub cursor: Option<LayoutRect>,
    pub content_width: f32,
    pub content_height: f32,
    /// A rectangle bounding the area a platform IME candidate window should
    /// avoid covering. Meaningful regardless of whether composition is
    /// currently in progress, so a host can pass it to
    /// `Window::set_ime_cursor_area` as soon as an Editor gains focus.
    pub ime_cursor_area: LayoutRect,
}

/// The editing seam: Unicode/grapheme-correct insertion, deletion, and
/// cursor/selection movement. Does not require a viewport.
pub trait EditorEngine {
    /// Returns the current text, cursor, and selection state.
    fn snapshot(&mut self) -> EngineSnapshot;

    /// Replaces the whole buffer and collapses the cursor to its start.
    ///
    /// This is the mechanism behind an authoritative `replace`: the prior
    /// cursor/selection position is not meaningful against unrelated text.
    fn set_text(&mut self, text: &str);

    /// Inserts `text` at the cursor, replacing the selection if any.
    fn insert(&mut self, text: &str);

    /// Deletes the selection, or one Unicode-safe unit before the cursor
    /// (always along a char boundary; hard line breaks and font-shaped
    /// ligature clusters are deleted as one step, other multi-scalar
    /// sequences may take multiple steps depending on shaping data).
    fn backspace(&mut self);

    /// Deletes the selection, or one Unicode-safe unit after the cursor,
    /// with the same cluster caveats as [`Self::backspace`].
    fn delete_forward(&mut self);

    /// Moves the cursor (collapsing any selection) by `movement`.
    fn move_cursor(&mut self, movement: Movement);

    /// Moves the selection focus by `movement`, extending the selection.
    fn extend_selection(&mut self, movement: Movement);

    /// Selects the entire buffer.
    fn select_all(&mut self);

    /// Collapses the selection to its focus, leaving a plain caret.
    fn collapse_selection(&mut self);

    /// Moves the collapsed cursor to an exact byte offset.
    ///
    /// No-op if `index` is not a char boundary. This is the primitive
    /// undo/redo restoration is built on -- restoring exactly where the
    /// cursor was before an edit is not expressible through the movement
    /// API alone.
    fn move_to_byte(&mut self, index: usize);

    /// Selects an exact byte range, with the focus (and so `snapshot`'s
    /// reported cursor) at `end`.
    ///
    /// No-op if either bound is not a char boundary.
    fn select_byte_range(&mut self, start: usize, end: usize);

    /// Sets or replaces the IME preedit (composing) text at the cursor,
    /// starting composition if not already in progress. `cursor` is a
    /// byte range relative to `text`'s start; `None` hides the cursor
    /// during composition.
    ///
    /// The preedit text is provisionally part of the buffer (so it's
    /// visible and participates in layout) but is not a committed edit --
    /// callers should not treat repeated calls to this method as separate
    /// undoable operations.
    fn ime_set_compose(&mut self, text: &str, cursor: Option<(usize, usize)>);

    /// Cancels IME composition, removing the preedit text entirely and
    /// restoring the cursor to where composition started. A no-op if not
    /// currently composing.
    fn ime_clear_compose(&mut self);

    /// Commits the current IME preedit text as ordinary buffer content,
    /// ending composition without changing what's in the buffer. A no-op
    /// if not currently composing.
    fn ime_finish_compose(&mut self);

    /// Whether an IME composition is currently in progress.
    fn is_composing(&self) -> bool;
}

/// The layout/presentation seam: viewport-aware geometry, only meaningful
/// once a real (or synthetic/virtual) viewport width is known.
pub trait EditorLayout {
    /// Sets the wrapping width. `None` means unconstrained (single line
    /// grows without wrapping).
    fn set_width(&mut self, width: Option<f32>);

    /// Maps a point to the nearest text byte offset, without moving the
    /// live cursor.
    fn hit_test(&mut self, x: f32, y: f32) -> usize;

    /// The number of laid-out visual lines (after wrapping).
    fn line_count(&mut self) -> usize;

    /// The rectangles covering the current selection, one per visual line
    /// it spans.
    fn selection_geometry(&mut self) -> Vec<LayoutRect>;

    /// The rectangle for the current caret, if a cursor should be shown.
    fn cursor_geometry(&mut self) -> Option<LayoutRect>;

    /// A rectangle bounding the area a platform IME candidate window should
    /// avoid covering.
    fn ime_cursor_area(&mut self) -> LayoutRect;

    /// The current layout as glyph runs plus selection/cursor geometry,
    /// ready for a rasterizer. Does not include text a renderer would need
    /// to re-shape.
    fn presentation(&mut self) -> TextPresentation;
}

/// The real, Parley-backed implementation of both [`EditorEngine`] and
/// [`EditorLayout`].
///
/// `Clone`s duplicate the font/shaping scratch space along with the buffer;
/// Parley's own documentation notes `FontContext`/`LayoutContext` are
/// intended as one-per-application/thread shared resources, so cloning is
/// correct but not free. This is acceptable for the moderate number of
/// concurrent Editor sessions Gate A targets; sharing a single font/layout
/// context across sessions is a future optimization, not a correctness
/// requirement.
#[derive(Clone)]
pub struct ParleyEditorEngine {
    editor: PlainEditor<()>,
    font_cx: FontContext,
    layout_cx: LayoutContext<()>,
}

impl std::fmt::Debug for ParleyEditorEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParleyEditorEngine").finish_non_exhaustive()
    }
}

const DEFAULT_FONT_SIZE: f32 = 16.0;
const DEFAULT_CURSOR_WIDTH: f32 = 1.5;

impl ParleyEditorEngine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            editor: PlainEditor::new(DEFAULT_FONT_SIZE),
            font_cx: FontContext::new(),
            layout_cx: LayoutContext::new(),
        }
    }

    #[must_use]
    pub fn with_text(text: &str) -> Self {
        let mut engine = Self::new();
        EditorEngine::set_text(&mut engine, text);
        engine
    }

    fn driver(&mut self) -> parley::PlainEditorDriver<'_, ()> {
        self.editor.driver(&mut self.font_cx, &mut self.layout_cx)
    }
}

impl Default for ParleyEditorEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorEngine for ParleyEditorEngine {
    fn snapshot(&mut self) -> EngineSnapshot {
        self.driver().refresh_layout();
        let text = self.editor.text().to_string();
        let selection = self.editor.raw_selection();
        let range = selection.text_range();
        EngineSnapshot {
            text,
            cursor: selection.focus().index(),
            selection: (!selection.is_collapsed()).then_some(range),
        }
    }

    fn set_text(&mut self, text: &str) {
        self.editor.set_text(text);
        // `set_text` alone leaves the previous selection's byte offsets in
        // place, which are meaningless against unrelated authoritative
        // content. Explicitly collapse to a defined, valid position.
        self.driver().move_to_text_start();
    }

    fn insert(&mut self, text: &str) {
        self.driver().insert_or_replace_selection(text);
    }

    fn backspace(&mut self) {
        self.driver().backdelete();
    }

    fn delete_forward(&mut self) {
        self.driver().delete();
    }

    fn move_cursor(&mut self, movement: Movement) {
        let mut driver = self.driver();
        match movement {
            Movement::Left => driver.move_left(),
            Movement::Right => driver.move_right(),
            Movement::Up => driver.move_up(),
            Movement::Down => driver.move_down(),
            Movement::Home => driver.move_to_line_start(),
            Movement::End => driver.move_to_line_end(),
            Movement::WordLeft => driver.move_word_left(),
            Movement::WordRight => driver.move_word_right(),
        }
    }

    fn extend_selection(&mut self, movement: Movement) {
        let mut driver = self.driver();
        match movement {
            Movement::Left => driver.select_left(),
            Movement::Right => driver.select_right(),
            Movement::Up => driver.select_up(),
            Movement::Down => driver.select_down(),
            Movement::Home => driver.select_to_line_start(),
            Movement::End => driver.select_to_line_end(),
            Movement::WordLeft => driver.select_word_left(),
            Movement::WordRight => driver.select_word_right(),
        }
    }

    fn select_all(&mut self) {
        self.driver().select_all();
    }

    fn collapse_selection(&mut self) {
        self.driver().collapse_selection();
    }

    fn move_to_byte(&mut self, index: usize) {
        self.driver().move_to_byte(index);
    }

    fn select_byte_range(&mut self, start: usize, end: usize) {
        self.driver().select_byte_range(start, end);
    }

    fn ime_set_compose(&mut self, text: &str, cursor: Option<(usize, usize)>) {
        if text.is_empty() {
            // Parley's `set_compose` asserts non-empty text; an empty
            // preedit update is winit's way of signaling the preedit was
            // cleared (see `Ime::Preedit`'s documentation), so route it to
            // clear instead.
            self.ime_clear_compose();
            return;
        }
        self.driver().set_compose(text, cursor);
    }

    fn ime_clear_compose(&mut self) {
        self.driver().clear_compose();
    }

    fn ime_finish_compose(&mut self) {
        self.driver().finish_compose();
    }

    fn is_composing(&self) -> bool {
        self.editor.is_composing()
    }
}

impl EditorLayout for ParleyEditorEngine {
    fn set_width(&mut self, width: Option<f32>) {
        self.editor.set_width(width);
    }

    fn hit_test(&mut self, x: f32, y: f32) -> usize {
        let layout = self.editor.layout(&mut self.font_cx, &mut self.layout_cx);
        Cursor::from_point(layout, x, y).index()
    }

    fn line_count(&mut self) -> usize {
        self.editor
            .layout(&mut self.font_cx, &mut self.layout_cx)
            .lines()
            .count()
    }

    fn selection_geometry(&mut self) -> Vec<LayoutRect> {
        self.driver().refresh_layout();
        self.editor
            .selection_geometry()
            .into_iter()
            .map(|(rect, _line)| rect.into())
            .collect()
    }

    fn cursor_geometry(&mut self) -> Option<LayoutRect> {
        self.driver().refresh_layout();
        self.editor
            .cursor_geometry(DEFAULT_CURSOR_WIDTH)
            .map(Into::into)
    }

    fn ime_cursor_area(&mut self) -> LayoutRect {
        self.driver().refresh_layout();
        self.editor.ime_cursor_area().into()
    }

    fn presentation(&mut self) -> TextPresentation {
        let selection = self
            .editor
            .selection_geometry()
            .into_iter()
            .map(|(rect, _line)| rect.into())
            .collect();
        let cursor = self
            .editor
            .cursor_geometry(DEFAULT_CURSOR_WIDTH)
            .map(Into::into);
        let ime_cursor_area = self.editor.ime_cursor_area().into();
        let layout = self.editor.layout(&mut self.font_cx, &mut self.layout_cx);
        let mut runs = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let font = glyph_run.run().font().clone();
                let font_size = glyph_run.run().font_size();
                let glyphs = glyph_run
                    .positioned_glyphs()
                    .map(|glyph| PositionedGlyph {
                        id: glyph.id,
                        x: glyph.x,
                        y: glyph.y,
                    })
                    .collect();
                runs.push(GlyphRun {
                    font,
                    font_size,
                    glyphs,
                });
            }
        }
        TextPresentation {
            runs,
            selection,
            cursor,
            content_width: layout.width(),
            content_height: layout.height(),
            ime_cursor_area,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_snapshot_round_trip() {
        let mut engine = ParleyEditorEngine::with_text("hello");
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.text, "hello");
        assert_eq!(snapshot.cursor, 0, "set_text collapses to the start");
        assert_eq!(snapshot.selection, None);
    }

    #[test]
    fn insert_at_cursor_not_always_at_end() {
        let mut engine = ParleyEditorEngine::with_text("ac");
        engine.move_cursor(Movement::Right); // cursor between 'a' and 'c'
        engine.insert("b");
        assert_eq!(engine.snapshot().text, "abc");
    }

    #[test]
    fn backspace_on_multi_scalar_sequences_never_corrupts_utf8() {
        // A ZWJ family emoji and a base+combining-mark pair are each
        // multiple Unicode scalar values. Parley's `backdelete` only
        // collapses a whole shaped cluster into one step when the font
        // backend actually shapes it as a ligature (see parley's own
        // `Cursor::logical_clusters` / `is_emoji` special-casing) -- that
        // depends on font/shaping data available in the process, which is
        // an environment property, not something this adapter controls.
        // What Youth's adapter DOES guarantee, and what these assert, is
        // that backspace always deletes along a char boundary (never
        // panics, never leaves invalid UTF-8) and always terminates at an
        // empty buffer.
        for text in [
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}", // family emoji (ZWJ sequence)
            "cafe\u{0301}",                                // "e" + combining acute accent
        ] {
            let mut engine = ParleyEditorEngine::with_text(text);
            engine.move_cursor(Movement::End);
            for _ in 0..text.chars().count() {
                engine.backspace();
                // `snapshot().text` is only well-formed UTF-8 if the buffer
                // itself stayed valid; String's own invariant guarantees
                // this would already have panicked on corruption.
                let _ = engine.snapshot().text;
            }
            assert_eq!(
                engine.snapshot().text,
                "",
                "repeated backspace must fully clear a multi-scalar sequence: {text:?}"
            );
        }
    }

    #[test]
    fn backspace_deletes_a_hard_line_break_as_one_cluster() {
        // Parley's `backdelete` special-cases hard line breaks
        // (`cluster.is_hard_line_break()`) to delete the whole break as
        // one unit. `Movement::End` is the current PHYSICAL line's end,
        // not the buffer end, so reach the second line via Down + Home to
        // land the cursor right after the break.
        let mut engine = ParleyEditorEngine::with_text("a\nb");
        engine.move_cursor(Movement::Home);
        engine.move_cursor(Movement::Down);
        engine.move_cursor(Movement::Home);
        engine.backspace();
        assert_eq!(engine.snapshot().text, "ab");
    }

    #[test]
    fn movement_past_buffer_bounds_is_a_safe_no_op() {
        let mut engine = ParleyEditorEngine::with_text("hi");
        engine.move_cursor(Movement::Left);
        engine.move_cursor(Movement::Left);
        engine.move_cursor(Movement::Left);
        assert_eq!(engine.snapshot().cursor, 0, "clamped at buffer start");

        engine.move_cursor(Movement::Right);
        engine.move_cursor(Movement::Right);
        engine.move_cursor(Movement::Right);
        assert_eq!(engine.snapshot().cursor, 2, "clamped at buffer end");
    }

    #[test]
    fn word_movement_lands_on_word_boundaries() {
        // Parley's word-right convention lands at the end of the word
        // itself, before any trailing whitespace (verified empirically).
        let mut engine = ParleyEditorEngine::with_text("alpha beta gamma");
        engine.move_cursor(Movement::Home);
        engine.move_cursor(Movement::WordRight);
        assert_eq!(engine.snapshot().cursor, 5, "end of 'alpha'");
        engine.move_cursor(Movement::WordRight);
        assert_eq!(engine.snapshot().cursor, 10, "end of 'beta'");
        engine.move_cursor(Movement::WordLeft);
        assert_eq!(engine.snapshot().cursor, 6, "start of 'beta'");
    }

    #[test]
    fn extend_selection_produces_a_real_selection_range() {
        let mut engine = ParleyEditorEngine::with_text("abcdef");
        engine.move_cursor(Movement::Home);
        engine.extend_selection(Movement::Right);
        engine.extend_selection(Movement::Right);
        engine.extend_selection(Movement::Right);
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.selection, Some(0..3));
        assert_eq!(snapshot.cursor, 3);
    }

    #[test]
    fn select_all_selects_the_whole_buffer() {
        let mut engine = ParleyEditorEngine::with_text("abc");
        engine.select_all();
        assert_eq!(engine.snapshot().selection, Some(0..3));
    }

    #[test]
    fn insert_replaces_a_non_collapsed_selection() {
        let mut engine = ParleyEditorEngine::with_text("abcdef");
        engine.move_cursor(Movement::Home);
        engine.extend_selection(Movement::Right);
        engine.extend_selection(Movement::Right);
        engine.insert("XY");
        assert_eq!(engine.snapshot().text, "XYcdef");
    }

    #[test]
    fn bidirectional_text_cursor_movement_does_not_panic_and_stays_in_bounds() {
        // Mixed Latin (LTR) and Hebrew (RTL) content.
        let mixed = "abc \u{05d0}\u{05d1}\u{05d2} def";
        let mut engine = ParleyEditorEngine::with_text(mixed);
        engine.move_cursor(Movement::Home);
        let byte_len = mixed.len();
        for _ in 0..32 {
            engine.move_cursor(Movement::Right);
            let cursor = engine.snapshot().cursor;
            assert!(cursor <= byte_len, "cursor must stay within the buffer");
        }
        engine.move_cursor(Movement::End);
        assert_eq!(engine.snapshot().cursor, byte_len);
    }

    #[test]
    fn narrowing_the_viewport_increases_wrapped_line_count() {
        let mut engine =
            ParleyEditorEngine::with_text("one two three four five six seven eight nine ten");
        engine.set_width(Some(2000.0));
        let wide_lines = engine.line_count();
        engine.set_width(Some(60.0));
        let narrow_lines = engine.line_count();
        assert!(
            narrow_lines > wide_lines,
            "a narrower viewport must wrap into more lines (wide={wide_lines}, narrow={narrow_lines})"
        );
    }

    #[test]
    fn hit_test_maps_a_point_to_a_text_position_without_moving_the_cursor() {
        let mut engine = ParleyEditorEngine::with_text("hello world");
        engine.move_cursor(Movement::Home);
        let before = engine.snapshot().cursor;
        let hit = engine.hit_test(1_000.0, 0.0);
        assert!(hit > 0, "a far-right hit should land near the end");
        assert_eq!(
            engine.snapshot().cursor,
            before,
            "hit_test must not mutate the live cursor"
        );
    }

    #[test]
    fn move_to_byte_and_select_byte_range_restore_exact_positions() {
        let mut engine = ParleyEditorEngine::with_text("hello world");
        engine.move_to_byte(6);
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.cursor, 6);
        assert_eq!(snapshot.selection, None);

        engine.select_byte_range(0, 5);
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.selection, Some(0..5));
        assert_eq!(snapshot.cursor, 5, "focus is the range end");
    }

    #[test]
    fn selection_geometry_is_empty_when_collapsed_and_populated_when_selecting() {
        let mut engine = ParleyEditorEngine::with_text("hello");
        assert!(engine.selection_geometry().is_empty());
        engine.select_all();
        assert!(!engine.selection_geometry().is_empty());
    }

    #[test]
    fn cursor_geometry_is_available_for_a_collapsed_caret() {
        let mut engine = ParleyEditorEngine::with_text("hello");
        assert!(engine.cursor_geometry().is_some());
    }

    #[test]
    fn presentation_produces_one_glyph_per_character_with_a_stable_font_handle() {
        let mut engine = ParleyEditorEngine::with_text("hi");
        let presentation = engine.presentation();
        assert!(!presentation.runs.is_empty(), "at least one glyph run");
        let total_glyphs: usize = presentation.runs.iter().map(|run| run.glyphs.len()).sum();
        assert_eq!(total_glyphs, 2, "one glyph per character for plain ASCII");
        assert!(presentation.content_width > 0.0);
        assert!(presentation.content_height > 0.0);
        assert!(presentation.cursor.is_some());
        assert!(presentation.selection.is_empty());

        let first_run = &presentation.runs[0];
        let first_id = first_run.font.data.id();
        assert_eq!(
            first_id,
            first_run.font.data.id(),
            "the font handle's id is stable across repeated reads"
        );
        assert!(
            !first_run.font.data.data().is_empty(),
            "font bytes are accessible"
        );
    }

    #[test]
    fn presentation_selection_is_populated_while_selecting() {
        let mut engine = ParleyEditorEngine::with_text("hello");
        engine.select_all();
        let presentation = engine.presentation();
        assert!(!presentation.selection.is_empty());
    }

    /// Total glyph count across every run, as a cheap proxy for "how much
    /// visible content is currently laid out" without needing a raw-text
    /// accessor. `presentation()` lays out the buffer including any live
    /// IME preedit content, unlike `snapshot()` (see below).
    fn glyph_count(presentation: &TextPresentation) -> usize {
        presentation.runs.iter().map(|run| run.glyphs.len()).sum()
    }

    #[test]
    fn ime_compose_is_visible_in_presentation_but_excluded_from_snapshot() {
        let mut engine = ParleyEditorEngine::with_text("ab");
        engine.move_cursor(Movement::End);
        assert!(!engine.is_composing());
        let before = glyph_count(&engine.presentation());

        engine.ime_set_compose("xyz", Some((0, 3)));
        assert!(engine.is_composing());
        assert_eq!(
            glyph_count(&engine.presentation()),
            before + 3,
            "preedit text is visibly laid out while composing"
        );
        // Parley deliberately excludes in-progress preedit from `text()`:
        // it is not yet what the user has committed to writing, and must
        // not leak into undo history or the youth:editor capability's
        // guest-visible snapshot while still cancellable.
        assert_eq!(
            engine.snapshot().text,
            "ab",
            "uncommitted preedit must not appear in the committed snapshot"
        );

        engine.ime_finish_compose();
        assert!(!engine.is_composing(), "finishing ends composition");
        assert_eq!(
            engine.snapshot().text,
            "abxyz",
            "the committed text is exactly what was being composed"
        );
    }

    #[test]
    fn ime_clear_compose_removes_the_preedit_text_entirely() {
        let mut engine = ParleyEditorEngine::with_text("ab");
        engine.move_cursor(Movement::End);
        let before = glyph_count(&engine.presentation());
        engine.ime_set_compose("xyz", Some((0, 3)));
        assert_eq!(glyph_count(&engine.presentation()), before + 3);

        engine.ime_clear_compose();
        assert!(!engine.is_composing());
        assert_eq!(
            glyph_count(&engine.presentation()),
            before,
            "cancelling composition removes the preedit text from layout"
        );
        assert_eq!(engine.snapshot().text, "ab");
    }

    #[test]
    fn repeated_ime_set_compose_replaces_rather_than_accumulates() {
        let mut engine = ParleyEditorEngine::with_text("");
        engine.ime_set_compose("k", Some((0, 1)));
        engine.ime_set_compose("ka", Some((0, 2)));
        engine.ime_set_compose("kan", Some((0, 3)));
        assert_eq!(
            glyph_count(&engine.presentation()),
            3,
            "each update replaces the whole preedit run, not appends to it"
        );
        engine.ime_finish_compose();
        assert_eq!(engine.snapshot().text, "kan");
    }

    #[test]
    fn an_empty_compose_update_clears_preedit_instead_of_panicking() {
        let mut engine = ParleyEditorEngine::with_text("ab");
        engine.ime_set_compose("xyz", Some((0, 3)));
        assert!(engine.is_composing());
        engine.ime_set_compose("", None);
        assert!(!engine.is_composing());
        assert_eq!(engine.snapshot().text, "ab");
    }
}
