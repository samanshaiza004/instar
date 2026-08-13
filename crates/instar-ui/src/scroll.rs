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

/// Thickness of a scrollbar, in logical pixels. Host policy.
pub const SCROLLBAR_THICKNESS: i32 = 12;

/// Where a scrollbar lives relative to the content it scrolls.
///
/// Host presentation policy, not `Scroll` semantics and not a guest concern.
/// Scroll offsets, extents, interaction and the retirement rules are identical
/// under both; the only difference is whether the chrome shares the viewport's
/// last [`SCROLLBAR_THICKNESS`] logical pixels with the content or is given
/// them.
///
/// It exists because the Gallery's nested-viewport experiment produced a clear
/// answer. Styling *can* make a nested scroll region obviously distinct — a
/// background, a border and a radius were enough, so `Scroll` needs no default
/// chrome. But even with the boundary unmistakable, two overlay bars still
/// land in the same right-edge band and cannot be told apart. Viewport
/// legibility was not the root problem.
///
/// Treating this as policy rather than as scrolling semantics has strong
/// precedent: AppKit supports overlay and legacy scrollers and picks between
/// them from a *user preference*, GTK has an explicit overlay-scrolling
/// setting, and Qt's classic scroll area reserves viewport space for a bar.
/// Three toolkits, one axis, and none of them makes it intrinsic to the widget.
///
/// The invariant that survives both: a bar belongs on the edge of the viewport
/// it scrolls. `Inset` changes how wide the usable content rectangle is; it
/// never moves a nested bar sideways because another bar happens to exist
/// nearby, and nesting depth never enters the geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarStyle {
    /// The bar paints over the viewport's edge. Content keeps the full width,
    /// and whatever is under the bar is covered by it.
    #[default]
    Overlay,
    /// The bar gets its own strip. The content rectangle is narrower by
    /// [`SCROLLBAR_THICKNESS`], and nothing is ever hidden beneath chrome.
    ///
    /// The strip is reserved whether or not the content currently overflows.
    /// Reserving only when a bar appears — as Qt's classic scroll area does —
    /// makes the content reflow at the moment it crosses the threshold, and
    /// for a viewport whose content is near its own height that oscillates.
    /// CSS calls the stable version `scrollbar-gutter: stable`, and it is the
    /// same trade: a little space always, or a reflow sometimes.
    Inset,
}

/// The shortest a thumb is allowed to get, in logical pixels.
///
/// Host policy, and the reason thumb position is not simply proportional: in a
/// very long document the proportional thumb would be a few pixels tall and
/// impossible to grab. Once the minimum binds, the thumb travels over a
/// slightly shorter track than the naive arithmetic suggests, which
/// [`Scrollbar::thumb`] accounts for.
pub const MIN_THUMB_LENGTH: i32 = 24;

/// Which piece of a scrollbar a pointer is over, or has hold of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarPart {
    /// The draggable handle.
    Thumb,
    /// The groove above or below it. Clicking pages.
    Track,
}

/// A viewport's vertical scrollbar, in absolute logical coordinates.
///
/// Presentation derived from the `Scroll` node, never a node itself: chrome
/// with a `NodeKey` would be chrome the guest can see, the ledger accounts
/// for, and accessibility has to explain.
///
/// Vertical only. A horizontal one is the same arithmetic on the other axis,
/// but the two together need a rule for the corner where they meet and for
/// whether each steals space from the other — that is a design question this
/// package does not need to answer to prove the interaction model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrollbar {
    /// The full groove, down the right edge of the viewport.
    pub track: crate::Rect,
    /// The handle, somewhere inside the track.
    pub thumb: crate::Rect,
}

impl Scrollbar {
    /// The scrollbar for a viewport, or `None` when the content fits.
    ///
    /// Nothing is drawn when there is nothing to scroll, so a viewport that
    /// happens to be large enough has no chrome rather than a full-length
    /// thumb that cannot move.
    pub fn for_viewport(viewport: crate::Rect, content_height: i32, offset_y: i32) -> Option<Self> {
        let scrollable = content_height - viewport.height;
        if scrollable <= 0 || viewport.width <= 0 || viewport.height <= 0 {
            return None;
        }

        let track = crate::Rect::new(
            viewport.x + viewport.width - SCROLLBAR_THICKNESS,
            viewport.y,
            SCROLLBAR_THICKNESS.min(viewport.width),
            viewport.height,
        );

        // Proportional, then floored at the minimum so it stays grabbable.
        let proportional =
            (viewport.height as i64 * viewport.height as i64 / content_height as i64) as i32;
        let length = proportional.clamp(MIN_THUMB_LENGTH.min(viewport.height), viewport.height);

        // Travel is what is left of the track once the thumb occupies part of
        // it. Dividing by `scrollable` rather than by the content height is
        // what keeps the thumb flush with the bottom at maximum offset even
        // when the minimum length bound.
        let travel = viewport.height - length;
        let position = if scrollable > 0 {
            (travel as i64 * offset_y.clamp(0, scrollable) as i64 / scrollable as i64) as i32
        } else {
            0
        };

        Some(Self {
            track,
            thumb: crate::Rect::new(track.x, track.y + position, track.width, length),
        })
    }

    /// The offset a thumb dragged to `thumb_top` corresponds to.
    ///
    /// The inverse of the position arithmetic above. A track with no travel —
    /// a thumb as long as its track — maps everything to zero rather than
    /// dividing by it.
    pub fn offset_for_thumb_top(&self, thumb_top: i32, scrollable: i32) -> i32 {
        let travel = self.track.height - self.thumb.height;
        if travel <= 0 {
            return 0;
        }
        let within = (thumb_top - self.track.y).clamp(0, travel);
        ((within as i64 * scrollable as i64 / travel as i64) as i32).clamp(0, scrollable.max(0))
    }

    pub fn part_at(&self, x: i32, y: i32) -> Option<ScrollbarPart> {
        if !crate::rect_contains(self.track, x, y) {
            return None;
        }
        Some(if crate::rect_contains(self.thumb, x, y) {
            ScrollbarPart::Thumb
        } else {
            ScrollbarPart::Track
        })
    }
}

/// A thumb drag in progress.
///
/// Both origins are recorded because the offset is computed from where the
/// drag *started*, not incrementally from the last event. Accumulating deltas
/// would let rounding drift over a long drag, and would make the thumb lag the
/// pointer after any clamped movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbDrag {
    pub viewport: NodeKey,
    pub origin_pointer_y: i32,
    pub origin_offset_y: i32,
}

/// Every live viewport's offset, plus whatever the pointer is doing to a
/// scrollbar right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrollState {
    offsets: HashMap<NodeKey, ScrollOffset>,
    hovered: Option<(NodeKey, ScrollbarPart)>,
    active: Option<ThumbDrag>,
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

    pub fn hovered(&self) -> Option<(NodeKey, ScrollbarPart)> {
        self.hovered
    }

    /// Records what the pointer is over. Returns whether it changed, which is
    /// the caller's cue to repaint and its cue *not* to when nothing moved.
    pub fn set_hovered(&mut self, hovered: Option<(NodeKey, ScrollbarPart)>) -> bool {
        let changed = self.hovered != hovered;
        self.hovered = hovered;
        changed
    }

    pub fn dragging(&self) -> Option<ThumbDrag> {
        self.active
    }

    pub fn begin_drag(&mut self, drag: ThumbDrag) {
        self.active = Some(drag);
    }

    /// Abandons any drag in progress.
    ///
    /// Called when the geometry the drag began against stops being valid — a
    /// resize, a scale change, the viewport being deleted or hidden. Finishing
    /// a drag against geometry that no longer exists is the same defect as
    /// completing a press against a node that no longer exists.
    pub fn cancel_drag(&mut self) {
        self.active = None;
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
        if self
            .active
            .is_some_and(|drag| removed.contains(&drag.viewport))
        {
            self.active = None;
        }
        if self.hovered.is_some_and(|(key, _)| removed.contains(&key)) {
            self.hovered = None;
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

/// How far one line of wheel travel scrolls, in logical pixels.
///
/// UI policy, which is why the windowing layer hands over a line *count* and
/// declines to answer this. A wheel notch moving a fixed distance regardless
/// of the text under it is the behaviour every desktop has; tying it to the
/// font of whatever is beneath the pointer makes the same gesture do different
/// things in different parts of one window.
pub const LOGICAL_PIXELS_PER_LINE: f64 = 40.0;

/// What a scroll gesture asked for, in logical pixels, sign already settled.
///
/// `+y` increases the offset and reveals content further down. That was fixed
/// at the window boundary; nothing here re-interprets it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollDeltaPixels {
    pub x: f64,
    pub y: f64,
}

impl ScrollDeltaPixels {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn is_zero(self) -> bool {
        self.x == 0.0 && self.y == 0.0
    }
}

/// What one wheel event did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollOutcome {
    /// Whether any viewport's offset actually moved.
    ///
    /// The reason a caller can decide not to ask for a frame. A redraw that
    /// changes no pixel is still a frame somebody paid for, and a wheel at the
    /// end of a list produces a stream of them.
    pub consumed: bool,
}

/// Applies a wheel delta to the viewports under `(x, y)`.
///
/// # Deepest first, and the remainder bubbles
///
/// The innermost viewport under the pointer takes what it can, and whatever it
/// could not use passes outward to its ancestors. "The nearest scroll owns the
/// whole event" is simpler and is the classic nested-scroll trap: an inner
/// viewport already at its limit swallows input that should have kept
/// scrolling the outer one, and the interface feels stuck for no reason a user
/// can see.
///
/// Returns whether anything moved. Nothing here can produce a guest event —
/// there is no path from this function to the wire, which is the point of
/// Stage 3's acceptance rather than an accident of this implementation.
pub fn apply_wheel(
    tree: &Tree,
    layout: &crate::LayoutSnapshot,
    state: &mut ScrollState,
    extent_of: &dyn Fn(NodeKey) -> Option<ScrollOffset>,
    x: i32,
    y: i32,
    delta: ScrollDeltaPixels,
) -> ScrollOutcome {
    // Innermost last, so draining from the end walks outward.
    let chain = viewport_chain(tree, layout, state, x, y);
    let mut remaining = delta;
    let mut consumed = false;

    for key in chain.iter().rev() {
        if remaining.is_zero() {
            break;
        }
        let Some(max) = extent_of(*key) else {
            continue;
        };
        let before = state.get(*key);
        let wanted = ScrollOffset::new(
            before.x + remaining.x.round() as i32,
            before.y + remaining.y.round() as i32,
        );
        let after = wanted.clamped(max);
        if after != before {
            state.set(*key, after);
            consumed = true;
        }
        // What this viewport could not absorb, in the same units it arrived
        // in. Subtracting the *applied* movement rather than the whole delta
        // is what makes a partially-scrolled viewport hand on only the excess.
        remaining = ScrollDeltaPixels::new(
            remaining.x - f64::from(after.x - before.x),
            remaining.y - f64::from(after.y - before.y),
        );
    }

    ScrollOutcome { consumed }
}

/// Every `Scroll` containing `(x, y)`, outermost first.
///
/// Uses the same clip-then-translate walk hit-testing does, because a viewport
/// the pointer is not actually over — scrolled out from under an ancestor
/// clip, say — must not be a candidate. Reimplementing the traversal here
/// would be a second answer to a question that already has one.
fn viewport_chain(
    tree: &Tree,
    layout: &crate::LayoutSnapshot,
    state: &ScrollState,
    x: i32,
    y: i32,
) -> Vec<NodeKey> {
    let mut chain = Vec::new();
    collect_viewports(&tree.root, layout, state, x, y, None, &mut chain);
    chain
}

fn collect_viewports(
    node: &Node,
    layout: &crate::LayoutSnapshot,
    state: &ScrollState,
    x: i32,
    y: i32,
    clip: Option<crate::Rect>,
    out: &mut Vec<NodeKey>,
) {
    if !crate::is_presented(node) {
        return;
    }
    let Some(rect) = layout.get(node.key) else {
        return;
    };

    let clips =
        node.layout.overflow == crate::WireOverflow::Clip || matches!(node.kind, NodeKind::Scroll);
    let clip = if clips {
        Some(match clip {
            Some(outer) => crate::rect_intersection(outer, rect),
            None => rect,
        })
    } else {
        clip
    };
    if let Some(clip) = clip
        && !crate::rect_contains(clip, x, y)
    {
        return;
    }

    if matches!(node.kind, NodeKind::Scroll) {
        out.push(node.key);
    }

    let (x, y, clip) = match node.kind {
        NodeKind::Scroll => {
            let offset = state.get(node.key);
            let moved = clip.map(|clip| {
                crate::Rect::new(
                    clip.x + offset.x,
                    clip.y + offset.y,
                    clip.width,
                    clip.height,
                )
            });
            (x + offset.x, y + offset.y, moved)
        }
        _ => (x, y, clip),
    };

    for child in &node.children {
        collect_viewports(child, layout, state, x, y, clip, out);
    }
}

/// The content child of a `Scroll`, if it has the one it is required to have.
///
/// `DecodedUiSnapshot::from_wire` rejects any other arity, so this returning
/// `None` means the tree was hand-built rather than decoded.
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
