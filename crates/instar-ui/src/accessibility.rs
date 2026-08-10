//! Projecting retained host state into an AccessKit tree.
//!
//! # A read, never a second model
//!
//! ```text
//! retained tree + LayoutSnapshot + FocusState + ScrollState
//!         ->  TreeUpdate  ->  platform accessibility APIs
//! ```
//!
//! Everything AccessKit wants, Instar already knows. **No accessibility
//! concept enters the guest wire**: a guest that has never heard of AccessKit
//! is already fully described, because roles come from node kinds, names come
//! from the text a guest was going to render anyway, bounds come from layout,
//! and focus comes from [`FocusState`].
//!
//! That is the whole reason this is a pure function. Accessibility that needed
//! the guest to participate would be accessibility that stops working the
//! moment a guest is slow, which is the same failure the rest of this stage
//! exists to rule out.
//!
//! # It must not cause work
//!
//! > Building accessibility state enters neither layout, shaping, nor
//! > rendering.
//!
//! This consumes state that has already been computed. Assistive technology
//! being active must not make the interface slower — and per the rule
//! `docs/ARCHITECTURE.md` records, the test for that observes *entry* into
//! layout and shaping rather than their cost.
//!
//! # Identity survives reuse
//!
//! `NodeId` is a `u64`, and [`NodeKey::to_accesskit_id`] packs the generation
//! into its high half. An id the guest removes and reuses therefore becomes a
//! genuinely new accessible object rather than resurrecting one a screen
//! reader may still hold a reference to.

use accesskit::{Action, Node as AccessNode, NodeId, Role, Tree as AccessTree, TreeUpdate};

use crate::{FocusState, LayoutSnapshot, Node, NodeKind, ScrollState, Tree, is_presented, scroll};

/// The accessible tree for the interface as it currently stands.
///
/// `None` when the root itself is not presented, since a tree with no root is
/// not a tree AccessKit can hold.
pub fn project(
    tree: &Tree,
    layout: &LayoutSnapshot,
    focus: &FocusState,
    scroll: &ScrollState,
    scale: f64,
) -> Option<TreeUpdate> {
    if !is_presented(&tree.root) {
        return None;
    }
    let root_id = ak_id(tree.root.key);

    let mut nodes = Vec::new();
    build(&tree.root, layout, scroll, scale, &mut nodes);

    Some(TreeUpdate {
        nodes,
        tree: Some(AccessTree::new(root_id)),
        tree_id: accesskit::TreeId::ROOT,
        // The single source of accessibility focus. AccessKit wants the root
        // when nothing in particular is focused, which is the same thing
        // `FocusState::None` means — so there is no second notion of focus to
        // keep in step with the first.
        focus: focus.focused().map_or(root_id, ak_id),
    })
}

/// The packed identity, as AccessKit's newtype.
///
/// `NodeKey::to_accesskit_id` gives the `u64`; this is the one place it is
/// wrapped, so the packing has a single spelling everywhere.
fn ak_id(key: crate::NodeKey) -> NodeId {
    NodeId(key.to_accesskit_id())
}

fn build(
    node: &Node,
    layout: &LayoutSnapshot,
    scroll: &ScrollState,
    scale: f64,
    out: &mut Vec<(NodeId, AccessNode)>,
) {
    let mut access = AccessNode::new(role_of(&node.kind));

    // Two transforms carry logical content coordinates into the space
    // AccessKit documents: "relative to the origin of the tree's container
    // (e.g. window), in physical pixels, with the y coordinate being
    // top-down."
    //
    // The root scales. Bounds stay logical everywhere -- that is the space
    // `instar-ui` works in, and pushing physical pixels down into the layout
    // layer to satisfy a platform would invert the whole dependency
    // direction -- so exactly one node converts, and it is the one whose
    // transform every other node inherits.
    //
    // A viewport translates by its scroll offset, so its descendants describe
    // where they *are* rather than where they would be at rest. Without it a
    // screen reader draws its cursor on the unscrolled position of everything
    // inside a scrolled region.
    //
    // Both were wrong until a real screen reader showed it. At scale 1 with
    // nothing scrolled, both are the identity, which is why every automated
    // test agreed.
    if node.key == crate::NodeKey::first(0) || matches!(node.kind, NodeKind::Root) {
        if scale != 1.0 {
            access.set_transform(accesskit::Affine::scale(scale));
        }
    } else if matches!(node.kind, NodeKind::Scroll) {
        let offset = scroll.get(node.key);
        if offset.x != 0 || offset.y != 0 {
            access.set_transform(accesskit::Affine::translate((
                f64::from(-offset.x),
                f64::from(-offset.y),
            )));
        }
    }

    // Bounds are logical, which is the space `instar-ui` works in throughout.
    // A node with no geometry is still projected: it is semantically present,
    // and omitting bounds is more honest than inventing a rectangle.
    if let Some(rect) = layout.get(node.key) {
        access.set_bounds(accesskit::Rect {
            x0: f64::from(rect.x),
            y0: f64::from(rect.y),
            x1: f64::from(rect.x + rect.width),
            y1: f64::from(rect.y + rect.height),
        });
    }

    match &node.kind {
        NodeKind::Text { text } => access.set_label(text.clone()),
        NodeKind::Button { label, enabled } => {
            access.set_label(label.clone());
            if *enabled {
                // Advertised only because Instar can honour it: this routes
                // into the same activation a click uses. AccessKit's
                // vocabulary is far larger, and advertising an action because
                // the enum has it is promising behaviour that does not exist.
                access.add_action(Action::Click);
                access.add_action(Action::Focus);
            } else {
                // Present and disabled rather than absent. A control that
                // vanishes when it stops working is worse than one announced
                // as unavailable — the same reasoning that draws a disabled
                // button rather than hiding it.
                access.set_disabled();
            }
        }
        NodeKind::Scroll => {
            access.add_action(Action::ScrollIntoView);
            let offset = scroll.get(node.key);
            access.set_scroll_y(f64::from(offset.y));
            access.set_scroll_y_min(0.0);
            let extent = scroll::content_of(node)
                .and_then(|content| Some((layout.get(node.key)?, layout.get(content.key)?)))
                .map_or(0, |(viewport, content)| {
                    (content.height - viewport.height).max(0)
                });
            access.set_scroll_y_max(f64::from(extent));
        }
        NodeKind::Root | NodeKind::Column | NodeKind::Row | NodeKind::Stack => {}
    }

    // Suppressed subtrees are absent, not hidden-but-present. `Display::None`
    // and `Visibility::Hidden` both mean "not part of the interface right
    // now", and a screen reader announcing a control the user cannot see or
    // reach is worse than one that does not mention it.
    let children: Vec<&Node> = node.children.iter().filter(|c| is_presented(c)).collect();
    access.set_children(
        children
            .iter()
            .map(|child| ak_id(child.key))
            .collect::<Vec<_>>(),
    );

    out.push((ak_id(node.key), access));
    for child in children {
        build(child, layout, scroll, scale, out);
    }
}

fn role_of(kind: &NodeKind) -> Role {
    match kind {
        NodeKind::Root => Role::Window,
        // Presentational, and documented as normally filtered out by
        // assistive technology — which is exactly right for a node whose whole
        // meaning is "these things are stacked". Inventing a semantic role for
        // it would put layout structure into the announced interface.
        NodeKind::Column | NodeKind::Row | NodeKind::Stack => Role::GenericContainer,
        NodeKind::Text { .. } => Role::Label,
        NodeKind::Button { .. } => Role::Button,
        NodeKind::Scroll => Role::ScrollView,
    }
}

/// What an accessibility object looks like, reduced to what can change it.
///
/// The point of hashing rather than keeping a second `Node` is that a node can
/// change all day without its *accessible* projection changing: a foreground,
/// a border, a corner radius, a hover, a pressed face. Comparing projections
/// answers "did the accessible object change?", which is the question, rather
/// than "did the Instar node change?", which is not.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Fingerprint {
    role: Role,
    name: u64,
    children: u64,
    bounds: Option<crate::Rect>,
    disabled: bool,
    scroll: Option<(i32, i32)>,
    /// The node's own transform, as the bits of its coefficients.
    ///
    /// Bounds alone do not describe where a node is once transforms carry the
    /// scale and the scroll offset: a DPI change moves every node on screen
    /// without changing a single logical rectangle, and so would produce no
    /// update at all.
    transform: Option<[u64; 6]>,
}

fn hash_of(value: impl std::hash::Hash) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Tracks what the platform has already been told, so each update carries only
/// what changed.
///
/// # An update happens if and only if something accessible changed
///
/// > An Instar change produces an AccessKit update iff it changes
/// > accessibility-observable state.
///
/// Not "produces an empty update". AccessKit documents that even unchanged
/// nodes in a `TreeUpdate` cost processing and replacement, and the whole
/// point of C's invalidation split was that a colour change costs nothing —
/// letting it cost an accessibility round trip would put the expense back
/// through a different door.
///
/// Dirtiness comes from three places, not one: a guest commit, a layout
/// result, and host-local state like focus and scroll. Tying accessibility to
/// guest commits alone would leave a screen reader with stale geometry after
/// every wheel event.
#[derive(Debug, Default)]
pub struct A11yProjection {
    entries: std::collections::HashMap<crate::NodeKey, Fingerprint>,
    focus: Option<NodeId>,
    started: bool,
}

impl A11yProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// The incremental update for the current state, or `None` when nothing
    /// an assistive technology can observe has changed.
    pub fn update(
        &mut self,
        tree: &Tree,
        layout: &LayoutSnapshot,
        focus: &FocusState,
        scroll: &ScrollState,
        scale: f64,
    ) -> Option<TreeUpdate> {
        if !is_presented(&tree.root) {
            return None;
        }
        let root_id = ak_id(tree.root.key);
        let focus_id = focus.focused().map_or(root_id, ak_id);

        let mut current = Vec::new();
        build(&tree.root, layout, scroll, scale, &mut current);

        let mut nodes = Vec::new();
        let mut seen = std::collections::HashSet::with_capacity(current.len());
        for (id, node) in current {
            let key = crate::NodeKey::from_accesskit_id(id.0);
            seen.insert(key);
            let fingerprint = fingerprint_of(&node);
            if self.entries.get(&key) != Some(&fingerprint) {
                self.entries.insert(key, fingerprint);
                nodes.push((id, node));
            }
        }

        // Removed nodes are dropped from the record but never *emitted*: their
        // surviving parent's child list changed, so the parent is already in
        // `nodes`, and AccessKit treats a node that is neither the root nor a
        // child of another node as an error.
        self.entries.retain(|key, _| seen.contains(key));

        let focus_changed = self.focus != Some(focus_id);
        let first = !self.started;
        if nodes.is_empty() && !focus_changed && !first {
            return None;
        }
        self.focus = Some(focus_id);
        self.started = true;

        Some(TreeUpdate {
            nodes,
            // The tree descriptor accompanies the first update; afterwards the
            // adapter already holds it.
            tree: first.then(|| AccessTree::new(root_id)),
            tree_id: accesskit::TreeId::ROOT,
            // Carried on every update, related to the changed nodes or not:
            // AccessKit asks for it each time, and for the root when nothing
            // in particular is focused.
            focus: focus_id,
        })
    }

    /// Forgets everything, so the next update is a complete tree again.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.focus = None;
        self.started = false;
    }
}

fn fingerprint_of(node: &AccessNode) -> Fingerprint {
    Fingerprint {
        role: node.role(),
        name: hash_of(node.label()),
        children: hash_of(node.children().iter().map(|id| id.0).collect::<Vec<_>>()),
        bounds: node.bounds().map(|rect| {
            crate::Rect::new(
                rect.x0 as i32,
                rect.y0 as i32,
                (rect.x1 - rect.x0) as i32,
                (rect.y1 - rect.y0) as i32,
            )
        }),
        disabled: node.is_disabled(),
        scroll: node
            .scroll_y()
            .map(|y| (y as i32, node.scroll_y_max().unwrap_or_default() as i32)),
        transform: node
            .transform()
            .map(|affine| affine.as_coeffs().map(f64::to_bits)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeKey, TextContext, Viewport, WireAlign, WireColor, WireLayout, WireSize};

    fn projected(tree: &Tree) -> (TreeUpdate, LayoutSnapshot) {
        let mut text = TextContext::new();
        let layout = tree.layout(&mut text, Viewport::new(400.0, 300.0));
        let update = project(tree, &layout, &FocusState::new(), &ScrollState::new(), 1.0)
            .expect("a presented root projects");
        (update, layout)
    }

    fn role(update: &TreeUpdate, key: NodeKey) -> Option<Role> {
        update
            .nodes
            .iter()
            .find(|(id, _)| *id == ak_id(key))
            .map(|(_, node)| node.role())
    }

    // --- F2: incremental updates. ---

    /// Drives a projection across successive states, like the host would.
    struct Session {
        projection: A11yProjection,
        text: TextContext,
        focus: FocusState,
        scroll: ScrollState,
        /// Scale 1 unless a test says otherwise, so the transforms are the
        /// identity and these tests keep asserting what they were written to.
        scale: f64,
    }

    impl Session {
        fn new() -> Self {
            Self {
                projection: A11yProjection::new(),
                text: TextContext::new(),
                focus: FocusState::new(),
                scroll: ScrollState::new(),
                scale: 1.0,
            }
        }

        fn commit(&mut self, tree: &Tree) -> Option<TreeUpdate> {
            let layout = tree.layout(&mut self.text, Viewport::new(400.0, 300.0));
            self.projection
                .update(tree, &layout, &self.focus, &self.scroll, self.scale)
        }
    }

    fn changed(update: &Option<TreeUpdate>) -> Vec<NodeId> {
        update
            .as_ref()
            .map(|u| u.nodes.iter().map(|(id, _)| *id).collect())
            .unwrap_or_default()
    }

    fn styled_tree(color: Option<WireColor>) -> Tree {
        let mut button = Node::button(2, "press");
        if let Some(color) = color {
            button = button
                .with_foreground(color)
                .with_background(color)
                .with_border(3, color)
                .with_corner_radius(4);
        }
        Tree::new(Node::root(0, vec![Node::text(1, "steady"), button]))
    }

    /// The structural invariant: paint-only produces *no update at all*, not
    /// an empty one. AccessKit charges for unchanged nodes in an update, so an
    /// empty round trip would put C's cost back through a different door.
    #[test]
    fn a_paint_only_change_produces_no_accessibility_update() {
        let mut session = Session::new();
        assert!(
            session.commit(&styled_tree(None)).is_some(),
            "the first update is the whole tree"
        );

        let repainted = session.commit(&styled_tree(Some(WireColor::opaque(255, 0, 0))));
        assert!(
            repainted.is_none(),
            "foreground, background, border and radius are invisible to \
             assistive technology: {:?}",
            changed(&repainted)
        );
    }

    /// A repaint of the *focused* node is still a repaint. Being focused does
    /// not make every pixel change accessibility-relevant.
    #[test]
    fn a_paint_only_change_to_the_focused_node_is_also_silent() {
        let mut session = Session::new();
        session.focus.focus_by_keyboard(Some(NodeKey::first(2)));
        session.commit(&styled_tree(None));

        assert!(
            session
                .commit(&styled_tree(Some(WireColor::opaque(0, 255, 0))))
                .is_none()
        );
    }

    #[test]
    fn an_identical_commit_produces_nothing() {
        let mut session = Session::new();
        session.commit(&styled_tree(None));
        assert!(session.commit(&styled_tree(None)).is_none());
    }

    #[test]
    fn changing_text_updates_only_that_node() {
        let mut session = Session::new();
        session.commit(&styled_tree(None));

        let renamed = Tree::new(Node::root(
            0,
            vec![Node::text(1, "different"), Node::button(2, "press")],
        ));
        assert_eq!(
            changed(&session.commit(&renamed)),
            vec![ak_id(NodeKey::first(1))],
            "the label changed and nothing else did"
        );
    }

    #[test]
    fn disabling_a_button_updates_only_that_node() {
        let mut session = Session::new();
        session.commit(&styled_tree(None));

        let disabled = Tree::new(Node::root(
            0,
            vec![Node::text(1, "steady"), Node::button(2, "press").disabled()],
        ));
        assert_eq!(
            changed(&session.commit(&disabled)),
            vec![ak_id(NodeKey::first(2))]
        );
    }

    /// AccessKit's parent/child contract: adding a child emits the child *and*
    /// the parent whose list changed.
    #[test]
    fn adding_a_child_emits_the_child_and_its_parent() {
        let mut session = Session::new();
        session.commit(&styled_tree(None));

        let grown = Tree::new(Node::root(
            0,
            vec![
                Node::text(1, "steady"),
                Node::button(2, "press"),
                Node::button(3, "new"),
            ],
        ));
        let ids = changed(&session.commit(&grown));
        assert!(ids.contains(&ak_id(NodeKey::first(3))), "the new child");
        assert!(
            ids.contains(&ak_id(NodeKey::first(0))),
            "and the parent, whose child list is now different -- without it \
             the platform holds a tree that does not mention the new node"
        );
        assert_eq!(ids.len(), 2, "and nothing else: {ids:?}");
    }

    /// Removal emits the surviving parent and *not* the departed subtree: a
    /// node that is neither root nor anyone's child is an error in AccessKit's
    /// model.
    #[test]
    fn removing_a_subtree_emits_the_parent_and_not_the_departed() {
        let mut session = Session::new();
        let full = Tree::new(Node::root(
            0,
            vec![
                Node::text(1, "steady"),
                Node::column(2, vec![Node::button(3, "inside")]),
            ],
        ));
        session.commit(&full);

        let pruned = Tree::new(Node::root(0, vec![Node::text(1, "steady")]));
        let ids = changed(&session.commit(&pruned));
        assert_eq!(ids, vec![ak_id(NodeKey::first(0))]);
        for gone in [2u32, 3] {
            assert!(
                !ids.contains(&ak_id(NodeKey::first(gone))),
                "node {gone} left the tree and must not be emitted"
            );
        }
    }

    #[test]
    fn hiding_an_ancestor_removes_its_whole_subtree() {
        let mut session = Session::new();
        let visible = Tree::new(Node::root(
            0,
            vec![
                Node::text(1, "steady"),
                Node::column(2, vec![Node::button(3, "inside")]),
            ],
        ));
        session.commit(&visible);

        let hidden = Tree::new(Node::root(
            0,
            vec![
                Node::text(1, "steady"),
                Node::column(2, vec![Node::button(3, "inside")]).hidden(),
            ],
        ));
        let ids = changed(&session.commit(&hidden));
        assert_eq!(
            ids,
            vec![ak_id(NodeKey::first(0))],
            "only the surviving parent, with a shorter child list"
        );
    }

    /// Bounds are accessibility state even though they are not application
    /// state. A moved control that keeps stale geometry is a screen reader
    /// AccessKit's coordinate contract, which nothing here previously kept.
    ///
    /// > AccessKit expects the final transformed coordinates to be relative to
    /// > the origin of the tree's container (e.g. window), in physical pixels,
    /// > with the y coordinate being top-down.
    ///
    /// Instar reported logical pixels and no transform at all. At scale 1 with
    /// nothing scrolled those coincide, which is every test written before a
    /// real screen reader ran: VoiceOver drew its cursor at half size on a
    /// 2x display, and on the resting position of anything scrolled.
    #[test]
    fn the_root_carries_the_scale_and_a_viewport_carries_its_scroll() {
        use crate::{WireAlign, WireLayout, WireSize};
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    10,
                    Node::column(
                        11,
                        vec![
                            Node::button(12, "near"),
                            Node::text(13, "tall").with_layout(WireLayout {
                                height: WireSize::Fixed(600),
                                ..WireLayout::default()
                            }),
                        ],
                    ),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(100),
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        ));
        let mut text = TextContext::new();
        let layout = tree.layout(&mut text, Viewport::new(400.0, 300.0));

        let mut scroll = ScrollState::new();
        scroll.set(NodeKey::first(10), crate::ScrollOffset { x: 0, y: 40 });

        let update = project(&tree, &layout, &FocusState::new(), &scroll, 2.0).unwrap();
        let node = |key: NodeKey| {
            update
                .nodes
                .iter()
                .find(|(id, _)| *id == ak_id(key))
                .map(|(_, node)| node.clone())
                .unwrap_or_else(|| panic!("{key:?} is missing"))
        };

        assert_eq!(
            node(NodeKey::first(0))
                .transform()
                .map(|affine| affine.as_coeffs()),
            Some([2.0, 0.0, 0.0, 2.0, 0.0, 0.0]),
            "the root converts logical to physical, once, for the whole tree"
        );

        assert_eq!(
            node(NodeKey::first(10))
                .transform()
                .map(|affine| affine.as_coeffs()),
            Some([1.0, 0.0, 0.0, 1.0, 0.0, -40.0]),
            "and a scrolled viewport moves its descendants by its offset, so \
             they describe where they are rather than where they would rest"
        );

        assert!(
            node(NodeKey::first(12)).transform().is_none(),
            "an ordinary node inherits both and states neither"
        );
    }

    /// A DPI change moves every node on screen without changing one logical
    /// rectangle, so bounds alone cannot notice it.
    #[test]
    fn a_scale_change_alone_produces_an_update() {
        let tree = Tree::new(Node::root(0, vec![Node::button(1, "press")]));
        let mut text = TextContext::new();
        let layout = tree.layout(&mut text, Viewport::new(400.0, 300.0));
        let focus = FocusState::new();
        let scroll = ScrollState::new();

        let mut projection = A11yProjection::new();
        assert!(
            projection
                .update(&tree, &layout, &focus, &scroll, 1.0)
                .is_some(),
            "the first update is the whole tree"
        );
        assert!(
            projection
                .update(&tree, &layout, &focus, &scroll, 1.0)
                .is_none(),
            "and nothing changed"
        );

        let update = projection
            .update(&tree, &layout, &focus, &scroll, 2.0)
            .expect("a scale change is accessibility-observable");
        assert!(
            update
                .nodes
                .iter()
                .any(|(id, _)| *id == ak_id(NodeKey::first(0))),
            "the root's transform is what changed, so the root must be resent"
        );
    }

    /// pointing at the wrong place.
    #[test]
    fn moving_a_node_updates_its_bounds() {
        let mut session = Session::new();
        let with_spacer = |height: u16| {
            Tree::new(Node::root(
                0,
                vec![
                    Node::text(1, "spacer").with_layout(WireLayout {
                        height: WireSize::Fixed(height),
                        ..WireLayout::default()
                    }),
                    Node::button(2, "moves"),
                ],
            ))
        };
        session.commit(&with_spacer(20));

        let ids = changed(&session.commit(&with_spacer(60)));
        assert!(
            ids.contains(&ak_id(NodeKey::first(2))),
            "the button did not change semantically but it did move: {ids:?}"
        );
    }

    /// Host-local scrolling produces accessibility updates with no guest
    /// involved -- accessibility dirtiness is not tied to guest commits.
    #[test]
    fn a_host_local_scroll_updates_the_viewport_without_a_commit() {
        let stretch = |height: u16| WireLayout {
            height: WireSize::Fixed(height),
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(1, Node::text(2, "content").with_layout(stretch(400)))
                    .with_layout(stretch(100)),
            ],
        ));

        let mut session = Session::new();
        session.commit(&tree);
        assert!(session.commit(&tree).is_none(), "nothing changed yet");

        // No new tree: only host state moved.
        session
            .scroll
            .set(NodeKey::first(1), crate::ScrollOffset::new(0, 150));
        let ids = changed(&session.commit(&tree));
        assert!(
            ids.contains(&ak_id(NodeKey::first(1))),
            "the viewport's reported offset changed: {ids:?}"
        );
    }

    #[test]
    fn a_focus_only_change_carries_focus_and_no_nodes() {
        let mut session = Session::new();
        session.commit(&styled_tree(None));

        session.focus.focus_by_keyboard(Some(NodeKey::first(2)));
        let update = session.commit(&styled_tree(None)).expect("focus moved");
        assert!(
            update.nodes.is_empty(),
            "no node's own properties changed: {:?}",
            changed(&Some(update.clone()))
        );
        assert_eq!(update.focus, ak_id(NodeKey::first(2)));
    }

    /// The first update carries the tree descriptor; later ones do not.
    #[test]
    fn only_the_first_update_describes_the_tree() {
        let mut session = Session::new();
        let first = session.commit(&styled_tree(None)).expect("first");
        assert!(first.tree.is_some());

        let renamed = Tree::new(Node::root(
            0,
            vec![Node::text(1, "different"), Node::button(2, "press")],
        ));
        let second = session.commit(&renamed).expect("changed");
        assert!(
            second.tree.is_none(),
            "the adapter already holds the tree descriptor"
        );
    }

    /// The C5-style invariant, for accessibility.
    ///
    /// Projecting consumes state that is already computed. Assistive
    /// technology being active must not make the interface slower, and the
    /// test observes *entry* into the text system rather than its cost —
    /// `reused` is the counter that can tell "nothing asked" from "something
    /// asked and the cache answered".
    #[test]
    fn projecting_enters_neither_layout_nor_shaping() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::text(1, "some words"),
                Node::button(2, "press me"),
                Node::scroll(3, Node::text(4, "content")),
            ],
        ));
        let mut text = TextContext::new();
        let layout = tree.layout(&mut text, Viewport::new(400.0, 300.0));

        text.reset_stats();
        for _ in 0..5 {
            project(&tree, &layout, &FocusState::new(), &ScrollState::new(), 1.0)
                .expect("projects");
        }

        let stats = text.stats();
        assert_eq!(
            (
                stats.rebuilt,
                stats.relinebroken,
                stats.extracted,
                stats.reused
            ),
            (0, 0, 0, 0),
            "a projection is a read of state that already exists: {stats:?}"
        );
    }

    #[test]
    fn every_kind_maps_to_its_boring_role() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::column(1, vec![Node::text(2, "hello")]),
                Node::row(3, vec![Node::button(4, "press")]),
                Node::stack(5, vec![]),
                Node::scroll(6, Node::text(7, "content")),
            ],
        ));
        let (update, _) = projected(&tree);

        assert_eq!(role(&update, NodeKey::first(0)), Some(Role::Window));
        assert_eq!(
            role(&update, NodeKey::first(1)),
            Some(Role::GenericContainer)
        );
        assert_eq!(role(&update, NodeKey::first(2)), Some(Role::Label));
        assert_eq!(
            role(&update, NodeKey::first(3)),
            Some(Role::GenericContainer)
        );
        assert_eq!(role(&update, NodeKey::first(4)), Some(Role::Button));
        assert_eq!(
            role(&update, NodeKey::first(5)),
            Some(Role::GenericContainer)
        );
        assert_eq!(role(&update, NodeKey::first(6)), Some(Role::ScrollView));
    }

    /// Two buttons that differ *only* in generation. A projection that packed
    /// the id alone would give them one accessible object between them.
    #[test]
    fn the_same_id_at_two_generations_gets_two_accessible_objects() {
        use crate::NodeKind;
        let button = |generation: u32| Node {
            key: NodeKey::new(7, generation),
            kind: NodeKind::Button {
                label: format!("generation {generation}"),
                enabled: true,
            },
            layout: WireLayout::default(),
            style: Default::default(),
            children: Vec::new(),
        };

        // Not a tree `from_wire` would accept -- duplicate ids are refused --
        // but the projection must still keep them apart, because across two
        // commits this is exactly the pair a screen reader must not confuse.
        let first = Tree::new(Node::root(0, vec![button(0)]));
        let second = Tree::new(Node::root(0, vec![button(1)]));
        let (a, _) = projected(&first);
        let (b, _) = projected(&second);

        let id_a = ak_id(NodeKey::new(7, 0));
        let id_b = ak_id(NodeKey::new(7, 1));
        assert_ne!(id_a, id_b, "the generation is in the packed id");
        assert!(a.nodes.iter().any(|(id, _)| *id == id_a));
        assert!(b.nodes.iter().any(|(id, _)| *id == id_b));
        assert!(
            !b.nodes.iter().any(|(id, _)| *id == id_a),
            "the new lifetime must not resurrect the old accessible object"
        );
    }

    #[test]
    fn a_disabled_button_is_present_and_announced_as_disabled() {
        let tree = Tree::new(Node::root(
            0,
            vec![Node::button(1, "on"), Node::button(2, "off").disabled()],
        ));
        let (update, _) = projected(&tree);

        let node = |key: NodeKey| {
            update
                .nodes
                .iter()
                .find(|(id, _)| *id == ak_id(key))
                .map(|(_, node)| node)
                .expect("projected")
        };
        assert!(!node(NodeKey::first(1)).is_disabled());
        assert!(
            node(NodeKey::first(2)).is_disabled(),
            "a disabled control is announced as unavailable, not omitted"
        );
        assert!(
            !node(NodeKey::first(2)).supports_action(Action::Click),
            "and does not advertise an action it would refuse"
        );
    }

    /// A visible child of a hidden parent must still disappear — the same rule
    /// hit-testing and painting already follow.
    #[test]
    fn a_visible_child_of_a_hidden_parent_is_absent_from_the_tree() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::button(1, "reachable"),
                Node::column(2, vec![Node::button(3, "inside")]).hidden(),
                Node::column(4, vec![Node::button(5, "gone")]).display_none(),
            ],
        ));
        let (update, _) = projected(&tree);

        let present = |key: NodeKey| update.nodes.iter().any(|(id, _)| *id == ak_id(key));
        assert!(present(NodeKey::first(1)));
        for absent in [2u32, 3, 4, 5] {
            assert!(
                !present(NodeKey::first(absent)),
                "node {absent} is suppressed and must not be announced"
            );
        }

        // And the root must not claim a child it no longer lists.
        let root = update
            .nodes
            .iter()
            .find(|(id, _)| *id == ak_id(NodeKey::first(0)))
            .map(|(_, node)| node)
            .unwrap();
        assert_eq!(
            root.children(),
            &[ak_id(NodeKey::first(1))],
            "a dangling child reference is an error in AccessKit's model"
        );
    }

    #[test]
    fn focus_state_is_the_only_source_of_accessibility_focus() {
        let tree = Tree::new(Node::root(
            0,
            vec![Node::button(1, "first"), Node::button(2, "second")],
        ));
        let mut text = TextContext::new();
        let layout = tree.layout(&mut text, Viewport::new(400.0, 300.0));

        let unfocused =
            project(&tree, &layout, &FocusState::new(), &ScrollState::new(), 1.0).unwrap();
        assert_eq!(
            unfocused.focus,
            ak_id(NodeKey::first(0)),
            "nothing focused means the root, which is what AccessKit asks for"
        );

        // Two otherwise-identical buttons: only the focus answer tells the
        // projections apart, which is what makes this a real test.
        let mut focus = FocusState::new();
        focus.focus_by_keyboard(Some(NodeKey::first(2)));
        let focused = project(&tree, &layout, &focus, &ScrollState::new(), 1.0).unwrap();
        assert_eq!(focused.focus, ak_id(NodeKey::first(2)));
        assert_ne!(focused.focus, unfocused.focus);
    }

    #[test]
    fn a_scroll_reports_its_offset_and_range() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    1,
                    Node::text(2, "content").with_layout(WireLayout {
                        height: WireSize::Fixed(400),
                        ..WireLayout::default()
                    }),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(100),
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        ));
        let mut text = TextContext::new();
        let layout = tree.layout(&mut text, Viewport::new(400.0, 300.0));

        let mut scroll = ScrollState::new();
        scroll.set(NodeKey::first(1), crate::ScrollOffset::new(0, 120));
        let update = project(&tree, &layout, &FocusState::new(), &scroll, 1.0).unwrap();

        let viewport = update
            .nodes
            .iter()
            .find(|(id, _)| *id == ak_id(NodeKey::first(1)))
            .map(|(_, node)| node)
            .unwrap();
        assert_eq!(viewport.scroll_y(), Some(120.0));
        assert_eq!(viewport.scroll_y_min(), Some(0.0));
        assert_eq!(
            viewport.scroll_y_max(),
            Some(300.0),
            "400 of content in a 100 viewport leaves 300 scrollable"
        );
        assert!(viewport.supports_action(Action::ScrollIntoView));
    }

    /// Nested viewports at different offsets must be distinguishable, which
    /// they only are if bounds and scroll state are read per node rather than
    /// once for the tree.
    #[test]
    fn nested_viewports_report_their_own_offsets() {
        let stretch = |height: u16| WireLayout {
            height: WireSize::Fixed(height),
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    1,
                    Node::column(
                        2,
                        vec![
                            Node::scroll(3, Node::text(4, "inner").with_layout(stretch(300)))
                                .with_layout(stretch(80)),
                            Node::text(5, "tail").with_layout(stretch(400)),
                        ],
                    ),
                )
                .with_layout(stretch(100)),
            ],
        ));
        let mut text = TextContext::new();
        let layout = tree.layout(&mut text, Viewport::new(400.0, 300.0));

        let mut scroll = ScrollState::new();
        scroll.set(NodeKey::first(1), crate::ScrollOffset::new(0, 40));
        scroll.set(NodeKey::first(3), crate::ScrollOffset::new(0, 90));
        let update = project(&tree, &layout, &FocusState::new(), &scroll, 1.0).unwrap();

        let offset_of = |key: NodeKey| {
            update
                .nodes
                .iter()
                .find(|(id, _)| *id == ak_id(key))
                .and_then(|(_, node)| node.scroll_y())
        };
        assert_eq!(offset_of(NodeKey::first(1)), Some(40.0));
        assert_eq!(
            offset_of(NodeKey::first(3)),
            Some(90.0),
            "each viewport reports its own offset, not its ancestor's"
        );
    }
}
