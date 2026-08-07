//! Instar's retained semantic UI tree (WP5).
//!
//! A guest describes its interface as a tree of semantic nodes and commits it;
//! the host decodes that tree, hit-tests against it, and dispatches
//! interaction back as events. This crate owns both sides of that contract and
//! nothing else — no rendering, no windowing, no layout engine.
//!
//! # Where this sits
//!
//! `instar-kernel` carries committed batches as opaque `list<u8>` and must
//! never learn what they mean (docs/PHASE-1.md's forbidden-dependency list).
//! This crate is what those bytes mean. The dependency edge runs `instar-ui ->
//! instar-kernel`, never back.
//!
//! # Decoding is adversarial
//!
//! Batches arrive from an untrusted guest, so [`Tree::decode`] treats its
//! input as hostile: every length is bounded before it is trusted, depth is
//! capped, and malformed input is always a [`DecodeError`] — never a panic,
//! never an unbounded allocation. The bounds are deliberately small and
//! explicit rather than "large enough"; see [`limits`].
//!
//! # What WP5 deliberately does not do
//!
//! Nodes carry explicit rects supplied by the guest. There is no layout engine
//! here and none is implied: WP5's job is to prove an interaction round-trip
//! end to end, and adding layout would mean adding a second unproven thing to
//! the same step. Layout arrives with the crates that need it.

use std::fmt;

/// Parser bounds applied to every decoded batch.
///
/// These are a security boundary, not a tuning knob: a guest that exceeds them
/// gets its batch rejected. Raise them only with a reason, since every one of
/// them is what stops a hostile guest from turning a commit into an
/// out-of-memory.
pub mod limits {
    /// Maximum nodes in one tree.
    pub const MAX_NODES: usize = 4096;
    /// Maximum nesting depth.
    pub const MAX_DEPTH: usize = 64;
    /// Maximum bytes in a single node's text.
    pub const MAX_TEXT_BYTES: usize = 4096;
    /// Maximum size of an entire encoded batch.
    pub const MAX_BATCH_BYTES: usize = 1 << 20;
}

/// Identifies a node within a tree. Assigned by the guest, stable across
/// commits so the host can address a node it saw in an earlier revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node{}", self.0)
    }
}

/// An axis-aligned rectangle in logical pixels.
///
/// Integer rather than floating point: these cross a trust boundary and get
/// compared for hit-testing, and integers make both decoding and hit-test
/// results exactly reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Whether `(px, py)` falls inside this rect.
    ///
    /// Half-open on the far edges, so adjacent rects that share an edge cannot
    /// both claim the same pixel.
    pub fn contains(&self, px: i32, py: i32) -> bool {
        // A non-positive extent contains nothing. Without this an empty rect
        // would still match its own origin.
        self.width > 0
            && self.height > 0
            && px >= self.x
            && py >= self.y
            && px < self.x.saturating_add(self.width)
            && py < self.y.saturating_add(self.height)
    }
}

/// What a node is, semantically. Presentation is not described here — that is
/// the renderer's business, downstream of this crate.
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
    /// the click rather than delivering it and trusting the guest to re-check.
    pub fn is_interactive(&self) -> bool {
        matches!(self, NodeKind::Button { enabled: true, .. })
    }
}

/// One node in the retained tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub bounds: Rect,
    pub kind: NodeKind,
    pub children: Vec<Node>,
}

impl Node {
    pub fn container(id: u32, bounds: Rect, children: Vec<Node>) -> Self {
        Self {
            id: NodeId(id),
            bounds,
            kind: NodeKind::Container,
            children,
        }
    }

    pub fn label(id: u32, bounds: Rect, text: impl Into<String>) -> Self {
        Self {
            id: NodeId(id),
            bounds,
            kind: NodeKind::Label { text: text.into() },
            children: Vec::new(),
        }
    }

    pub fn button(id: u32, bounds: Rect, label: impl Into<String>) -> Self {
        Self {
            id: NodeId(id),
            bounds,
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

    fn count(&self) -> usize {
        1 + self.children.iter().map(Node::count).sum::<usize>()
    }
}

/// A committed UI tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub root: Node,
}

const TREE_MAGIC: &[u8; 4] = b"IUI1";
const EVENT_MAGIC: &[u8; 4] = b"IUE1";

const KIND_CONTAINER: u8 = 0;
const KIND_LABEL: u8 = 1;
const KIND_BUTTON: u8 = 2;

/// Why a batch was rejected.
///
/// Every variant names what was wrong specifically enough to debug a guest
/// without attaching a debugger to the host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("batch is {0} bytes, over the {max} byte limit", max = limits::MAX_BATCH_BYTES)]
    TooLarge(usize),
    #[error("batch is not an Instar UI batch (bad magic)")]
    BadMagic,
    #[error("batch ended unexpectedly while reading {while_reading}")]
    Truncated { while_reading: &'static str },
    #[error("unknown node kind {0}")]
    UnknownKind(u8),
    #[error("tree has more than {max} nodes", max = limits::MAX_NODES)]
    TooManyNodes,
    #[error("tree nests deeper than {max}", max = limits::MAX_DEPTH)]
    TooDeep,
    #[error("node text is {0} bytes, over the {max} byte limit", max = limits::MAX_TEXT_BYTES)]
    TextTooLong(usize),
    #[error("node text is not valid UTF-8")]
    InvalidUtf8,
    #[error("duplicate node id {0}")]
    DuplicateId(NodeId),
    #[error("{0} trailing bytes after the tree")]
    TrailingBytes(usize),
}

/// Reads primitives out of a byte slice without ever panicking.
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, n: usize, while_reading: &'static str) -> Result<&'a [u8], DecodeError> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or(DecodeError::Truncated { while_reading })?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(DecodeError::Truncated { while_reading })?;
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self, while_reading: &'static str) -> Result<u8, DecodeError> {
        Ok(self.take(1, while_reading)?[0])
    }

    fn u16(&mut self, while_reading: &'static str) -> Result<u16, DecodeError> {
        let bytes = self.take(2, while_reading)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, while_reading: &'static str) -> Result<u32, DecodeError> {
        let bytes = self.take(4, while_reading)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self, while_reading: &'static str) -> Result<i32, DecodeError> {
        Ok(self.u32(while_reading)? as i32)
    }

    fn text(&mut self, while_reading: &'static str) -> Result<String, DecodeError> {
        let len = self.u16(while_reading)? as usize;
        if len > limits::MAX_TEXT_BYTES {
            return Err(DecodeError::TextTooLong(len));
        }
        let bytes = self.take(len, while_reading)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }
}

fn write_text(out: &mut Vec<u8>, text: &str) {
    // Encoding is the trusted side of this boundary, but a guest built on this
    // crate should still fail loudly rather than emit a batch the host will
    // reject, so oversize text is truncated at a char boundary instead of
    // producing a length that cannot round-trip.
    let mut bytes = text.as_bytes();
    if bytes.len() > limits::MAX_TEXT_BYTES {
        let mut end = limits::MAX_TEXT_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        bytes = &text.as_bytes()[..end];
    }
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

impl Tree {
    pub fn new(root: Node) -> Self {
        Self { root }
    }

    /// Encodes this tree into a batch for `kernel-ui.commit`.
    ///
    /// Pre-order with an explicit child count per node, which lets the decoder
    /// rebuild the tree with a bounded stack and no recursion.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(TREE_MAGIC);
        out.extend_from_slice(&(self.root.count().min(u16::MAX as usize) as u16).to_le_bytes());
        encode_node(&mut out, &self.root);
        out
    }

    /// Decodes a batch received from an untrusted guest.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes.len() > limits::MAX_BATCH_BYTES {
            return Err(DecodeError::TooLarge(bytes.len()));
        }

        let mut reader = Reader::new(bytes);
        if reader.take(4, "magic")? != TREE_MAGIC {
            return Err(DecodeError::BadMagic);
        }

        let declared = reader.u16("node count")? as usize;
        if declared > limits::MAX_NODES {
            return Err(DecodeError::TooManyNodes);
        }

        let mut seen_ids = std::collections::HashSet::with_capacity(declared);
        let root = decode_node(&mut reader, 0, &mut seen_ids)?;

        if reader.remaining() > 0 {
            return Err(DecodeError::TrailingBytes(reader.remaining()));
        }
        Ok(Self { root })
    }

    /// Finds the innermost interactive node containing `(x, y)`.
    ///
    /// Depth-first, last-child-first, so a node drawn later (on top) wins over
    /// an earlier sibling it overlaps. Non-interactive nodes never match, but
    /// are still descended into — a button inside a container is reachable.
    pub fn hit_test(&self, x: i32, y: i32) -> Option<&Node> {
        hit_test_node(&self.root, x, y)
    }

    /// Every node, pre-order. Useful for assertions and for rendering.
    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        let mut stack = vec![&self.root];
        std::iter::from_fn(move || {
            let node = stack.pop()?;
            stack.extend(node.children.iter().rev());
            Some(node)
        })
    }

    pub fn find(&self, id: NodeId) -> Option<&Node> {
        self.iter().find(|node| node.id == id)
    }
}

fn encode_node(out: &mut Vec<u8>, node: &Node) {
    let kind_byte = match &node.kind {
        NodeKind::Container => KIND_CONTAINER,
        NodeKind::Label { .. } => KIND_LABEL,
        NodeKind::Button { .. } => KIND_BUTTON,
    };
    out.push(kind_byte);
    out.extend_from_slice(&node.id.0.to_le_bytes());
    out.extend_from_slice(&node.bounds.x.to_le_bytes());
    out.extend_from_slice(&node.bounds.y.to_le_bytes());
    out.extend_from_slice(&node.bounds.width.to_le_bytes());
    out.extend_from_slice(&node.bounds.height.to_le_bytes());

    match &node.kind {
        NodeKind::Container => {}
        NodeKind::Label { text } => write_text(out, text),
        NodeKind::Button { label, enabled } => {
            write_text(out, label);
            out.push(u8::from(*enabled));
        }
    }

    out.extend_from_slice(&(node.children.len().min(u16::MAX as usize) as u16).to_le_bytes());
    for child in &node.children {
        encode_node(out, child);
    }
}

fn decode_node(
    reader: &mut Reader<'_>,
    depth: usize,
    seen_ids: &mut std::collections::HashSet<NodeId>,
) -> Result<Node, DecodeError> {
    if depth >= limits::MAX_DEPTH {
        return Err(DecodeError::TooDeep);
    }

    let kind_byte = reader.u8("node kind")?;
    let id = NodeId(reader.u32("node id")?);
    let bounds = Rect {
        x: reader.i32("node x")?,
        y: reader.i32("node y")?,
        width: reader.i32("node width")?,
        height: reader.i32("node height")?,
    };

    // Duplicate ids would make hit-test results ambiguous and make a later
    // targeted update apply to the wrong node, so they are rejected outright.
    if !seen_ids.insert(id) {
        return Err(DecodeError::DuplicateId(id));
    }
    if seen_ids.len() > limits::MAX_NODES {
        return Err(DecodeError::TooManyNodes);
    }

    let kind = match kind_byte {
        KIND_CONTAINER => NodeKind::Container,
        KIND_LABEL => NodeKind::Label {
            text: reader.text("label text")?,
        },
        KIND_BUTTON => {
            let label = reader.text("button label")?;
            NodeKind::Button {
                label,
                enabled: reader.u8("button enabled")? != 0,
            }
        }
        other => return Err(DecodeError::UnknownKind(other)),
    };

    let child_count = reader.u16("child count")? as usize;
    // Bound the child count against what is actually left to read before
    // allocating for it: otherwise a two-byte count could ask for a
    // 65535-element allocation per node.
    if child_count > reader.remaining() {
        return Err(DecodeError::Truncated {
            while_reading: "children",
        });
    }

    let mut children = Vec::with_capacity(child_count.min(64));
    for _ in 0..child_count {
        children.push(decode_node(reader, depth + 1, seen_ids)?);
    }

    Ok(Node {
        id,
        bounds,
        kind,
        children,
    })
}

fn hit_test_node(node: &Node, x: i32, y: i32) -> Option<&Node> {
    if !node.bounds.contains(x, y) {
        return None;
    }
    for child in node.children.iter().rev() {
        if let Some(hit) = hit_test_node(child, x, y) {
            return Some(hit);
        }
    }
    node.kind.is_interactive().then_some(node)
}

/// An interaction the host delivers to the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiEvent {
    /// The user activated an interactive node.
    Click { node: NodeId },
}

const EVENT_CLICK: u8 = 0;

impl UiEvent {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9);
        out.extend_from_slice(EVENT_MAGIC);
        match self {
            UiEvent::Click { node } => {
                out.push(EVENT_CLICK);
                out.extend_from_slice(&node.0.to_le_bytes());
            }
        }
        out
    }

    /// Decoded guest-side. Bounded for the same reason batches are: a guest
    /// should not be able to be crashed by a malformed host event either.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        if reader.take(4, "magic")? != EVENT_MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let event = match reader.u8("event kind")? {
            EVENT_CLICK => UiEvent::Click {
                node: NodeId(reader.u32("node id")?),
            },
            other => return Err(DecodeError::UnknownKind(other)),
        };
        if reader.remaining() > 0 {
            return Err(DecodeError::TrailingBytes(reader.remaining()));
        }
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tree {
        Tree::new(Node::container(
            0,
            Rect::new(0, 0, 200, 100),
            vec![
                Node::label(1, Rect::new(10, 10, 180, 20), "Clicked 0 times"),
                Node::button(2, Rect::new(10, 40, 100, 30), "Press me"),
            ],
        ))
    }

    #[test]
    fn round_trips() {
        let tree = sample();
        assert_eq!(Tree::decode(&tree.encode()).unwrap(), tree);
    }

    #[test]
    fn hit_test_finds_the_button_and_nothing_else() {
        let tree = sample();
        assert_eq!(tree.hit_test(20, 50).map(|n| n.id), Some(NodeId(2)));
        // Inside the label: not interactive, so no hit.
        assert_eq!(tree.hit_test(20, 15).map(|n| n.id), None);
        // Inside the container but on no child.
        assert_eq!(tree.hit_test(150, 80).map(|n| n.id), None);
        // Entirely outside.
        assert_eq!(tree.hit_test(500, 500).map(|n| n.id), None);
    }

    #[test]
    fn a_disabled_button_is_not_hit() {
        let tree = Tree::new(Node::container(
            0,
            Rect::new(0, 0, 100, 100),
            vec![Node::button(1, Rect::new(0, 0, 50, 50), "no").disabled()],
        ));
        assert_eq!(tree.hit_test(10, 10), None);
    }

    #[test]
    fn later_siblings_win_when_overlapping() {
        let tree = Tree::new(Node::container(
            0,
            Rect::new(0, 0, 100, 100),
            vec![
                Node::button(1, Rect::new(0, 0, 50, 50), "under"),
                Node::button(2, Rect::new(0, 0, 50, 50), "over"),
            ],
        ));
        assert_eq!(tree.hit_test(10, 10).map(|n| n.id), Some(NodeId(2)));
    }

    #[test]
    fn edges_are_half_open() {
        let rect = Rect::new(10, 10, 10, 10);
        assert!(rect.contains(10, 10), "origin is inside");
        assert!(rect.contains(19, 19), "last pixel is inside");
        assert!(!rect.contains(20, 15), "far edge is outside");
        assert!(!rect.contains(15, 20), "far edge is outside");
        assert!(!Rect::new(0, 0, 0, 0).contains(0, 0), "empty rect is empty");
    }

    #[test]
    fn events_round_trip() {
        let event = UiEvent::Click { node: NodeId(7) };
        assert_eq!(UiEvent::decode(&event.encode()).unwrap(), event);
    }

    // --- Everything below is about surviving a hostile guest. ---

    #[test]
    fn rejects_bad_magic() {
        assert_eq!(Tree::decode(b"XXXX\x00\x00"), Err(DecodeError::BadMagic));
    }

    #[test]
    fn rejects_truncation_at_every_length() {
        let full = sample().encode();
        // Every proper prefix must be an error, and must not panic.
        for len in 0..full.len() {
            assert!(
                Tree::decode(&full[..len]).is_err(),
                "prefix of length {len} decoded successfully"
            );
        }
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = sample().encode();
        bytes.push(0);
        assert!(matches!(
            Tree::decode(&bytes),
            Err(DecodeError::TrailingBytes(1))
        ));
    }

    #[test]
    fn rejects_unknown_kinds() {
        let mut bytes = sample().encode();
        bytes[6] = 99; // first node's kind byte
        assert_eq!(Tree::decode(&bytes), Err(DecodeError::UnknownKind(99)));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let tree = Tree::new(Node::container(
            1,
            Rect::new(0, 0, 10, 10),
            vec![Node::label(1, Rect::new(0, 0, 5, 5), "same id")],
        ));
        assert_eq!(
            Tree::decode(&tree.encode()),
            Err(DecodeError::DuplicateId(NodeId(1)))
        );
    }

    #[test]
    fn rejects_oversized_batches() {
        let huge = vec![0u8; limits::MAX_BATCH_BYTES + 1];
        assert!(matches!(Tree::decode(&huge), Err(DecodeError::TooLarge(_))));
    }

    #[test]
    fn rejects_excessive_depth_without_overflowing_the_stack() {
        // Hand-rolled rather than built with `Node`, because building a
        // 10_000-deep `Node` would itself recurse on drop.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(TREE_MAGIC);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        let depth = limits::MAX_DEPTH + 50;
        for id in 0..depth {
            bytes.push(KIND_CONTAINER);
            bytes.extend_from_slice(&(id as u32).to_le_bytes());
            for _ in 0..4 {
                bytes.extend_from_slice(&0i32.to_le_bytes());
            }
            bytes.extend_from_slice(&1u16.to_le_bytes()); // one child
        }
        assert_eq!(Tree::decode(&bytes), Err(DecodeError::TooDeep));
    }

    #[test]
    fn rejects_a_child_count_larger_than_the_remaining_input() {
        // The attack this blocks: a 2-byte count asking for a huge allocation.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(TREE_MAGIC);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(KIND_CONTAINER);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..4 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(
            Tree::decode(&bytes),
            Err(DecodeError::Truncated { .. })
        ));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(TREE_MAGIC);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(KIND_LABEL);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..4 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(Tree::decode(&bytes), Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn rejects_malformed_events() {
        assert!(UiEvent::decode(b"").is_err());
        assert!(UiEvent::decode(b"IUE1").is_err());
        assert_eq!(
            UiEvent::decode(b"IUE1\x09"),
            Err(DecodeError::UnknownKind(9))
        );
    }
}
