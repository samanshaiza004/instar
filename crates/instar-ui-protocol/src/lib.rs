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
    /// Maximum entries in a layout snapshot.
    pub const MAX_LAYOUT_ENTRIES: usize = MAX_NODES;
}

/// Node kind opcodes.
pub mod opcode {
    pub const NODE_CONTAINER: u8 = 0;
    pub const NODE_LABEL: u8 = 1;
    pub const NODE_BUTTON: u8 = 2;

    pub const SECTION_END: u8 = 0;
    pub const SECTION_TREE: u8 = 1;
    pub const SECTION_LAYOUT: u8 = 2;

    pub const EVENT_CLICK: u8 = 0;
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

/// One node as it appears on the wire, in pre-order.
///
/// Flat, with an explicit `child_count`, so a decoder can rebuild the
/// hierarchy with a bounded stack. Carries no geometry — see
/// [`WireBatch::layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireNode {
    pub kind: u8,
    pub key: NodeKey,
    pub flags: u8,
    /// Present for label and button nodes.
    pub text: Option<String>,
    pub child_count: u16,
}

impl WireNode {
    pub fn is_enabled(&self) -> bool {
        self.flags & flags::ENABLED != 0
    }
}

/// A decoded batch: the tree, plus an optional layout snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireBatch {
    /// Pre-order nodes.
    pub nodes: Vec<WireNode>,
    /// **Provisional, and scheduled for removal.**
    ///
    /// Geometry is deliberately *not* part of the tree: the host owns
    /// presentation, and a guest that dictates rects would quietly become
    /// authoritative over geometry. This section exists only so WP5 could
    /// prove an interaction round-trip before a layout engine existed.
    ///
    /// When the host computes layout (WP7), it produces the snapshot itself
    /// and this section is deleted — a change to the tree format is not
    /// required, which is the entire reason it is a separate section rather
    /// than fields on `WireNode`.
    pub layout: Vec<(NodeKey, WireRect)>,
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
    TooManyLayoutEntries,
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
            Self::TooManyLayoutEntries => write!(
                f,
                "layout snapshot has more than {} entries",
                limits::MAX_LAYOUT_ENTRIES
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
    layout: Vec<u8>,
    layout_count: usize,
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
        child_count: u16,
    ) -> &mut Self {
        self.nodes.push(kind);
        self.nodes.extend_from_slice(&key.0.to_le_bytes());
        self.nodes.push(flags);
        if let Some(text) = text {
            write_text(&mut self.nodes, text);
        }
        self.nodes.extend_from_slice(&child_count.to_le_bytes());
        self.node_count += 1;
        self
    }

    /// Appends a layout entry. See [`WireBatch::layout`] — provisional.
    pub fn layout_entry(&mut self, key: NodeKey, rect: WireRect) -> &mut Self {
        self.layout.extend_from_slice(&key.0.to_le_bytes());
        self.layout.extend_from_slice(&rect.x.to_le_bytes());
        self.layout.extend_from_slice(&rect.y.to_le_bytes());
        self.layout.extend_from_slice(&rect.width.to_le_bytes());
        self.layout.extend_from_slice(&rect.height.to_le_bytes());
        self.layout_count += 1;
        self
    }

    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.nodes.len() + self.layout.len());
        out.extend_from_slice(&BATCH_MAGIC);
        out.push(PROTOCOL_VERSION);

        out.push(opcode::SECTION_TREE);
        out.extend_from_slice(&(self.node_count.min(u16::MAX as usize) as u16).to_le_bytes());
        out.extend_from_slice(&self.nodes);

        if self.layout_count > 0 {
            out.push(opcode::SECTION_LAYOUT);
            out.extend_from_slice(&(self.layout_count.min(u16::MAX as usize) as u16).to_le_bytes());
            out.extend_from_slice(&self.layout);
        }

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
            opcode::SECTION_LAYOUT => {
                batch.layout = decode_layout_section(&mut reader)?;
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
            opcode::NODE_CONTAINER => None,
            opcode::NODE_LABEL => Some(reader.text("label text")?),
            opcode::NODE_BUTTON => Some(reader.text("button label")?),
            value => {
                return Err(ProtocolError::UnknownOpcode {
                    context: "node kind",
                    value,
                });
            }
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

fn decode_layout_section(
    reader: &mut Reader<'_>,
) -> Result<Vec<(NodeKey, WireRect)>, ProtocolError> {
    let count = reader.u16("layout entry count")? as usize;
    if count > limits::MAX_LAYOUT_ENTRIES {
        return Err(ProtocolError::TooManyLayoutEntries);
    }
    // Each entry is 20 bytes; refuse a count the input cannot possibly satisfy
    // before allocating for it.
    if count.saturating_mul(20) > reader.remaining() {
        return Err(ProtocolError::Truncated {
            while_reading: "layout entries",
        });
    }

    let mut entries = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let key = NodeKey(reader.u32("layout key")?);
        entries.push((
            key,
            WireRect {
                x: reader.i32("layout x")?,
                y: reader.i32("layout y")?,
                width: reader.i32("layout width")?,
                height: reader.i32("layout height")?,
            },
        ));
    }
    Ok(entries)
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

    fn sample() -> Vec<u8> {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_CONTAINER, NodeKey(0), 0, None, 2)
            .node(
                opcode::NODE_LABEL,
                NodeKey(1),
                0,
                Some("Clicked 0 times"),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey(2),
                flags::ENABLED,
                Some("Press me"),
                0,
            )
            .layout_entry(NodeKey(0), WireRect::new(0, 0, 200, 100))
            .layout_entry(NodeKey(2), WireRect::new(10, 40, 100, 30));
        encoder.finish()
    }

    #[test]
    fn round_trips() {
        let batch = decode_batch(&sample()).unwrap();
        assert_eq!(batch.nodes.len(), 3);
        assert_eq!(batch.nodes[0].child_count, 2);
        assert_eq!(batch.nodes[1].text.as_deref(), Some("Clicked 0 times"));
        assert!(batch.nodes[2].is_enabled());
        assert_eq!(batch.layout.len(), 2);
        assert_eq!(batch.layout[1].1, WireRect::new(10, 40, 100, 30));
    }

    #[test]
    fn a_batch_without_layout_is_valid() {
        let mut encoder = BatchEncoder::new();
        encoder.node(opcode::NODE_CONTAINER, NodeKey(0), 0, None, 0);
        let batch = decode_batch(&encoder.finish()).unwrap();
        assert_eq!(batch.nodes.len(), 1);
        assert!(batch.layout.is_empty());
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
            encoder.node(opcode::NODE_CONTAINER, NodeKey(id as u32), 0, None, 1);
        }
        assert_eq!(decode_batch(&encoder.finish()), Err(ProtocolError::TooDeep));
    }

    #[test]
    fn rejects_a_child_count_larger_than_the_remaining_input() {
        let mut encoder = BatchEncoder::new();
        encoder.node(opcode::NODE_CONTAINER, NodeKey(0), 0, None, u16::MAX);
        assert!(matches!(
            decode_batch(&encoder.finish()),
            Err(ProtocolError::Truncated { .. })
        ));
    }

    #[test]
    fn rejects_a_forest_pretending_to_be_a_tree() {
        // Two roots: the pre-order stream would be well-formed for a forest,
        // but a batch describes exactly one tree.
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_CONTAINER, NodeKey(0), 0, None, 0)
            .node(opcode::NODE_CONTAINER, NodeKey(1), 0, None, 0);
        // The second root is never consumed by the tree walk, so it shows up
        // as an unknown section tag or trailing input rather than being
        // silently accepted.
        assert!(decode_batch(&encoder.finish()).is_err());
    }

    #[test]
    fn rejects_invalid_utf8() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BATCH_MAGIC);
        bytes.push(PROTOCOL_VERSION);
        bytes.push(opcode::SECTION_TREE);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(opcode::NODE_LABEL);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.push(opcode::SECTION_END);
        assert_eq!(decode_batch(&bytes), Err(ProtocolError::InvalidUtf8));
    }

    /// A count over the hard limit is refused by the limit, before the
    /// remaining-input check ever runs.
    #[test]
    fn rejects_a_layout_count_over_the_limit() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BATCH_MAGIC);
        bytes.push(PROTOCOL_VERSION);
        bytes.push(opcode::SECTION_TREE);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(opcode::NODE_CONTAINER);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.push(opcode::SECTION_LAYOUT);
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(
            decode_batch(&bytes),
            Err(ProtocolError::TooManyLayoutEntries)
        );
    }

    /// A count *under* the limit but larger than the input can supply is
    /// refused before allocating for it. This is the path the limit check
    /// above would otherwise hide.
    #[test]
    fn rejects_a_layout_count_larger_than_the_remaining_input() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BATCH_MAGIC);
        bytes.push(PROTOCOL_VERSION);
        bytes.push(opcode::SECTION_TREE);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.push(opcode::NODE_CONTAINER);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.push(opcode::SECTION_LAYOUT);
        // 100 entries promised, zero bytes of entries supplied.
        bytes.extend_from_slice(&100u16.to_le_bytes());
        assert!(matches!(
            decode_batch(&bytes),
            Err(ProtocolError::Truncated { .. })
        ));
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
