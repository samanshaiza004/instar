//! Host-owned scroll offsets, and the rules for keeping them.
//!
//! # The offset is not on the wire, in either direction
//!
//! > A guest owns the content. The host owns where that content is scrolled
//! > to.
//!
//! A guest cannot set an offset, read one, or veto a change to one. That is
//! what makes B2's promise possible — a wheel event moves the view with no
//! Wasm round trip — and it is the same reasoning as geometry: a guest
//! authoritative over presentation state would undermine the retained host
//! presentation model. It also means a guest cannot scroll a view out from
//! under a user who is reading it.
//!
//! # Retained, clamped, and eventually destroyed
//!
//! ```text
//! commit that keeps the Scroll alive   the offset survives unchanged
//! content shrinks                      clamped before the next presentation
//!                                      becomes interactive
//! Display::None / Visibility::Hidden   no interaction; the offset is kept
//! the node is deleted                  the offset is destroyed with it
//! ```
//!
//! Hiding and deleting look alike and are opposites, which is the distinction
//! this module exists to hold. A node the guest hid is still a node the guest
//! has, and returning to where you were when it reappears is what a user
//! expects. A node the guest removed is gone. Generational [`NodeKey`]s make
//! that unambiguous rather than a rule anyone has to remember: an id that
//! comes back comes back at a new generation, so it cannot collide with the
//! entry that was dropped.

use std::collections::HashMap;

use crate::{Node, NodeKey, NodeKind, Tree};

/// How far a viewport's content is scrolled, in logical pixels.
///
/// Non-negative and measured from the content's origin, so painting
/// translates descendants by `-offset` and hit-testing translates the pointer
/// by `+offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollOffset {
    pub x: i32,
    pub y: i32,
}

impl ScrollOffset {
    pub const ZERO: Self = Self { x: 0, y: 0 };

    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Confines this offset to what `max` allows, never below zero.
    ///
    /// Both directions matter. Content that shrank leaves an offset pointing
    /// past the end, and an offset below zero would scroll a viewport away
    /// from its own content — which nothing should be able to ask for, but
    /// clamping here means nothing downstream has to check.
    pub fn clamped(self, max: Self) -> Self {
        Self {
            x: self.x.clamp(0, max.x.max(0)),
            y: self.y.clamp(0, max.y.max(0)),
        }
    }
}

/// Every live viewport's offset, keyed by the `Scroll` node that owns it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrollState {
    offsets: HashMap<NodeKey, ScrollOffset>,
}

impl ScrollState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The offset for `key`, or zero for a viewport nothing has scrolled.
    ///
    /// Absent and zero are deliberately the same answer: a viewport at the top
    /// and a viewport nobody has touched look identical to a user, and keeping
    /// them distinct would only create a state to get wrong.
    pub fn get(&self, key: NodeKey) -> ScrollOffset {
        self.offsets.get(&key).copied().unwrap_or_default()
    }

    /// Replaces an offset outright. B2's wheel handling goes through here.
    pub fn set(&mut self, key: NodeKey, offset: ScrollOffset) {
        self.offsets.insert(key, offset);
    }

    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Drops the offsets of nodes the diff reported as removed.
    ///
    /// Deletion destroys; hiding does not. This is the deletion half, and it
    /// belongs beside `Interaction::retire` in the commit path for the same
    /// reason: state that outlives the node it describes eventually lands on
    /// something else.
    pub fn retire(&mut self, removed: &[NodeKey]) {
        for key in removed {
            self.offsets.remove(key);
        }
    }

    /// Confines every offset to what the new layout leaves scrollable.
    ///
    /// Runs after layout and before the snapshot becomes interactive, which is
    /// the whole point: content that shrank must not leave a viewport showing
    /// a region that no longer exists, and a hit-test against a stale offset
    /// would resolve to the wrong node rather than to nothing.
    ///
    /// A `Scroll` that is no longer laid out at all — hidden, or under a
    /// `Display::None` ancestor — is skipped rather than zeroed. Its offset is
    /// retained, and there is nothing to clamp it against until it comes back.
    pub fn clamp_to(&mut self, tree: &Tree, extents: &dyn Fn(NodeKey) -> Option<ScrollOffset>) {
        for node in tree.iter() {
            if !matches!(node.kind, NodeKind::Scroll) {
                continue;
            }
            let Some(max) = extents(node.key) else {
                continue;
            };
            if let Some(offset) = self.offsets.get_mut(&node.key) {
                *offset = offset.clamped(max);
            }
        }
    }
}

/// The content child of a `Scroll`, if it has the one it is required to have.
///
/// `Tree::from_wire` rejects any other arity, so this returning `None` means
/// the tree was hand-built rather than decoded.
pub fn content_of(node: &Node) -> Option<&Node> {
    matches!(node.kind, NodeKind::Scroll)
        .then(|| node.children.first())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWPORT: NodeKey = NodeKey::first(1);

    #[test]
    fn an_untouched_viewport_reads_as_zero() {
        let state = ScrollState::new();
        assert_eq!(state.get(VIEWPORT), ScrollOffset::ZERO);
        assert!(state.is_empty());
    }

    #[test]
    fn clamping_confines_an_offset_to_the_content_that_is_left() {
        let offset = ScrollOffset::new(80, 300);
        assert_eq!(
            offset.clamped(ScrollOffset::new(80, 150)),
            ScrollOffset::new(80, 150),
            "content that shrank pulls the offset back to the new end"
        );
        assert_eq!(
            offset.clamped(ScrollOffset::ZERO),
            ScrollOffset::ZERO,
            "content that no longer overflows leaves nothing to scroll"
        );
    }

    #[test]
    fn a_negative_bound_clamps_to_zero_rather_than_inverting() {
        assert_eq!(
            ScrollOffset::new(10, 10).clamped(ScrollOffset::new(-50, -50)),
            ScrollOffset::ZERO,
            "a viewport larger than its content has no scrollable extent, and \
             a negative maximum must not become a negative offset"
        );
    }

    #[test]
    fn deletion_destroys_the_offset() {
        let mut state = ScrollState::new();
        state.set(VIEWPORT, ScrollOffset::new(0, 120));
        state.retire(&[VIEWPORT]);
        assert_eq!(state.get(VIEWPORT), ScrollOffset::ZERO);
        assert!(state.is_empty(), "the entry is gone, not merely zeroed");
    }

    #[test]
    fn an_unrelated_removal_leaves_the_offset_alone() {
        let mut state = ScrollState::new();
        state.set(VIEWPORT, ScrollOffset::new(0, 120));
        state.retire(&[NodeKey::first(99)]);
        assert_eq!(state.get(VIEWPORT), ScrollOffset::new(0, 120));
    }

    /// An id that returns does so at a new generation, so the offset the old
    /// lifetime held cannot be inherited by the new one even by accident.
    #[test]
    fn a_reused_id_starts_from_zero() {
        let mut state = ScrollState::new();
        let first = NodeKey::new(7, 0);
        state.set(first, ScrollOffset::new(0, 90));
        state.retire(&[first]);
        assert_eq!(state.get(NodeKey::new(7, 1)), ScrollOffset::ZERO);
    }
}
