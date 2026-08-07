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

pub use instar_ui_protocol as protocol;
pub use instar_ui_protocol::{NodeKey, ProtocolError, WireRect as Rect, limits};

use std::collections::HashMap;

use instar_ui_protocol::{BatchEncoder, WireBatch, WireEvent, WireNode, flags, opcode};

/// What a node is, semantically. Presentation is not described here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// Groups children. Not interactive.
    Container,
    /// Displays text. Not interactive.
    Label { text: String },
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
    pub children: Vec<Node>,
}

impl Node {
    pub fn container(key: u32, children: Vec<Node>) -> Self {
        Self {
            key: NodeKey(key),
            kind: NodeKind::Container,
            children,
        }
    }

    pub fn label(key: u32, text: impl Into<String>) -> Self {
        Self {
            key: NodeKey(key),
            kind: NodeKind::Label { text: text.into() },
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
            children: Vec::new(),
        }
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
    #[error("layout snapshot names {0}, which is not in the tree")]
    LayoutForUnknownNode(NodeKey),
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
        opcode::NODE_CONTAINER => NodeKind::Container,
        opcode::NODE_LABEL => NodeKind::Label {
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
        children,
    })
}

fn encode_node(encoder: &mut BatchEncoder, node: &Node) {
    let (kind, text, node_flags) = match &node.kind {
        NodeKind::Container => (opcode::NODE_CONTAINER, None, 0),
        NodeKind::Label { text } => (opcode::NODE_LABEL, Some(text.as_str()), 0),
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
        node.children.len().min(u16::MAX as usize) as u16,
    );
    for child in &node.children {
        encode_node(encoder, child);
    }
}

/// Where each node is, in logical pixels.
///
/// Owned by the host and kept separate from the tree on purpose: the host owns
/// presentation, and a guest that dictated its own geometry would become
/// authoritative over something it should not be.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutSnapshot {
    rects: HashMap<NodeKey, Rect>,
}

impl LayoutSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: NodeKey, rect: Rect) -> &mut Self {
        self.rects.insert(key, rect);
        self
    }

    pub fn get(&self, key: NodeKey) -> Option<Rect> {
        self.rects.get(&key).copied()
    }

    pub fn len(&self) -> usize {
        self.rects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Builds a snapshot from geometry a *guest* supplied.
    ///
    /// **Test scaffolding, scheduled for removal.** It exists only because WP5
    /// proved the interaction round-trip before a layout engine existed. Once
    /// the host computes layout, it constructs the snapshot itself and this
    /// goes away; keeping it beyond that point would leave the guest
    /// authoritative over geometry, which is exactly the outcome the
    /// tree/snapshot split is meant to prevent.
    ///
    /// Validates that every entry names a node that exists, so a guest cannot
    /// smuggle in geometry for nodes it never declared.
    pub fn from_wire(batch: &WireBatch, tree: &Tree) -> Result<Self, TreeError> {
        let mut snapshot = Self::new();
        for (key, rect) in &batch.layout {
            if tree.find(*key).is_none() {
                return Err(TreeError::LayoutForUnknownNode(*key));
            }
            snapshot.insert(*key, *rect);
        }
        Ok(snapshot)
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

    fn sample() -> (Tree, LayoutSnapshot) {
        let tree = Tree::new(Node::container(
            0,
            vec![
                Node::label(1, "Clicked 0 times"),
                Node::button(2, "Press me"),
            ],
        ));
        let mut layout = LayoutSnapshot::new();
        layout
            .insert(NodeKey(0), Rect::new(0, 0, 200, 100))
            .insert(NodeKey(1), Rect::new(10, 10, 180, 20))
            .insert(NodeKey(2), Rect::new(10, 40, 100, 30));
        (tree, layout)
    }

    #[test]
    fn round_trips_through_the_wire() {
        let (tree, _) = sample();
        assert_eq!(Tree::decode(&tree.encode()).unwrap(), tree);
    }

    #[test]
    fn hit_test_finds_the_button_and_nothing_else() {
        let (tree, layout) = sample();
        assert_eq!(
            tree.hit_test(&layout, 20, 50).map(|n| n.key),
            Some(NodeKey(2))
        );
        // Inside the label: not interactive.
        assert_eq!(tree.hit_test(&layout, 20, 15).map(|n| n.key), None);
        // Inside the container but on no child.
        assert_eq!(tree.hit_test(&layout, 150, 80).map(|n| n.key), None);
        // Outside entirely.
        assert_eq!(tree.hit_test(&layout, 500, 500).map(|n| n.key), None);
    }

    #[test]
    fn a_disabled_button_is_not_hit() {
        let tree = Tree::new(Node::container(0, vec![Node::button(1, "no").disabled()]));
        let mut layout = LayoutSnapshot::new();
        layout
            .insert(NodeKey(0), Rect::new(0, 0, 100, 100))
            .insert(NodeKey(1), Rect::new(0, 0, 50, 50));
        assert_eq!(tree.hit_test(&layout, 10, 10), None);
    }

    #[test]
    fn a_node_without_layout_cannot_be_hit() {
        let (tree, mut layout) = sample();
        layout = {
            let mut trimmed = LayoutSnapshot::new();
            trimmed.insert(NodeKey(0), layout.get(NodeKey(0)).unwrap());
            trimmed
        };
        assert_eq!(
            tree.hit_test(&layout, 20, 50),
            None,
            "a button with no geometry is not on screen"
        );
    }

    #[test]
    fn later_siblings_win_when_overlapping() {
        let tree = Tree::new(Node::container(
            0,
            vec![Node::button(1, "under"), Node::button(2, "over")],
        ));
        let mut layout = LayoutSnapshot::new();
        layout
            .insert(NodeKey(0), Rect::new(0, 0, 100, 100))
            .insert(NodeKey(1), Rect::new(0, 0, 50, 50))
            .insert(NodeKey(2), Rect::new(0, 0, 50, 50));
        assert_eq!(
            tree.hit_test(&layout, 10, 10).map(|n| n.key),
            Some(NodeKey(2))
        );
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
            .node(opcode::NODE_CONTAINER, NodeKey(1), 0, None, 1)
            .node(opcode::NODE_LABEL, NodeKey(1), 0, Some("same key"), 0);
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::DuplicateKey(NodeKey(1)))
        );
    }

    #[test]
    fn rejects_layout_for_nodes_that_do_not_exist() {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_CONTAINER, NodeKey(0), 0, None, 0)
            .layout_entry(NodeKey(99), Rect::new(0, 0, 10, 10));
        let batch = instar_ui_protocol::decode_batch(&encoder.finish()).unwrap();
        let tree = Tree::from_wire(&batch).unwrap();
        assert_eq!(
            LayoutSnapshot::from_wire(&batch, &tree),
            Err(TreeError::LayoutForUnknownNode(NodeKey(99)))
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
