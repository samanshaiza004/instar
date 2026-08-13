//! A presentation of a buffer: caret, selection, and where it is looking.
//!
//! # Two "which side?" questions that are not the same question
//!
//! This module answers one of them and [`TextAffinity`] answers the other. They
//! were both called "affinity" until package B measured the difference, and the
//! overloading was dangerous enough to be worth two names:
//!
//! ```text
//! edit stickiness    an insertion happens exactly at a position. Does the
//!                    position stay before the new text, or move after it?
//!                    Decided by whether this view did the typing.
//!
//! visual affinity    a byte offset is visually ambiguous — a bidi boundary,
//!                    a soft line break. Which side does the caret draw on?
//!                    Decided by where the user put it, and carried with the
//!                    position ever after.
//! ```
//!
//! Nothing may make one decide the other. They both answer "which side", and
//! that is the whole of their similarity.
//!
//! # Edit stickiness, frozen
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
//! Note that this is decided by `originated_here`, not by any stored field: a
//! position does not carry its stickiness, because stickiness is a property of
//! an edit rather than of a position.

use crate::{TextBufferId, TextEdit};

/// Which visual side of an ambiguous byte offset a position sits on.
///
/// # Why this exists now, and did not in package A
///
/// A's version of this module said an affinity enum would arrive "if a case
/// needs both, in this project's usual order — a type with one meaningful value
/// is speculative generality". Package B is that case, and it arrived with
/// evidence: at a bidi boundary the same byte draws in two different places,
/// and a caret built from the byte alone lands in one of them arbitrarily.
///
/// # Why it lives here
///
/// It is a property of a position in a *document*, not of a shaped layout. The
/// document is this crate's. `instar-ui` keeps its own Parley-facing
/// `Affinity` and `instar-host` converts between them, which is what keeps
/// `instar-ui -> instar-text` absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAffinity {
    /// Attached to the character logically after the offset.
    #[default]
    Downstream,
    /// Attached to the character logically before it.
    Upstream,
}

/// A position in a document: which byte, and which side of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextPosition {
    pub byte: usize,
    pub affinity: TextAffinity,
}

impl TextPosition {
    pub fn at(byte: usize) -> Self {
        Self {
            byte,
            affinity: TextAffinity::Downstream,
        }
    }

    pub fn with_affinity(byte: usize, affinity: TextAffinity) -> Self {
        Self { byte, affinity }
    }
}

/// An anchored region of text. Empty when both ends are on the same byte.
///
/// `head` is where the caret is and where typing happens; `anchor` is where the
/// selection started. Keeping them ordered would lose which end the user is
/// dragging, which is the whole reason a selection is not a `Range`.
///
/// **This is the only persistent selection in Instar.** The host projects it
/// onto individual shaped segments to ask for geometry, but those projections
/// are temporary — a Parley `Selection` is a range within one layout, and a
/// document is many. Two persistent selections is the shape of defect this
/// project has removed twice; there is one authority and it is here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Selection {
    pub anchor: TextPosition,
    pub head: TextPosition,
}

impl Selection {
    pub fn at(byte: usize) -> Self {
        Self::from_position(TextPosition::at(byte))
    }

    pub fn from_position(position: TextPosition) -> Self {
        Self {
            anchor: position,
            head: position,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor.byte == self.head.byte
    }

    /// The selection as an ordered byte range.
    ///
    /// Direction is deliberately lost here: this is for intersecting with a
    /// region, not for describing the gesture that made it.
    pub fn range(&self) -> std::ops::Range<usize> {
        if self.anchor.byte <= self.head.byte {
            self.anchor.byte..self.head.byte
        } else {
            self.head.byte..self.anchor.byte
        }
    }

    /// Moves the head, leaving the anchor where it was.
    pub fn extend_to(&self, head: TextPosition) -> Self {
        Self {
            anchor: self.anchor,
            head,
        }
    }
}

/// One view of one buffer.
#[derive(Debug, Clone)]
pub struct TextView {
    buffer: TextBufferId,
    selection: Selection,
    /// Scroll offset in logical pixels, host-owned exactly as a `Scroll` node's
    /// is.
    scroll_y: i32,
}

impl TextView {
    pub fn new(buffer: TextBufferId) -> Self {
        Self {
            buffer,
            selection: Selection::default(),
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
        self.selection.head.byte
    }

    /// Where the caret is, including which side of its byte it sits on.
    pub fn caret_position(&self) -> TextPosition {
        self.selection.head
    }

    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
    }

    pub fn set_caret(&mut self, byte: usize) {
        self.selection = Selection::at(byte);
    }

    pub fn set_caret_position(&mut self, position: TextPosition) {
        self.selection = Selection::from_position(position);
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
    /// `originated_here` is edit stickiness, and nothing to do with
    /// [`TextAffinity`] — see the module documentation. Visual affinity is
    /// carried through unchanged: an edit moves *where* a position is, not
    /// which side of itself it sits on.
    pub(crate) fn transform(&mut self, edit: &TextEdit, originated_here: bool) {
        self.selection.anchor.byte =
            transform_position(self.selection.anchor.byte, edit, originated_here);
        self.selection.head.byte =
            transform_position(self.selection.head.byte, edit, originated_here);
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

    /// The two "which side?" questions, held apart by a test rather than only
    /// by two names.
    ///
    /// An edit moves *where* a position is. It does not decide which visual
    /// side of itself that position sits on — that was chosen by whoever put
    /// the caret there, and an insertion elsewhere in the document has no
    /// opinion about it. Without this, edit stickiness quietly resetting visual
    /// affinity would pass the whole suite.
    #[test]
    fn an_edit_moves_a_position_without_changing_its_visual_affinity() {
        for originated_here in [false, true] {
            let mut v = view();
            v.set_selection(Selection {
                anchor: TextPosition::with_affinity(20, TextAffinity::Upstream),
                head: TextPosition::with_affinity(25, TextAffinity::Upstream),
            });

            v.transform(&TextEdit::insert(10, "abc"), originated_here);

            assert_eq!(v.selection().anchor.byte, 23, "the position moved");
            assert_eq!(v.selection().head.byte, 28);
            assert_eq!(
                v.selection().anchor.affinity,
                TextAffinity::Upstream,
                "and its visual affinity did not, because an edit three rows \
                 away has no opinion about which side of a bidi boundary a \
                 caret was placed on"
            );
            assert_eq!(v.selection().head.affinity, TextAffinity::Upstream);
        }
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
            anchor: TextPosition::at(10),
            head: TextPosition::at(14),
        });
        v.transform(&TextEdit::replace(10..14, "hello"), true);
        assert_eq!(
            (v.selection().anchor.byte, v.caret()),
            (10, 15),
            "the caret ends after what was typed, not in front of it"
        );
    }

    #[test]
    fn a_selection_keeps_which_end_is_being_dragged() {
        let backwards = Selection {
            anchor: TextPosition::at(20),
            head: TextPosition::at(10),
        };
        assert_eq!(backwards.range(), 10..20);
        assert_eq!(backwards.head.byte, 10, "the dragged end survives");
    }
}
