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
) -> Option<TreeUpdate> {
    if !is_presented(&tree.root) {
        return None;
    }
    let root_id = ak_id(tree.root.key);

    let mut nodes = Vec::new();
    build(&tree.root, layout, scroll, &mut nodes);

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
    out: &mut Vec<(NodeId, AccessNode)>,
) {
    let mut access = AccessNode::new(role_of(&node.kind));

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
        build(child, layout, scroll, out);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeKey, TextContext, Viewport, WireAlign, WireLayout, WireSize};

    fn projected(tree: &Tree) -> (TreeUpdate, LayoutSnapshot) {
        let mut text = TextContext::new();
        let layout = tree.layout(&mut text, Viewport::new(400.0, 300.0));
        let update = project(tree, &layout, &FocusState::new(), &ScrollState::new())
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
            project(&tree, &layout, &FocusState::new(), &ScrollState::new()).expect("projects");
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

        let unfocused = project(&tree, &layout, &FocusState::new(), &ScrollState::new()).unwrap();
        assert_eq!(
            unfocused.focus,
            ak_id(NodeKey::first(0)),
            "nothing focused means the root, which is what AccessKit asks for"
        );

        // Two otherwise-identical buttons: only the focus answer tells the
        // projections apart, which is what makes this a real test.
        let mut focus = FocusState::new();
        focus.focus_by_keyboard(Some(NodeKey::first(2)));
        let focused = project(&tree, &layout, &focus, &ScrollState::new()).unwrap();
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
        let update = project(&tree, &layout, &FocusState::new(), &scroll).unwrap();

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
        let update = project(&tree, &layout, &FocusState::new(), &scroll).unwrap();

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
