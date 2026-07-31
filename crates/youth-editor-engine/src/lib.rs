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
#[derive(Clone, Copy, Debug, PartialEq)]
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
}

