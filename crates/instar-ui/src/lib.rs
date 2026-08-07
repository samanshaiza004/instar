//! Instar's retained semantic UI tree, and the interaction rules over it.
//!
//! The wire format lives in [`instar_ui_protocol`], which this crate re-exports
//! the useful parts of. The split is not cosmetic: guests link the protocol
//! crate to speak the encoding, and must never link this one. This crate is
//! where layout, hit-testing, and interaction policy live — and where a layout
//! engine will land — none of which belongs in a guest.
//!
//! # Responsibilities
//!
//! - **Assembly**: turn a decoded flat wire batch into a [`Tree`].
//! - **Semantic validation**: reject trees that are structurally decodable but
//!   meaningless (duplicate keys, text on a container).
//! - **Layout**: hold a [`LayoutSnapshot`] mapping keys to geometry.
//! - **Interaction**: hit-test, apply disabled rules, and produce
//!   [`UiAction`]s.
//!
//! # Geometry belongs to the host
//!
//! Hit-testing takes a [`LayoutSnapshot`] separate from the tree, because the
//! host owns presentation. Today the snapshot can be built from geometry a
//! guest supplied ([`LayoutSnapshot::from_wire`]) — scaffolding from WP5, kept
//! only so tests can drive interaction before a layout engine exists. When the
//! host computes layout it will produce the snapshot itself and that
//! constructor goes away. Nothing else has to change, which is exactly why the
//! two are separate types.

pub mod layout;

pub use instar_ui_protocol as protocol;
pub use instar_ui_protocol::{NodeKey, ProtocolError, WireDimension, WireLayout, limits};
pub use layout::{LayoutSnapshot, Rect, TEXT_METRICS, TextMetrics, Viewport};

use instar_ui_protocol::{BatchEncoder, WireBatch, WireEvent, WireNode, flags, opcode};

/// What a node is, semantically. Presentation is not described here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// The single outermost node, sized to the viewport.
    Root,
    /// Stacks children vertically. Not interactive.
    Column,
    /// Displays text. Not interactive. Measured by the host.
    Text { text: String },
    /// Interactive. Hit-testing resolves to these.
    Button { label: String, enabled: bool },
}

impl NodeKind {
    /// Whether this node can be the target of interaction.
    ///
    /// A disabled button is deliberately *not* interactive: the host refuses
    /// to hit it at all, rather than delivering a click and trusting the guest
    /// to re-check.
    pub fn is_interactive(&self) -> bool {
        matches!(self, NodeKind::Button { enabled: true, .. })
    }
}

/// One node in the retained tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub key: NodeKey,
    pub kind: NodeKind,
    /// Layout *intent*. The host turns this into geometry; the guest never
    /// states a rectangle.
    pub layout: WireLayout,
    pub children: Vec<Node>,
}

impl Node {
    pub fn root(key: u32, children: Vec<Node>) -> Self {
        Self {
            key: NodeKey(key),
            kind: NodeKind::Root,
            layout: WireLayout {
                width: WireDimension::Fill,
                ..WireLayout::default()
            },
            children,
        }
    }

    pub fn column(key: u32, children: Vec<Node>) -> Self {
        Self {
            key: NodeKey(key),
            kind: NodeKind::Column,
            layout: WireLayout {
                width: WireDimension::Fill,
                ..WireLayout::default()
            },
            children,
        }
    }

    pub fn text(key: u32, text: impl Into<String>) -> Self {
        Self {
            key: NodeKey(key),
            kind: NodeKind::Text { text: text.into() },
            layout: WireLayout::default(),
            children: Vec::new(),
        }
    }

    pub fn button(key: u32, label: impl Into<String>) -> Self {
        Self {
            key: NodeKey(key),
            kind: NodeKind::Button {
                label: label.into(),
                enabled: true,
            },
            layout: WireLayout::default(),
            children: Vec::new(),
        }
    }

    pub fn with_layout(mut self, layout: WireLayout) -> Self {
        self.layout = layout;
        self
    }

    pub fn disabled(mut self) -> Self {
        if let NodeKind::Button { enabled, .. } = &mut self.kind {
            *enabled = false;
        }
        self
    }
}

/// Why an otherwise-decodable batch was rejected as meaningless.
///
/// Distinct from [`ProtocolError`] on purpose: that one means "these bytes are
/// malformed", this one means "these bytes parse but describe nonsense". A
/// guest author needs to tell those apart.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("wire format error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("batch contained no nodes")]
    Empty,
    #[error("duplicate node key {0}")]
    DuplicateKey(NodeKey),
    #[error("the root node is a {0}, but a tree's root must be a root node")]
    BadRoot(&'static str),
    #[error("{0} is a root node, but only the outermost node may be one")]
    NestedRoot(NodeKey),
    #[error("{0} sets height to Fill, which Phase 1 does not support")]
    FillHeight(NodeKey),
}

/// A committed UI tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub root: Node,
}

impl Tree {
    pub fn new(root: Node) -> Self {
        Self { root }
    }

    /// Assembles a tree from a decoded wire batch, applying semantic rules the
    /// protocol layer deliberately does not.
    pub fn from_wire(batch: &WireBatch) -> Result<Self, TreeError> {
        if batch.nodes.is_empty() {
            return Err(TreeError::Empty);
        }

        let mut seen = std::collections::HashSet::with_capacity(batch.nodes.len());
        for node in &batch.nodes {
            if !seen.insert(node.key) {
                // Duplicate keys would make hit-test results ambiguous and
                // make a targeted update land on the wrong node.
                return Err(TreeError::DuplicateKey(node.key));
            }
        }

        // Structural rules the wire deliberately does not enforce: exactly one
        // root, and it is outermost. The protocol layer reports what the bytes
        // say; whether that is a sensible interface is this crate's call.
        match batch.nodes[0].kind {
            opcode::NODE_ROOT => {}
            opcode::NODE_COLUMN => return Err(TreeError::BadRoot("column")),
            opcode::NODE_TEXT => return Err(TreeError::BadRoot("text")),
            _ => return Err(TreeError::BadRoot("button")),
        }
        for node in &batch.nodes[1..] {
            if node.kind == opcode::NODE_ROOT {
                return Err(TreeError::NestedRoot(node.key));
            }
        }
        for node in &batch.nodes {
            // Phase 1's vocabulary has no fill height: a column of
            // fill-height children has no defined distribution, and choosing
            // one silently would be inventing layout semantics.
            if node.layout.height == WireDimension::Fill {
                return Err(TreeError::FillHeight(node.key));
            }
        }

        let mut cursor = 0usize;
        let root = assemble(&batch.nodes, &mut cursor)?;
        Ok(Self { root })
    }

    /// Decodes and assembles in one step.
    pub fn decode(bytes: &[u8]) -> Result<Self, TreeError> {
        Self::from_wire(&instar_ui_protocol::decode_batch(bytes)?)
    }

    /// Encodes this tree, with no layout section.
    pub fn encode(&self) -> Vec<u8> {
        let mut encoder = BatchEncoder::new();
        encode_node(&mut encoder, &self.root);
        encoder.finish()
    }

    /// Every node, pre-order.
    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        let mut stack = vec![&self.root];
        std::iter::from_fn(move || {
            let node = stack.pop()?;
            stack.extend(node.children.iter().rev());
            Some(node)
        })
    }

    pub fn find(&self, key: NodeKey) -> Option<&Node> {
        self.iter().find(|node| node.key == key)
    }

    /// Computes geometry for this tree against a logical viewport.
    ///
    /// The host owns geometry entirely: this is the only source of a
    /// [`LayoutSnapshot`], and a guest cannot supply one.
    pub fn layout(&self, viewport: Viewport) -> LayoutSnapshot {
        layout::compute(self, viewport)
    }

    /// Finds the innermost interactive node containing `(x, y)`.
    ///
    /// Depth-first, last-child-first, so a node drawn later (on top) wins over
    /// an earlier sibling it overlaps. Non-interactive nodes never match but
    /// are still descended into, so a button inside a container is reachable.
    /// Nodes absent from the snapshot have no geometry and cannot be hit.
    pub fn hit_test(&self, layout: &LayoutSnapshot, x: i32, y: i32) -> Option<&Node> {
        hit_test_node(&self.root, layout, x, y)
    }
}

fn assemble(nodes: &[WireNode], cursor: &mut usize) -> Result<Node, TreeError> {
    let wire = nodes.get(*cursor).ok_or(TreeError::Empty)?;
    *cursor += 1;

    let kind = match wire.kind {
        opcode::NODE_ROOT => NodeKind::Root,
        opcode::NODE_COLUMN => NodeKind::Column,
        opcode::NODE_TEXT => NodeKind::Text {
            text: wire.text.clone().unwrap_or_default(),
        },
        opcode::NODE_BUTTON => NodeKind::Button {
            label: wire.text.clone().unwrap_or_default(),
            enabled: wire.is_enabled(),
        },
        // Unreachable via `decode_batch`, which rejects unknown kinds; handled
        // rather than panicked because `from_wire` is public and a caller may
        // hand-build a `WireBatch`.
        value => {
            return Err(TreeError::Protocol(ProtocolError::UnknownOpcode {
                context: "node kind",
                value,
            }));
        }
    };

    let mut children = Vec::with_capacity(wire.child_count as usize);
    for _ in 0..wire.child_count {
        children.push(assemble(nodes, cursor)?);
    }

    Ok(Node {
        key: wire.key,
        kind,
        layout: wire.layout,
        children,
    })
}

fn encode_node(encoder: &mut BatchEncoder, node: &Node) {
    let (kind, text, node_flags) = match &node.kind {
        NodeKind::Root => (opcode::NODE_ROOT, None, 0),
        NodeKind::Column => (opcode::NODE_COLUMN, None, 0),
        NodeKind::Text { text } => (opcode::NODE_TEXT, Some(text.as_str()), 0),
        NodeKind::Button { label, enabled } => (
            opcode::NODE_BUTTON,
            Some(label.as_str()),
            if *enabled { flags::ENABLED } else { 0 },
        ),
    };
    encoder.node(
        kind,
        node.key,
        node_flags,
        text,
        node.layout,
        node.children.len().min(u16::MAX as usize) as u16,
    );
    for child in &node.children {
        encode_node(encoder, child);
    }
}

/// Whether `(px, py)` falls inside `rect`.
///
/// Half-open on the far edges, so adjacent rects sharing an edge cannot both
/// claim the same pixel. A non-positive extent contains nothing.
pub fn rect_contains(rect: Rect, px: i32, py: i32) -> bool {
    rect.width > 0
        && rect.height > 0
        && px >= rect.x
        && py >= rect.y
        && px < rect.x.saturating_add(rect.width)
        && py < rect.y.saturating_add(rect.height)
}

fn hit_test_node<'a>(node: &'a Node, layout: &LayoutSnapshot, x: i32, y: i32) -> Option<&'a Node> {
    let Some(rect) = layout.get(node.key) else {
        // No geometry means nothing to hit, and nothing beneath it either --
        // an unlaid-out subtree is not on screen.
        return None;
    };
    if !rect_contains(rect, x, y) {
        return None;
    }
    for child in node.children.iter().rev() {
        if let Some(hit) = hit_test_node(child, layout, x, y) {
            return Some(hit);
        }
    }
    node.kind.is_interactive().then_some(node)
}

/// A semantic outcome of interaction, for the host to act on.
///
/// This is what leaves the UI layer: not raw input, and not a guest event.
/// The host decides what to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    ButtonActivated(NodeKey),
}

impl UiAction {
    /// The guest event this action should be delivered as.
    pub fn to_event(self) -> WireEvent {
        match self {
            UiAction::ButtonActivated(node) => WireEvent::Click { node },
        }
    }

    pub fn encode(self) -> Vec<u8> {
        self.to_event().encode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A counter-shaped tree: root > column > (text, button, disabled button).
    /// The guest supplies no geometry at all.
    fn sample() -> Tree {
        Tree::new(Node::root(
            0,
            vec![Node::column(
                1,
                vec![
                    Node::text(2, "Clicked 0 times"),
                    Node::button(3, "Press me"),
                    Node::button(4, "Reset").disabled(),
                ],
            )],
        ))
    }

    const VIEWPORT: Viewport = Viewport::new(400.0, 300.0);

    #[test]
    fn round_trips_through_the_wire() {
        let tree = sample();
        assert_eq!(Tree::decode(&tree.encode()).unwrap(), tree);
    }

    /// The exit gate for WP7A: the guest provides zero geometry, and the host
    /// still produces a usable snapshot.
    #[test]
    fn the_host_computes_geometry_the_guest_never_supplied() {
        let tree = sample();
        let layout = tree.layout(VIEWPORT);

        for key in [0, 1, 2, 3, 4] {
            assert!(
                layout.get(NodeKey(key)).is_some(),
                "every node should have geometry, {key} did not"
            );
        }

        let root = layout.get(NodeKey(0)).unwrap();
        assert_eq!(
            (root.width, root.height),
            (400, 300),
            "the root fills the viewport"
        );
    }

    /// Layout is a pure function of tree and viewport.
    #[test]
    fn layout_is_deterministic() {
        let tree = sample();
        assert_eq!(tree.layout(VIEWPORT), tree.layout(VIEWPORT));
    }

    /// Assertions are relative rather than exact: the text measurement is a
    /// placeholder that a real font stack will replace, and these should
    /// survive that.
    #[test]
    fn a_column_stacks_its_children_without_overlap() {
        let layout = sample().layout(VIEWPORT);
        let text = layout.get(NodeKey(2)).unwrap();
        let press = layout.get(NodeKey(3)).unwrap();
        let reset = layout.get(NodeKey(4)).unwrap();

        assert!(
            text.y < press.y && press.y < reset.y,
            "children stack in declaration order: {text:?} {press:?} {reset:?}"
        );
        assert!(
            text.y + text.height <= press.y,
            "stacked children must not overlap"
        );
        assert!(
            press.y + press.height <= reset.y,
            "stacked children must not overlap"
        );
    }

    #[test]
    fn children_are_contained_by_their_parent() {
        let layout = sample().layout(VIEWPORT);
        let root = layout.get(NodeKey(0)).unwrap();
        for key in [1, 2, 3, 4] {
            let child = layout.get(NodeKey(key)).unwrap();
            assert!(
                child.x >= root.x
                    && child.y >= root.y
                    && child.x + child.width <= root.x + root.width
                    && child.y + child.height <= root.y + root.height,
                "node {key} ({child:?}) escaped the root ({root:?})"
            );
        }
    }

    #[test]
    fn padding_insets_children() {
        let padded = Tree::new(
            Node::root(0, vec![Node::text(1, "hi")]).with_layout(WireLayout {
                width: WireDimension::Fill,
                height: WireDimension::Content,
                padding: 20,
                gap: 0,
            }),
        );
        let layout = padded.layout(VIEWPORT);
        let child = layout.get(NodeKey(1)).unwrap();
        assert_eq!(
            (child.x, child.y),
            (20, 20),
            "a padded parent should inset its child by the padding"
        );
    }

    #[test]
    fn gap_separates_siblings() {
        let build = |gap: u16| {
            Tree::new(Node::root(
                0,
                vec![
                    Node::column(1, vec![Node::text(2, "a"), Node::text(3, "b")]).with_layout(
                        WireLayout {
                            width: WireDimension::Fill,
                            height: WireDimension::Content,
                            padding: 0,
                            gap,
                        },
                    ),
                ],
            ))
        };
        let tight = build(0).layout(VIEWPORT);
        let loose = build(10).layout(VIEWPORT);

        let tight_second = tight.get(NodeKey(3)).unwrap();
        let loose_second = loose.get(NodeKey(3)).unwrap();
        assert_eq!(
            loose_second.y - tight_second.y,
            10,
            "a 10px gap should push the second child down by exactly 10px"
        );
    }

    #[test]
    fn a_fixed_size_is_honoured_exactly() {
        let tree = Tree::new(Node::root(
            0,
            vec![Node::button(1, "x").with_layout(WireLayout {
                width: WireDimension::Fixed(120),
                height: WireDimension::Fixed(40),
                padding: 0,
                gap: 0,
            })],
        ));
        let button = tree.layout(VIEWPORT).get(NodeKey(1)).unwrap();
        assert_eq!((button.width, button.height), (120, 40));
    }

    #[test]
    fn fill_width_is_wider_than_content_width() {
        let build = |width: WireDimension| {
            Tree::new(Node::root(
                0,
                vec![Node::button(1, "x").with_layout(WireLayout {
                    width,
                    height: WireDimension::Content,
                    padding: 0,
                    gap: 0,
                })],
            ))
        };
        let content = build(WireDimension::Content)
            .layout(VIEWPORT)
            .get(NodeKey(1))
            .unwrap();
        let fill = build(WireDimension::Fill)
            .layout(VIEWPORT)
            .get(NodeKey(1))
            .unwrap();
        assert!(
            fill.width > content.width,
            "fill ({}) should exceed content ({}) for a one-character label",
            fill.width,
            content.width
        );
    }

    #[test]
    fn longer_text_measures_wider() {
        let build = |text: &str| Tree::new(Node::root(0, vec![Node::text(1, text)]));
        let short = build("hi").layout(VIEWPORT).get(NodeKey(1)).unwrap();
        let long = build("a much longer line of text")
            .layout(VIEWPORT)
            .get(NodeKey(1))
            .unwrap();
        assert!(
            long.width > short.width,
            "intrinsic measurement should make longer text wider"
        );
    }

    #[test]
    fn a_narrower_viewport_narrows_filled_nodes() {
        let tree = sample();
        let wide = tree.layout(Viewport::new(800.0, 300.0));
        let narrow = tree.layout(Viewport::new(200.0, 300.0));
        assert!(
            narrow.get(NodeKey(1)).unwrap().width < wide.get(NodeKey(1)).unwrap().width,
            "a filled column should follow the viewport"
        );
    }

    // --- Hit-testing, against host-computed geometry. ---

    #[test]
    fn hit_test_finds_the_button_the_host_placed() {
        let tree = sample();
        let layout = tree.layout(VIEWPORT);
        let button = layout.get(NodeKey(3)).unwrap();

        let hit = tree.hit_test(&layout, button.x + 1, button.y + 1);
        assert_eq!(
            hit.map(|n| n.key),
            Some(NodeKey(3)),
            "a point inside the button's computed rect should hit it"
        );
    }

    #[test]
    fn a_disabled_button_is_not_hit() {
        let tree = sample();
        let layout = tree.layout(VIEWPORT);
        let reset = layout.get(NodeKey(4)).unwrap();
        assert_eq!(
            tree.hit_test(&layout, reset.x + 1, reset.y + 1),
            None,
            "a disabled button must not be hit-testable even though it has geometry"
        );
    }

    #[test]
    fn text_is_not_interactive() {
        let tree = sample();
        let layout = tree.layout(VIEWPORT);
        let text = layout.get(NodeKey(2)).unwrap();
        assert_eq!(tree.hit_test(&layout, text.x + 1, text.y + 1), None);
    }

    #[test]
    fn a_point_outside_everything_hits_nothing() {
        let tree = sample();
        let layout = tree.layout(VIEWPORT);
        assert_eq!(tree.hit_test(&layout, 5_000, 5_000), None);
    }

    #[test]
    fn a_node_without_layout_cannot_be_hit() {
        let tree = sample();
        // A snapshot missing the button entirely: nothing to hit.
        let layout = LayoutSnapshot::from_rects([(NodeKey(0), Rect::new(0, 0, 400, 300))]);
        assert_eq!(tree.hit_test(&layout, 10, 10), None);
    }

    #[test]
    fn edges_are_half_open() {
        let rect = Rect::new(10, 10, 10, 10);
        assert!(rect_contains(rect, 10, 10), "origin is inside");
        assert!(rect_contains(rect, 19, 19), "last pixel is inside");
        assert!(!rect_contains(rect, 20, 15), "far edge is outside");
        assert!(!rect_contains(rect, 15, 20), "far edge is outside");
        assert!(
            !rect_contains(Rect::new(0, 0, 0, 0), 0, 0),
            "empty is empty"
        );
    }

    #[test]
    fn actions_become_guest_events() {
        assert_eq!(
            UiAction::ButtonActivated(NodeKey(2)).to_event(),
            WireEvent::Click { node: NodeKey(2) }
        );
    }

    // --- Semantic validation, distinct from wire validation. ---

    #[test]
    fn rejects_duplicate_keys() {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                NodeKey(1),
                0,
                None,
                WireLayout::default(),
                1,
            )
            .node(
                opcode::NODE_TEXT,
                NodeKey(1),
                0,
                Some("same key"),
                WireLayout::default(),
                0,
            );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::DuplicateKey(NodeKey(1)))
        );
    }

    #[test]
    fn rejects_a_tree_whose_root_is_not_a_root_node() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_COLUMN,
            NodeKey(0),
            0,
            None,
            WireLayout::default(),
            0,
        );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::BadRoot("column"))
        );
    }

    #[test]
    fn rejects_a_nested_root() {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                NodeKey(0),
                0,
                None,
                WireLayout::default(),
                1,
            )
            .node(
                opcode::NODE_ROOT,
                NodeKey(1),
                0,
                None,
                WireLayout::default(),
                0,
            );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::NestedRoot(NodeKey(1)))
        );
    }

    /// Phase 1 has no fill height: a column of fill-height children has no
    /// defined distribution, and picking one silently would be inventing
    /// layout semantics rather than implementing them.
    #[test]
    fn rejects_fill_height() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROOT,
            NodeKey(0),
            0,
            None,
            WireLayout {
                width: WireDimension::Fill,
                height: WireDimension::Fill,
                padding: 0,
                gap: 0,
            },
            0,
        );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::FillHeight(NodeKey(0)))
        );
    }

    #[test]
    fn wire_errors_surface_as_protocol_errors() {
        assert!(matches!(
            Tree::decode(b"nope"),
            Err(TreeError::Protocol(ProtocolError::BadMagic))
        ));
    }
}

/// Pointer interaction state for one surface.
///
/// Lives here rather than in the host because it is presentation behaviour:
/// deciding that a press followed by a release *over the same node* is an
/// activation is a UI rule, not an orchestration one. Per docs/PHASE-1.md's
/// layering, `instar-host` routes; `instar-ui` decides what input means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interaction {
    /// The node a press landed on, if any. Cleared on release.
    pressed: Option<NodeKey>,
}

impl Interaction {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pressed(&self) -> Option<NodeKey> {
        self.pressed
    }

    /// A press landed at `(x, y)`. Records the target; activates nothing.
    ///
    /// Activation deliberately waits for the release, matching every desktop
    /// convention: pressing a button and dragging away must not activate it.
    pub fn on_press(&mut self, tree: &Tree, layout: &LayoutSnapshot, x: i32, y: i32) {
        self.pressed = tree.hit_test(layout, x, y).map(|node| node.key);
    }

    /// A release landed at `(x, y)`.
    ///
    /// Activates only when the release lands on the same node the press did.
    /// Releasing elsewhere cancels, which is what lets a user change their
    /// mind mid-click.
    pub fn on_release(
        &mut self,
        tree: &Tree,
        layout: &LayoutSnapshot,
        x: i32,
        y: i32,
    ) -> Option<UiAction> {
        let pressed = self.pressed.take()?;
        let released_on = tree.hit_test(layout, x, y)?.key;
        (released_on == pressed).then_some(UiAction::ButtonActivated(pressed))
    }

    /// Abandons any in-progress press.
    ///
    /// The host calls this when geometry is invalidated: the press was
    /// recorded against a layout that no longer exists, so completing it could
    /// activate a node that has since moved elsewhere.
    pub fn cancel(&mut self) {
        self.pressed = None;
    }
}
