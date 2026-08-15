//! Which node has the keyboard, and how it moves.
//!
//! # Host-owned, like every other transient interaction state
//!
//! > Focus is host state keyed by a generational [`NodeKey`]. A guest may
//! > request focus semantically; ordinary movement needs no Wasm round trip.
//!
//! Tab must not wait for a guest, for the same reason a pressed button must
//! not: the response is presentation, and presentation is the host's. What the
//! guest hears about is the *outcome* — a button was activated — never the
//! traversal that led there.
//!
//! # Why the generation matters here more than anywhere
//!
//! Focus is the longest-lived reference the host keeps to a single node. It
//! survives commits, it survives the node scrolling out of view, and a user can
//! leave it somewhere for minutes. That is exactly the shape of reference that
//! outlives what it names:
//!
//! ```text
//! focus (7, 0)  ->  guest removes it  ->  guest re-adds (7, 1)
//!               ->  the new button must not be focused
//! ```
//!
//! With a generation this needs no rule at all — the stale key does not match.

use crate::{Node, NodeKey, Tree, is_presented};

/// What the keyboard is pointed at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FocusState {
    focused: Option<NodeKey>,
    focus_visible: bool,
}

/// Which direction a traversal moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusMove {
    Next,
    Previous,
}

impl FocusState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn focused(&self) -> Option<NodeKey> {
        self.focused
    }

    /// Whether focus should be *drawn*.
    ///
    /// Deterministic host policy rather than a `:focus-visible` heuristic:
    /// keyboard traversal and accessibility set it, a pointer click clears it.
    /// That keeps a keyboard-style ring off the screen after every mouse click
    /// without the guest tracking input modality.
    pub fn focus_visible(&self) -> bool {
        self.focus_visible
    }

    /// Focus moved because someone clicked. The ring stays hidden.
    pub fn focus_by_pointer(&mut self, key: Option<NodeKey>) -> bool {
        let changed = self.focused != key || self.focus_visible;
        self.focused = key;
        self.focus_visible = false;
        changed
    }

    /// Focus moved by keyboard or accessibility. The ring shows.
    pub fn focus_by_keyboard(&mut self, key: Option<NodeKey>) -> bool {
        let changed = self.focused != key || !self.focus_visible;
        self.focused = key;
        self.focus_visible = key.is_some();
        changed
    }

    /// Moves focus one step through `tree`, returning whether anything moved.
    ///
    /// Wraps at both ends, which is what every desktop does and what makes a
    /// short form navigable without reaching for the mouse.
    pub fn traverse(&mut self, tree: &Tree, direction: FocusMove) -> bool {
        let order = focusable_order(tree);
        if order.is_empty() {
            return self.focus_by_keyboard(None);
        }

        // A focused key that is no longer in the order — retired between
        // commits, say — restarts rather than searching for where it was.
        let current = self
            .focused
            .and_then(|key| order.iter().position(|candidate| *candidate == key));

        let next = match (current, direction) {
            (Some(index), FocusMove::Next) => (index + 1) % order.len(),
            (Some(index), FocusMove::Previous) => (index + order.len() - 1) % order.len(),
            (None, FocusMove::Next) => 0,
            (None, FocusMove::Previous) => order.len() - 1,
        };
        self.focus_by_keyboard(Some(order[next]))
    }

    /// Drops focus if it names a node that can no longer hold it.
    ///
    /// # The invariant
    ///
    /// > Focus is retired before the new tree becomes interactive if the
    /// > focused node was removed, hidden, disabled, or given
    /// > `Display::None`.
    ///
    /// One rule covering all four, because to focus they are the same answer:
    /// a node the keyboard cannot reach must not be holding the keyboard.
    ///
    /// Focus is **cleared**, not moved to a neighbour. Moving it would keep a
    /// user near where they were, and would need a second piece of transient
    /// state — a remembered position keyed by `NodeKey` — with its own
    /// retirement rules and its own way of eventually referencing a node that
    /// no longer exists. Clearing cannot be subtly wrong.
    pub fn retire(&mut self, tree: &Tree) -> bool {
        let Some(focused) = self.focused else {
            return false;
        };
        if focusable_order(tree).contains(&focused) {
            return false;
        }
        self.focused = None;
        self.focus_visible = false;
        true
    }
}

/// Where a revealed node should sit within its viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RevealAlignment {
    /// Move as little as possible. A node already visible does not move at
    /// all, and a partly visible one moves just enough to expose it.
    #[default]
    Nearest,
    Start,
    Center,
    End,
}

/// Brings `key` into view, adjusting every `Scroll` ancestor that needs it.
///
/// # Semantic intent, never an offset
///
/// A guest asks for a node to be visible. It does not compute where that
/// leaves the viewport, is not told, and could not act on the answer — the
/// offset is host state for the same reason geometry is.
///
/// # Innermost outward, recomputing between every step
///
/// Moving an inner viewport changes where the target sits relative to the
/// outer one. Computing every offset from the original geometry gives the
/// outer viewport a stale answer, and that is wrong only in the nested case —
/// which is exactly the case that goes untested if nobody writes it down.
///
/// Returns whether anything moved. Nothing here can reach the guest.
pub fn reveal(
    tree: &Tree,
    layout: &crate::LayoutSnapshot,
    scroll: &mut crate::ScrollState,
    extent_of: &dyn Fn(NodeKey) -> Option<crate::ScrollOffset>,
    key: NodeKey,
    alignment: RevealAlignment,
) -> bool {
    // A node that cannot be seen cannot be revealed, and a key nobody knows is
    // a no-op rather than an error: a stale key from a queued request names a
    // node the guest has since replaced.
    let Some(path) = ancestry(&tree.root, key) else {
        return false;
    };
    if !path.iter().all(|node| is_presented(node)) {
        return false;
    }
    let Some(mut target) = layout.get(key) else {
        return false;
    };

    let mut moved = false;
    // `path` runs root -> target, so reversing walks outward from the node.
    for node in path.iter().rev() {
        if !matches!(node.kind, crate::NodeKind::Scroll) {
            continue;
        }
        let (Some(viewport), Some(max)) = (layout.get(node.key), extent_of(node.key)) else {
            continue;
        };

        let before = scroll.get(node.key);
        // The target's position as currently presented, which for an inner
        // viewport already reflects whatever the step before it did.
        let relative_top = target.y - viewport.y;
        let relative_bottom = relative_top + target.height;

        let desired = match alignment {
            RevealAlignment::Nearest => {
                if relative_top < 0 {
                    before.y + relative_top
                } else if relative_bottom > viewport.height {
                    before.y + (relative_bottom - viewport.height)
                } else {
                    before.y
                }
            }
            RevealAlignment::Start => before.y + relative_top,
            RevealAlignment::End => before.y + (relative_bottom - viewport.height),
            RevealAlignment::Center => {
                before.y + relative_top - (viewport.height - target.height) / 2
            }
        };

        let after = crate::ScrollOffset::new(before.x, desired).clamped(max);
        if after != before {
            scroll.set(node.key, after);
            moved = true;
        }
        // Recompute before the next viewport outward looks at it.
        target = crate::Rect::new(
            target.x,
            target.y - (after.y - before.y),
            target.width,
            target.height,
        );
    }
    moved
}

/// The chain of nodes from the root down to `key`, inclusive.
fn ancestry(node: &Node, key: NodeKey) -> Option<Vec<&Node>> {
    if node.key == key {
        return Some(vec![node]);
    }
    for child in &node.children {
        if let Some(mut path) = ancestry(child, key) {
            path.insert(0, node);
            return Some(path);
        }
    }
    None
}

/// Every node the keyboard can reach, in retained tree order.
///
/// Tree order rather than a guest-stated tab index: the order a guest
/// described its interface in is the order it reads in, and an explicit index
/// is a second source of truth that drifts from the first.
pub fn focusable_order(tree: &Tree) -> Vec<NodeKey> {
    let mut order = Vec::new();
    collect(&tree.root, &mut order);
    order
}

fn collect(node: &Node, out: &mut Vec<NodeKey>) {
    // Suppression is inherited, so a hidden container takes its whole subtree
    // out of the order rather than only itself.
    if !is_presented(node) {
        return;
    }
    if node.kind.is_focusable() {
        out.push(node.key);
    }
    for child in &node.children {
        collect(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Node;

    fn form() -> Tree {
        Tree::new(Node::root(
            0,
            vec![
                Node::text(1, "label"),
                Node::button(2, "first"),
                Node::button(3, "second"),
                Node::button(4, "disabled").disabled(),
            ],
        ))
    }

    #[test]
    fn only_reachable_controls_are_in_the_order() {
        assert_eq!(
            focusable_order(&form()),
            vec![NodeKey::first(2), NodeKey::first(3)],
            "text is not interactive and a disabled button is not reachable"
        );
    }

    #[test]
    fn a_hidden_container_takes_its_whole_subtree_out() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::button(1, "visible"),
                Node::column(2, vec![Node::button(3, "inside")]).hidden(),
                Node::column(4, vec![Node::button(5, "gone")]).display_none(),
            ],
        ));
        assert_eq!(focusable_order(&tree), vec![NodeKey::first(1)]);
    }

    #[test]
    fn tab_walks_forward_and_wraps() {
        let tree = form();
        let mut focus = FocusState::new();

        focus.traverse(&tree, FocusMove::Next);
        assert_eq!(focus.focused(), Some(NodeKey::first(2)));
        focus.traverse(&tree, FocusMove::Next);
        assert_eq!(focus.focused(), Some(NodeKey::first(3)));
        focus.traverse(&tree, FocusMove::Next);
        assert_eq!(
            focus.focused(),
            Some(NodeKey::first(2)),
            "past the last control, back to the first"
        );
    }

    #[test]
    fn shift_tab_walks_backward_and_wraps() {
        let tree = form();
        let mut focus = FocusState::new();

        focus.traverse(&tree, FocusMove::Previous);
        assert_eq!(
            focus.focused(),
            Some(NodeKey::first(3)),
            "from nothing, Shift+Tab starts at the end"
        );
        focus.traverse(&tree, FocusMove::Previous);
        assert_eq!(focus.focused(), Some(NodeKey::first(2)));
        focus.traverse(&tree, FocusMove::Previous);
        assert_eq!(focus.focused(), Some(NodeKey::first(3)));
    }

    #[test]
    fn traversal_shows_the_ring_and_a_click_hides_it() {
        let tree = form();
        let mut focus = FocusState::new();

        focus.traverse(&tree, FocusMove::Next);
        assert!(focus.focus_visible(), "keyboard traversal draws focus");

        focus.focus_by_pointer(Some(NodeKey::first(3)));
        assert_eq!(focus.focused(), Some(NodeKey::first(3)));
        assert!(
            !focus.focus_visible(),
            "clicking moves focus without painting a keyboard ring"
        );
    }

    #[test]
    fn a_tree_with_nothing_focusable_focuses_nothing() {
        let tree = Tree::new(Node::root(0, vec![Node::text(1, "just text")]));
        let mut focus = FocusState::new();
        focus.traverse(&tree, FocusMove::Next);
        assert_eq!(focus.focused(), None);
    }

    #[test]
    fn retirement_clears_focus_when_the_node_stops_being_reachable() {
        for (what, tree) in [
            (
                "removed",
                Tree::new(Node::root(0, vec![Node::button(3, "second")])),
            ),
            (
                "disabled",
                Tree::new(Node::root(
                    0,
                    vec![Node::button(2, "first").disabled(), Node::button(3, "b")],
                )),
            ),
            (
                "hidden",
                Tree::new(Node::root(
                    0,
                    vec![Node::button(2, "first").hidden(), Node::button(3, "b")],
                )),
            ),
            (
                "display none",
                Tree::new(Node::root(
                    0,
                    vec![
                        Node::button(2, "first").display_none(),
                        Node::button(3, "b"),
                    ],
                )),
            ),
        ] {
            let mut focus = FocusState::new();
            focus.focus_by_keyboard(Some(NodeKey::first(2)));
            assert!(focus.retire(&tree), "{what} should retire focus");
            assert_eq!(focus.focused(), None, "{what} clears focus outright");
            assert!(!focus.focus_visible(), "{what} hides the ring with it");
        }
    }

    #[test]
    fn retirement_leaves_a_still_reachable_focus_alone() {
        let mut focus = FocusState::new();
        focus.focus_by_keyboard(Some(NodeKey::first(2)));
        assert!(!focus.retire(&form()), "nothing about node 2 changed");
        assert_eq!(focus.focused(), Some(NodeKey::first(2)));
    }

    /// The regression generational keys exist for.
    #[test]
    fn a_reused_id_does_not_inherit_focus() {
        let mut focus = FocusState::new();
        focus.focus_by_keyboard(Some(NodeKey::new(7, 0)));

        // The same id, a new lifetime.
        let reborn = Tree::new(Node::root(
            0,
            vec![Node {
                key: NodeKey::new(7, 1),
                kind: crate::NodeKind::Button {
                    label: "reused".into(),
                    enabled: true,
                },
                layout: Default::default(),
                style: Default::default(),
                children: Vec::new(),
            }],
        ));

        assert!(focus.retire(&reborn), "the old key is not in the new order");
        assert_eq!(
            focus.focused(),
            None,
            "a button that happens to reuse id 7 must not inherit the \
             keyboard from the one that had it"
        );
    }

    #[test]
    fn focus_restarts_from_the_top_after_retirement() {
        let tree = form();
        let mut focus = FocusState::new();
        focus.focus_by_keyboard(Some(NodeKey::first(3)));
        focus.retire(&Tree::new(Node::root(0, vec![Node::button(2, "first")])));

        focus.traverse(&tree, FocusMove::Next);
        assert_eq!(
            focus.focused(),
            Some(NodeKey::first(2)),
            "cleared focus means the next Tab starts at the beginning, which \
             is the whole reason not to remember a position"
        );
    }
}
