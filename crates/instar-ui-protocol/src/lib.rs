//! Instar's UI wire format — and nothing else.
//!
//! This crate exists because of a boundary problem WP5 exposed: the guest and
//! the host must agree on an encoding byte for byte, but a guest has no
//! business linking the host's UI implementation to get it. `instar-ui` is
//! about to grow a layout engine; nothing here may follow it into a guest.
//!
//! So the split is:
//!
//! | | `instar-ui-protocol` (here) | `instar-ui` |
//! |---|---|---|
//! | Owns | bytes | meaning |
//! | Contents | version, opcodes, primitives, bounds, encoder, decoder | tree assembly, semantic validation, hit-testing, interaction rules |
//! | Linked by | guests **and** host | host only |
//! | Dependencies | none, ever | whatever it needs |
//!
//! # The encoding is written by hand, on purpose
//!
//! No Serde, no bincode, no `repr(C)`. Every field's position and width is
//! stated in code you can read, because this is a compatibility surface that
//! outlives any one implementation of either side, and because a derived
//! encoding makes it far too easy to change the wire format by accident.
//!
//! # Decoding is adversarial
//!
//! Batches arrive from untrusted guests. Every length is bounded before it is
//! trusted, every read is checked, counts are validated against the input that
//! actually remains before anything is allocated, and malformed input is
//! always a [`ProtocolError`] — never a panic, never an unbounded allocation.
//!
//! # Structure, not semantics
//!
//! This crate reports what the bytes say. It does not decide whether what they
//! say is *sensible*: duplicate keys, unreachable nodes, and nonsense
//! hierarchies are `instar-ui`'s to reject. Keeping that line sharp is what
//! stops semantic rules from quietly becoming wire rules.

#![forbid(unsafe_code)]

use core::fmt;

/// Wire format version. Bump only for an incompatible change, and only
/// together with the decoder's handling of older versions.
pub const PROTOCOL_VERSION: u8 = 1;

/// Leading bytes of a committed UI batch.
pub const BATCH_MAGIC: [u8; 4] = *b"IUI1";
/// Leading bytes of a host-to-guest event.
pub const EVENT_MAGIC: [u8; 4] = *b"IUE1";

/// Hard bounds applied to every decode.
///
/// A security boundary, not a tuning knob: exceeding any of these rejects the
/// batch. They are what stops a hostile guest from turning a commit into an
/// out-of-memory, so raise them only with a reason.
pub mod limits {
    /// Maximum nodes in one tree.
    pub const MAX_NODES: usize = 4096;
    /// Maximum nesting depth.
    pub const MAX_DEPTH: usize = 64;
    /// Maximum bytes in a single node's text.
    pub const MAX_TEXT_BYTES: usize = 4096;
    /// Maximum size of an entire encoded batch.
    pub const MAX_BATCH_BYTES: usize = 1 << 20;
    /// Largest accepted fixed dimension, padding, or gap, in logical pixels.
    /// Bounds layout arithmetic to values that cannot overflow downstream.
    pub const MAX_LENGTH: u16 = 1 << 14;
}

/// Node kind opcodes.
///
/// Deliberately four. Phase 1's layout vocabulary is meant to stay small
/// enough to reason about completely; a general CSS surface is not a goal, and
/// every kind added here is one the host must lay out, hit-test, and paint
/// forever.
pub mod opcode {
    /// The single outermost node. Fills the viewport.
    pub const NODE_ROOT: u8 = 0;
    /// Stacks its children vertically.
    pub const NODE_COLUMN: u8 = 1;
    /// Displays text. Measured, not sized by the guest.
    pub const NODE_TEXT: u8 = 2;
    /// Interactive, with a text label.
    pub const NODE_BUTTON: u8 = 3;

    pub const SECTION_END: u8 = 0;
    pub const SECTION_TREE: u8 = 1;

    pub const EVENT_CLICK: u8 = 0;

    /// Dimension tags. See [`super::WireDimension`].
    pub const DIM_FILL: u8 = 0;
    pub const DIM_CONTENT: u8 = 1;
    pub const DIM_FIXED: u8 = 2;
}

/// Node flag bits.
pub mod flags {
    /// Set when an interactive node is enabled.
    pub const ENABLED: u8 = 1 << 0;
}

/// A node's identity on the wire. Assigned by the guest and stable across
/// commits, so the host can address a node it saw in an earlier revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeKey(pub u32);

impl fmt::Display for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node{}", self.0)
    }
}

/// An axis-aligned rectangle in logical pixels.
///
/// Integer rather than floating point: these cross a trust boundary and are
/// compared for hit-testing, and integers make decoding and hit-test results
/// exactly reproducible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WireRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl WireRect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// How a node is sized along one axis.
///
/// The entire Phase 1 sizing vocabulary. Note what is absent: no percentages,
/// no min/max, no arbitrary positioning, no grid. A guest states intent; the
/// host decides the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireDimension {
    /// Take all the space the parent offers.
    Fill,
    /// Be as large as the content needs.
    Content,
    /// Exactly this many logical pixels.
    Fixed(u16),
}

impl WireDimension {
    pub fn tag(self) -> u8 {
        match self {
            Self::Fill => opcode::DIM_FILL,
            Self::Content => opcode::DIM_CONTENT,
            Self::Fixed(_) => opcode::DIM_FIXED,
        }
    }

    pub fn value(self) -> u16 {
        match self {
            Self::Fixed(px) => px,
            _ => 0,
        }
    }
}

/// A node's layout intent.
///
/// **Intent, not geometry.** A guest says "fill the width, pad by 8"; it never
/// says "you are at (10, 40) and 100x30". Geometry is computed by the host and
/// never travels on this wire in either direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireLayout {
    pub width: WireDimension,
    pub height: WireDimension,
    /// Inset applied on all four sides, in logical pixels.
    pub padding: u16,
    /// Space between children, in logical pixels. Ignored by leaf nodes.
    pub gap: u16,
}

impl Default for WireLayout {
    fn default() -> Self {
        Self {
            width: WireDimension::Content,
            height: WireDimension::Content,
            padding: 0,
            gap: 0,
        }
    }
}

/// One node as it appears on the wire, in pre-order.
///
/// Flat, with an explicit `child_count`, so a decoder can rebuild the
/// hierarchy with a bounded stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireNode {
    pub kind: u8,
    pub key: NodeKey,
    pub flags: u8,
    /// Present for text and button nodes.
    pub text: Option<String>,
    pub layout: WireLayout,
    pub child_count: u16,
}

impl WireNode {
    pub fn is_enabled(&self) -> bool {
        self.flags & flags::ENABLED != 0
    }
}

/// A decoded batch.
///
/// Just the tree. There is no geometry section and no way to express one: as
/// of WP7A the host computes all geometry from layout intent, and a guest
/// cannot state a rectangle even if it wants to. That is the point — a guest
/// authoritative over geometry would undermine the retained host presentation
/// model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireBatch {
    /// Pre-order nodes.
    pub nodes: Vec<WireNode>,
}

/// Why a batch or event was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    TooLarge(usize),
    BadMagic,
    UnsupportedVersion(u8),
    Truncated {
        while_reading: &'static str,
    },
    UnknownOpcode {
        context: &'static str,
        value: u8,
    },
    TooManyNodes,
    TooDeep,
    TextTooLong(usize),
    InvalidUtf8,
    /// A fixed dimension, padding, or gap exceeded [`limits::MAX_LENGTH`].
    LengthTooLarge(u16),
    /// The pre-order child counts do not describe one well-formed tree.
    MalformedTree(&'static str),
    TrailingBytes(usize),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(n) => write!(
                f,
                "batch is {n} bytes, over the {} byte limit",
                limits::MAX_BATCH_BYTES
            ),
            Self::BadMagic => write!(f, "not an Instar UI message (bad magic)"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "wire version {v} is not supported (this build speaks {PROTOCOL_VERSION})"
            ),
            Self::Truncated { while_reading } => {
                write!(f, "input ended unexpectedly while reading {while_reading}")
            }
            Self::UnknownOpcode { context, value } => {
                write!(f, "unknown {context} opcode {value}")
            }
            Self::TooManyNodes => write!(f, "tree has more than {} nodes", limits::MAX_NODES),
            Self::TooDeep => write!(f, "tree nests deeper than {}", limits::MAX_DEPTH),
            Self::TextTooLong(n) => write!(
                f,
                "text is {n} bytes, over the {} byte limit",
                limits::MAX_TEXT_BYTES
            ),
            Self::InvalidUtf8 => write!(f, "text is not valid UTF-8"),
            Self::LengthTooLarge(n) => write!(
                f,
                "layout length {n} exceeds the {} pixel limit",
                limits::MAX_LENGTH
            ),
            Self::MalformedTree(why) => write!(f, "malformed tree: {why}"),
            Self::TrailingBytes(n) => write!(f, "{n} trailing bytes after the message"),
        }
    }
}

impl core::error::Error for ProtocolError {}

/// Reads primitives out of a byte slice without ever panicking.
///
/// Public because both sides decode: the host decodes batches, the guest
/// decodes events, and neither should reimplement bounds checking.
pub struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub fn take(
        &mut self,
        n: usize,
        while_reading: &'static str,
    ) -> Result<&'a [u8], ProtocolError> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or(ProtocolError::Truncated { while_reading })?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(ProtocolError::Truncated { while_reading })?;
        self.offset = end;
        Ok(slice)
    }

    pub fn u8(&mut self, while_reading: &'static str) -> Result<u8, ProtocolError> {
        Ok(self.take(1, while_reading)?[0])
    }

    pub fn u16(&mut self, while_reading: &'static str) -> Result<u16, ProtocolError> {
        let b = self.take(2, while_reading)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self, while_reading: &'static str) -> Result<u32, ProtocolError> {
        let b = self.take(4, while_reading)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i32(&mut self, while_reading: &'static str) -> Result<i32, ProtocolError> {
        Ok(self.u32(while_reading)? as i32)
    }

    pub fn text(&mut self, while_reading: &'static str) -> Result<String, ProtocolError> {
        let len = self.u16(while_reading)? as usize;
        if len > limits::MAX_TEXT_BYTES {
            return Err(ProtocolError::TextTooLong(len));
        }
        let bytes = self.take(len, while_reading)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolError::InvalidUtf8)
    }

    /// Reads a bounded length. Layout arithmetic downstream assumes these fit
    /// comfortably in an f32 without surprises.
    pub fn length(&mut self, while_reading: &'static str) -> Result<u16, ProtocolError> {
        let value = self.u16(while_reading)?;
        if value > limits::MAX_LENGTH {
            return Err(ProtocolError::LengthTooLarge(value));
        }
        Ok(value)
    }

    pub fn dimension(
        &mut self,
        while_reading: &'static str,
    ) -> Result<WireDimension, ProtocolError> {
        let tag = self.u8(while_reading)?;
        let value = self.length(while_reading)?;
        match tag {
            opcode::DIM_FILL => Ok(WireDimension::Fill),
            opcode::DIM_CONTENT => Ok(WireDimension::Content),
            opcode::DIM_FIXED => Ok(WireDimension::Fixed(value)),
            value => Err(ProtocolError::UnknownOpcode {
                context: "dimension",
                value,
            }),
        }
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
}

/// Appends a length-prefixed string, truncating at a char boundary rather than
/// emitting a length the decoder would reject.
pub fn write_text(out: &mut Vec<u8>, text: &str) {
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

/// Builds a batch. The only supported way to produce one.
#[derive(Debug, Default)]
pub struct BatchEncoder {
    nodes: Vec<u8>,
    node_count: usize,
}

impl BatchEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one node in pre-order. The caller is responsible for emitting
    /// exactly `child_count` children immediately afterwards.
    pub fn node(
        &mut self,
        kind: u8,
        key: NodeKey,
        flags: u8,
        text: Option<&str>,
        layout: WireLayout,
        child_count: u16,
    ) -> &mut Self {
        self.nodes.push(kind);
        self.nodes.extend_from_slice(&key.0.to_le_bytes());
        self.nodes.push(flags);
        if let Some(text) = text {
            write_text(&mut self.nodes, text);
        }
        self.nodes.push(layout.width.tag());
        self.nodes
            .extend_from_slice(&layout.width.value().to_le_bytes());
        self.nodes.push(layout.height.tag());
        self.nodes
            .extend_from_slice(&layout.height.value().to_le_bytes());
        self.nodes.extend_from_slice(&layout.padding.to_le_bytes());
        self.nodes.extend_from_slice(&layout.gap.to_le_bytes());
        self.nodes.extend_from_slice(&child_count.to_le_bytes());
        self.node_count += 1;
        self
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.nodes.len());
        out.extend_from_slice(&BATCH_MAGIC);
        out.push(PROTOCOL_VERSION);

        out.push(opcode::SECTION_TREE);
        out.extend_from_slice(&(self.node_count.min(u16::MAX as usize) as u16).to_le_bytes());
        out.extend_from_slice(&self.nodes);

        out.push(opcode::SECTION_END);
        out
    }
}

/// Decodes a batch from an untrusted source.
///
/// Reports structure only: that the bytes describe *a* tree. Whether that tree
/// makes sense is `instar-ui`'s judgement.
pub fn decode_batch(bytes: &[u8]) -> Result<WireBatch, ProtocolError> {
    if bytes.len() > limits::MAX_BATCH_BYTES {
        return Err(ProtocolError::TooLarge(bytes.len()));
    }

    let mut reader = Reader::new(bytes);
    if reader.take(4, "magic")? != BATCH_MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    let version = reader.u8("version")?;
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }

    let mut batch = WireBatch::default();
    let mut seen_tree = false;

    loop {
        match reader.u8("section tag")? {
            opcode::SECTION_END => break,
            opcode::SECTION_TREE => {
                if seen_tree {
                    return Err(ProtocolError::MalformedTree("more than one tree section"));
                }
                seen_tree = true;
                batch.nodes = decode_tree_section(&mut reader)?;
            }
            value => {
                return Err(ProtocolError::UnknownOpcode {
                    context: "section",
                    value,
                });
            }
        }
    }

    if !seen_tree {
        return Err(ProtocolError::MalformedTree("no tree section"));
    }
    if reader.remaining() > 0 {
        return Err(ProtocolError::TrailingBytes(reader.remaining()));
    }
    Ok(batch)
}

fn decode_tree_section(reader: &mut Reader<'_>) -> Result<Vec<WireNode>, ProtocolError> {
    let declared = reader.u16("node count")? as usize;
    if declared > limits::MAX_NODES {
        return Err(ProtocolError::TooManyNodes);
    }

    let mut nodes = Vec::with_capacity(declared.min(64));
    // `expected` tracks how many more nodes the tree still owes us: one for the
    // root, then one per declared child. Walking it to exactly zero is what
    // proves the pre-order stream is a single well-formed tree rather than a
    // forest or a truncated branch.
    let mut expected: usize = 1;
    let mut depth_stack: Vec<u16> = Vec::new();

    while expected > 0 {
        if nodes.len() >= limits::MAX_NODES {
            return Err(ProtocolError::TooManyNodes);
        }

        let kind = reader.u8("node kind")?;
        let key = NodeKey(reader.u32("node key")?);
        let node_flags = reader.u8("node flags")?;
        let text = match kind {
            opcode::NODE_ROOT | opcode::NODE_COLUMN => None,
            opcode::NODE_TEXT => Some(reader.text("text content")?),
            opcode::NODE_BUTTON => Some(reader.text("button label")?),
            value => {
                return Err(ProtocolError::UnknownOpcode {
                    context: "node kind",
                    value,
                });
            }
        };
        let layout = WireLayout {
            width: reader.dimension("width")?,
            height: reader.dimension("height")?,
            padding: reader.length("padding")?,
            gap: reader.length("gap")?,
        };
        let child_count = reader.u16("child count")?;

        // Bound the count against what is actually left before trusting it: a
        // two-byte count could otherwise demand a 65535-element allocation per
        // node from a handful of bytes.
        if child_count as usize > reader.remaining() {
            return Err(ProtocolError::Truncated {
                while_reading: "children",
            });
        }

        nodes.push(WireNode {
            kind,
            key,
            flags: node_flags,
            text,
            layout,
            child_count,
        });

        expected -= 1;
        expected = expected
            .checked_add(child_count as usize)
            .ok_or(ProtocolError::TooManyNodes)?;

        // Depth is tracked by counting down each ancestor's outstanding
        // children, which is how a flat stream still has a meaningful depth.
        if child_count > 0 {
            if depth_stack.len() >= limits::MAX_DEPTH {
                return Err(ProtocolError::TooDeep);
            }
            depth_stack.push(child_count);
        } else {
            while let Some(remaining) = depth_stack.last_mut() {
                *remaining -= 1;
                if *remaining == 0 {
                    depth_stack.pop();
                } else {
                    break;
                }
            }
        }
    }

    Ok(nodes)
}

/// A host-to-guest interaction event on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireEvent {
    Click { node: NodeKey },
}

impl WireEvent {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10);
        out.extend_from_slice(&EVENT_MAGIC);
        out.push(PROTOCOL_VERSION);
        match self {
            WireEvent::Click { node } => {
                out.push(opcode::EVENT_CLICK);
                out.extend_from_slice(&node.0.to_le_bytes());
            }
        }
        out
    }

    /// Decoded guest-side, and bounded for the same reason batches are: a
    /// guest should not be crashable by a malformed host event either.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut reader = Reader::new(bytes);
        if reader.take(4, "magic")? != EVENT_MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let version = reader.u8("version")?;
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let event = match reader.u8("event kind")? {
            opcode::EVENT_CLICK => WireEvent::Click {
                node: NodeKey(reader.u32("node key")?),
            },
            value => {
                return Err(ProtocolError::UnknownOpcode {
                    context: "event",
                    value,
                });
            }
        };
        if reader.remaining() > 0 {
            return Err(ProtocolError::TrailingBytes(reader.remaining()));
        }
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill_width() -> WireLayout {
        WireLayout {
            width: WireDimension::Fill,
            height: WireDimension::Content,
            padding: 8,
            gap: 4,
        }
    }

    fn sample() -> Vec<u8> {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey(0), 0, None, fill_width(), 1)
            .node(opcode::NODE_COLUMN, NodeKey(1), 0, None, fill_width(), 2)
            .node(
                opcode::NODE_TEXT,
                NodeKey(2),
                0,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey(3),
                flags::ENABLED,
                Some("Press me"),
                WireLayout {
                    width: WireDimension::Fixed(100),
                    height: WireDimension::Fixed(30),
                    padding: 4,
                    gap: 0,
                },
                0,
            );
        encoder.finish()
    }

    #[test]
    fn round_trips() {
        let batch = decode_batch(&sample()).unwrap();
        assert_eq!(batch.nodes.len(), 4);
        assert_eq!(batch.nodes[0].kind, opcode::NODE_ROOT);
        assert_eq!(batch.nodes[0].layout.width, WireDimension::Fill);
        assert_eq!(batch.nodes[0].layout.padding, 8);
        assert_eq!(batch.nodes[0].layout.gap, 4);
        assert_eq!(batch.nodes[2].text.as_deref(), Some("Clicked 0 times"));
        assert!(batch.nodes[3].is_enabled());
        assert_eq!(batch.nodes[3].layout.width, WireDimension::Fixed(100));
        assert_eq!(batch.nodes[3].layout.height, WireDimension::Fixed(30));
    }

    /// There is no way to put geometry on the wire. WP7A removed the layout
    /// section outright rather than deprecating it, so a guest cannot be
    /// authoritative over geometry even by accident.
    #[test]
    fn a_batch_carries_only_a_tree() {
        let batch = decode_batch(&sample()).unwrap();
        // `WireBatch` has exactly one field; this fails to compile if a
        // geometry channel is ever reintroduced alongside it.
        let WireBatch { nodes } = batch;
        assert_eq!(nodes.len(), 4);
    }

    #[test]
    fn events_round_trip() {
        let event = WireEvent::Click { node: NodeKey(7) };
        assert_eq!(WireEvent::decode(&event.encode()).unwrap(), event);
    }

    // --- Everything below is about surviving a hostile guest. ---

    #[test]
    fn rejects_bad_magic() {
        assert_eq!(decode_batch(b"XXXX\x01\x00"), Err(ProtocolError::BadMagic));
    }

    #[test]
    fn rejects_unsupported_versions() {
        let mut bytes = sample();
        bytes[4] = 99;
        assert_eq!(
            decode_batch(&bytes),
            Err(ProtocolError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn rejects_truncation_at_every_length() {
        let full = sample();
        for len in 0..full.len() {
            assert!(
                decode_batch(&full[..len]).is_err(),
                "prefix of length {len} decoded successfully"
            );
        }
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = sample();
        bytes.push(0);
        assert!(matches!(
            decode_batch(&bytes),
            Err(ProtocolError::TrailingBytes(1))
        ));
    }

    #[test]
    fn rejects_unknown_node_kinds() {
        let mut bytes = sample();
        bytes[8] = 99; // first node's kind byte
        assert!(matches!(
            decode_batch(&bytes),
            Err(ProtocolError::UnknownOpcode {
                context: "node kind",
                value: 99
            })
        ));
    }

    #[test]
    fn rejects_unknown_dimension_tags() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROOT,
            NodeKey(0),
            0,
            None,
            WireLayout::default(),
            0,
        );
        let mut bytes = encoder.finish();
        // The width tag sits just past kind(1) + key(4) + flags(1) in the node,
        // which itself starts after magic(4) + version(1) + section(1) + count(2).
        bytes[8 + 6] = 99;
        assert!(matches!(
            decode_batch(&bytes),
            Err(ProtocolError::UnknownOpcode {
                context: "dimension",
                value: 99
            })
        ));
    }

    /// Layout lengths are bounded so downstream arithmetic cannot be handed
    /// absurd values by a hostile guest.
    #[test]
    fn rejects_oversized_layout_lengths() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BATCH_MAGIC);
        bytes.push(PROTOCOL_VERSION);
        bytes.push(opcode::SECTION_TREE);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(opcode::NODE_ROOT);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.push(opcode::DIM_FIXED);
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(
            decode_batch(&bytes),
            Err(ProtocolError::LengthTooLarge(_))
        ));
    }

    #[test]
    fn rejects_oversized_batches() {
        let huge = vec![0u8; limits::MAX_BATCH_BYTES + 1];
        assert!(matches!(
            decode_batch(&huge),
            Err(ProtocolError::TooLarge(_))
        ));
    }

    #[test]
    fn rejects_excessive_depth_without_overflowing_the_stack() {
        let mut encoder = BatchEncoder::new();
        for id in 0..(limits::MAX_DEPTH + 50) {
            encoder.node(
                opcode::NODE_COLUMN,
                NodeKey(id as u32),
                0,
                None,
                WireLayout::default(),
                1,
            );
        }
        assert_eq!(decode_batch(&encoder.finish()), Err(ProtocolError::TooDeep));
    }

    #[test]
    fn rejects_a_child_count_larger_than_the_remaining_input() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROOT,
            NodeKey(0),
            0,
            None,
            WireLayout::default(),
            u16::MAX,
        );
        assert!(matches!(
            decode_batch(&encoder.finish()),
            Err(ProtocolError::Truncated { .. })
        ));
    }

    #[test]
    fn rejects_a_forest_pretending_to_be_a_tree() {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                NodeKey(0),
                0,
                None,
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_ROOT,
                NodeKey(1),
                0,
                None,
                WireLayout::default(),
                0,
            );
        assert!(decode_batch(&encoder.finish()).is_err());
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BATCH_MAGIC);
        bytes.push(PROTOCOL_VERSION);
        bytes.push(opcode::SECTION_TREE);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(opcode::NODE_TEXT);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(decode_batch(&bytes), Err(ProtocolError::InvalidUtf8));
    }

    #[test]
    fn rejects_malformed_events() {
        assert!(WireEvent::decode(b"").is_err());
        assert!(WireEvent::decode(b"IUE1").is_err());
        assert!(matches!(
            WireEvent::decode(b"IUE1\x01\x09"),
            Err(ProtocolError::UnknownOpcode {
                context: "event",
                ..
            })
        ));
    }
}
