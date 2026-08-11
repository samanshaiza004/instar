//! A presentation of a buffer: caret, selection, and where it is looking.
//!
//! # The affinity policy, frozen
//!
//! View A inserts `"abc"` at byte 10. View B's caret is also at byte 10. Does B
//! end up at 10 or 13? Nothing about a byte offset decides it, and the two-view
//! test cannot claim a caret "moved correctly" until it does.
//!
//! ```text
//! An insertion at a view's exact caret position moves that view's own caret
//! to after the inserted text, and leaves every other view's caret before it.
//!
//! A deletion clamps any position inside the removed range to its start.
//! ```
//!
//! The typist's caret follows their own typing — that is what typing *is*. An
//! observer is not dragged along by someone else's insertion, because from B's
//! point of view text appeared ahead of the cursor.
//!
//! No `Affinity` enum yet. One policy, stated here and asserted by
//! `an_insertion_at_a_shared_caret_moves_only_the_typist`. The enum arrives if
//! a case needs both, in this project's usual order — a type with one
//! meaningful value is speculative generality.

use crate::{TextBufferId, TextEdit};

/// An anchored region of text. Empty when `anchor == head`.
///
/// `head` is where the caret is and where typing happens; `anchor` is where the
/// selection started. Keeping them ordered would lose which end the user is
/// dragging, which is the whole reason a selection is not a `Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn at(byte: usize) -> Self {
        Self {
            anchor: byte,
            head: byte,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The selection as an ordered byte range.
    pub fn range(&self) -> std::ops::Range<usize> {
        if self.anchor <= self.head {
            self.anchor..self.head
        } else {
            self.head..self.anchor
        }
    }
}

/// One view of one buffer.
#[derive(Debug, Clone)]
pub struct TextView {
    buffer: TextBufferId,
    selection: Selection,
    /// Wrap width in logical pixels. `None` is "do not wrap".
    wrap_width: Option<f32>,
    /// Scroll offset in logical pixels, host-owned exactly as a `Scroll` node's
    /// is.
    scroll_y: i32,
}

impl TextView {
    pub fn new(buffer: TextBufferId) -> Self {
        Self {
            buffer,
            selection: Selection::default(),
            wrap_width: None,
            scroll_y: 0,
        }
    }

    pub fn buffer(&self) -> TextBufferId {
        self.buffer
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// Where the caret is.
    pub fn caret(&self) -> usize {
        self.selection.head
    }

    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
    }

    pub fn set_caret(&mut self, byte: usize) {
        self.selection = Selection::at(byte);
    }

    pub fn wrap_width(&self) -> Option<f32> {
        self.wrap_width
    }

    pub fn set_wrap_width(&mut self, width: Option<f32>) {
        self.wrap_width = width;
    }

    pub fn scroll_y(&self) -> i32 {
        self.scroll_y
    }

    pub fn set_scroll_y(&mut self, offset: i32) {
        self.scroll_y = offset.max(0);
    }

    /// Moves this view's positions across an edit.
    ///
    /// `originated_here` is the whole of the affinity policy: it decides what
    /// happens to a position sitting exactly where text was inserted. See the
    /// module documentation.
    pub(crate) fn transform(&mut self, edit: &TextEdit, originated_here: bool) {
        self.selection.anchor = transform_position(self.selection.anchor, edit, originated_here);
        self.selection.head = transform_position(self.selection.head, edit, originated_here);
    }
}

/// One byte offset, moved across one edit.
fn transform_position(position: usize, edit: &TextEdit, originated_here: bool) -> usize {
    let start = edit.range.start;
    let end = edit.range.end;

    if position < start {
        // Entirely before the edit: untouched.
        return position;
    }

    if position == start && start == end {
        // A pure insertion, exactly at this position. The one genuinely
        // ambiguous case, and the only place the policy applies.
        return if originated_here {
            edit.resulting_end()
        } else {
            position
        };
    }

    if position <= end && position >= start {
        // Inside or at the edge of what was removed. Clamped to the start,
        // then carried past the replacement if this view did the editing --
        // otherwise a typist replacing a selection would find their caret at
        // the front of the text they just typed.
        return if originated_here && position == end {
            edit.resulting_end()
        } else if position == start {
            if originated_here && start == end {
                edit.resulting_end()
            } else {
                start
            }
        } else {
            start
        };
    }

    // Entirely after the edit: shifted by what it added or removed.
    (position as isize + edit.delta()) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> TextView {
        TextView::new(TextBufferId {
            id: 1,
            generation: 0,
        })
    }

    #[test]
    fn a_position_before_an_edit_does_not_move() {
        let mut v = view();
        v.set_caret(3);
        v.transform(&TextEdit::insert(10, "abc"), false);
        assert_eq!(v.caret(), 3);
    }

    #[test]
    fn a_position_after_an_edit_shifts_by_its_delta() {
        let mut v = view();
        v.set_caret(20);
        v.transform(&TextEdit::insert(10, "abc"), false);
        assert_eq!(v.caret(), 23);

        let mut v = view();
        v.set_caret(20);
        v.transform(&TextEdit::delete(10..14), false);
        assert_eq!(v.caret(), 16);
    }

    /// The frozen policy, and the reason it had to be decided before this test
    /// could claim anything.
    #[test]
    fn an_insertion_at_a_shared_caret_moves_only_the_typist() {
        let mut typist = view();
        typist.set_caret(10);
        typist.transform(&TextEdit::insert(10, "abc"), true);
        assert_eq!(
            typist.caret(),
            13,
            "a caret follows the text its own view typed -- that is what \
             typing is"
        );

        let mut observer = view();
        observer.set_caret(10);
        observer.transform(&TextEdit::insert(10, "abc"), false);
        assert_eq!(
            observer.caret(),
            10,
            "and is not dragged by someone else's insertion: from here, text \
             appeared ahead of the cursor"
        );
    }

    #[test]
    fn a_position_inside_a_deletion_clamps_to_its_start() {
        for position in [11, 12, 13] {
            let mut v = view();
            v.set_caret(position);
            v.transform(&TextEdit::delete(10..14), false);
            assert_eq!(v.caret(), 10, "position {position} had nowhere else to go");
        }
    }

    #[test]
    fn replacing_a_selection_leaves_the_typists_caret_after_the_replacement() {
        let mut v = view();
        v.set_selection(Selection {
            anchor: 10,
            head: 14,
        });
        v.transform(&TextEdit::replace(10..14, "hello"), true);
        assert_eq!(
            (v.selection().anchor, v.caret()),
            (10, 15),
            "the caret ends after what was typed, not in front of it"
        );
    }

    #[test]
    fn a_selection_keeps_which_end_is_being_dragged() {
        let backwards = Selection {
            anchor: 20,
            head: 10,
        };
        assert_eq!(backwards.range(), 10..20);
        assert_eq!(backwards.head, 10, "the dragged end survives");
    }
}
