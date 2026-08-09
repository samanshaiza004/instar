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

/// Wire format version. Bump only for an incompatible change. The magic
/// identifies the format; the version byte identifies the revision, so
/// [`BATCH_MAGIC`] and [`EVENT_MAGIC`] stay put when this changes.
pub const PROTOCOL_VERSION: u8 = 4;

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
    /// Largest accepted flex grow or shrink factor.
    ///
    /// The ceiling is deliberately boring: far past anything sensible, but a
    /// hard one, because a hostile guest must not be able to hand layout an
    /// unbounded ratio. The exact value matters less than that a bound exists.
    pub const MAX_FLEX_FACTOR: f32 = 1024.0;
}

/// Node kind opcodes.
///
/// Deliberately six. The layout vocabulary is meant to stay small enough to
/// reason about completely; a general CSS surface is not a goal, and every
/// kind added here is one the host must lay out, hit-test, and paint forever.
pub mod opcode {
    /// The single outermost node. Fills the viewport.
    pub const NODE_ROOT: u8 = 0;
    /// Stacks its children vertically.
    pub const NODE_COLUMN: u8 = 1;
    /// Displays text. Measured, not sized by the guest.
    pub const NODE_TEXT: u8 = 2;
    /// Interactive, with a text label.
    pub const NODE_BUTTON: u8 = 3;
    /// Stacks its children horizontally.
    pub const NODE_ROW: u8 = 4;
    /// Overlaps its children at the content-box origin; later children paint
    /// over earlier ones.
    pub const NODE_STACK: u8 = 5;

    pub const SECTION_END: u8 = 0;
    pub const SECTION_TREE: u8 = 1;

    pub const EVENT_CLICK: u8 = 0;

    /// Dimension tags. See [`super::WireSize`].
    pub const DIM_CONTENT: u8 = 1;
    pub const DIM_FIXED: u8 = 2;

    /// Alignment tags. See [`super::WireAlign`].
    pub const ALIGN_START: u8 = 0;
    pub const ALIGN_CENTER: u8 = 1;
    pub const ALIGN_END: u8 = 2;
    pub const ALIGN_STRETCH: u8 = 3;

    /// Main-axis distribution tags. See [`super::WireJustify`].
    pub const JUSTIFY_START: u8 = 0;
    pub const JUSTIFY_CENTER: u8 = 1;
    pub const JUSTIFY_END: u8 = 2;
    pub const JUSTIFY_SPACE_BETWEEN: u8 = 3;
    pub const JUSTIFY_SPACE_AROUND: u8 = 4;
    pub const JUSTIFY_SPACE_EVENLY: u8 = 5;
}

/// Node flag bits.
pub mod flags {
    /// Set when an interactive node is enabled.
    pub const ENABLED: u8 = 1 << 0;
}

/// A node's identity on the wire. Assigned by the guest and stable across
/// commits, so the host can address a node it saw in an earlier revision.
///
/// The generation is the guest's answer to "is this still the same logical
/// node?": an id that is removed and reused comes back at a higher
/// generation, and an event carrying an old `(id, generation)` names a node
/// that no longer exists.
///
/// Derive order matters. `Ord` sorts by `id` and then `generation`, which is
/// the ordering snapshots and diffs want. AccessKit's packed id is the
/// opposite ordering — generation in the high half — so do not "fix" one to
/// match the other; [`NodeKey::to_accesskit_id`] is where that packing lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeKey {
    pub id: u32,
    pub generation: u32,
}

impl NodeKey {
    pub const fn new(id: u32, generation: u32) -> Self {
        Self { id, generation }
    }

    /// Generation 0: the first lifetime of an id.
    pub const fn first(id: u32) -> Self {
        Self { id, generation: 0 }
    }

    /// The packing AccessKit uses for stable ids: generation in the high
    /// half, id in the low, both halves losslessly represented.
    pub const fn to_accesskit_id(self) -> u64 {
        ((self.generation as u64) << 32) | self.id as u64
    }

    pub const fn from_accesskit_id(packed: u64) -> Self {
        Self {
            id: packed as u32,
            generation: (packed >> 32) as u32,
        }
    }
}

impl fmt::Display for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node{}#{}", self.id, self.generation)
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

/// A node's *preferred* size along one axis.
///
/// Preferred, and nothing more. What happens when there is spare space or not
/// enough is [`WireLayout::grow`] and [`WireLayout::shrink`]; what happens on
/// the cross axis is [`WireLayout::align_self`]. Those are separate questions
/// and the wire keeps them separate.
///
/// There used to be a third variant, `Fill`. It meant cross-axis stretch under
/// a column, height under a row, and content-size on a row's main axis — one
/// name for three behaviours, which is a rule nobody can hold in their head.
/// See `docs/PHASE-2.md`, "`Fill` leaves the wire in A2".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireSize {
    /// Be as large as the content needs.
    Content,
    /// Exactly this many logical pixels.
    Fixed(u16),
}

impl WireSize {
    pub fn tag(self) -> u8 {
        match self {
            Self::Content => opcode::DIM_CONTENT,
            Self::Fixed(_) => opcode::DIM_FIXED,
        }
    }

    pub fn value(self) -> u16 {
        match self {
            Self::Fixed(px) => px,
            Self::Content => 0,
        }
    }
}

/// Cross-axis placement.
///
/// `Stretch` is the only way to span a parent's cross axis, which is what
/// makes it readable: a node that stretches says so, rather than implying it
/// through a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WireAlign {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

impl WireAlign {
    pub fn tag(self) -> u8 {
        match self {
            Self::Start => opcode::ALIGN_START,
            Self::Center => opcode::ALIGN_CENTER,
            Self::End => opcode::ALIGN_END,
            Self::Stretch => opcode::ALIGN_STRETCH,
        }
    }
}

/// Main-axis distribution of a container's children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WireJustify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

impl WireJustify {
    pub fn tag(self) -> u8 {
        match self {
            Self::Start => opcode::JUSTIFY_START,
            Self::Center => opcode::JUSTIFY_CENTER,
            Self::End => opcode::JUSTIFY_END,
            Self::SpaceBetween => opcode::JUSTIFY_SPACE_BETWEEN,
            Self::SpaceAround => opcode::JUSTIFY_SPACE_AROUND,
            Self::SpaceEvenly => opcode::JUSTIFY_SPACE_EVENLY,
        }
    }
}

/// A node's layout intent.
///
/// **Intent, not geometry.** A guest says "grow into the spare space, pad by
/// 8"; it never says "you are at (10, 40) and 100x30". Geometry is computed by
/// the host and never travels on this wire in either direction.
///
/// # Four orthogonal questions
///
/// ```text
/// preferred size         width / height
/// main-axis expansion    grow
/// main-axis contraction  shrink
/// cross-axis filling     align_self: Stretch
/// ```
///
/// Taffy separates these too — `flex_grow` is main-axis expansion,
/// `align_items`/`align_self` are cross-axis — so this is the wire describing
/// intent rather than a conflation Instar invented.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WireLayout {
    pub width: WireSize,
    pub height: WireSize,
    /// Bounds on the computed size. `None` is "unconstrained", which is not
    /// the same as zero — hence an explicit option rather than a sentinel.
    pub min_width: Option<u16>,
    pub max_width: Option<u16>,
    pub min_height: Option<u16>,
    pub max_height: Option<u16>,
    /// Share of surplus main-axis space. `0.0` never grows.
    ///
    /// A ratio rather than a length, which is why it is the one float on this
    /// wire: `0.5` and `2.0` are both meaningful, and rounding them to
    /// integers would distort the vocabulary to dodge a hazard that
    /// [`Reader::flex_factor`] closes properly.
    pub grow: f32,
    /// Share of the deficit when children overflow. Defaults to `1.0`, as CSS
    /// does — a node that does not say otherwise gives way rather than
    /// overflowing its parent.
    pub shrink: f32,
    /// This node's own cross-axis placement. `None` inherits the parent's
    /// [`WireLayout::align_items`], which is the only reason it is optional.
    pub align_self: Option<WireAlign>,
    /// How this node places its children on the cross axis.
    pub align_items: WireAlign,
    /// How this node distributes its children along the main axis.
    pub justify_content: WireJustify,
    /// Inset applied on all four sides, in logical pixels.
    pub padding: u16,
    /// Space between children, in logical pixels. Ignored by leaf nodes.
    pub gap: u16,
}

impl Default for WireLayout {
    fn default() -> Self {
        Self {
            width: WireSize::Content,
            height: WireSize::Content,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            grow: 0.0,
            shrink: 1.0,
            align_self: None,
            align_items: WireAlign::Start,
            justify_content: WireJustify::Start,
            padding: 0,
            gap: 0,
        }
    }
}

/// One node as it appears on the wire, in pre-order.
///
/// Flat, with an explicit `child_count`, so a decoder can rebuild the
/// hierarchy with a bounded stack.
///
/// Not `Eq`: [`WireLayout`] carries flex factors, and `f32` has no total
/// equality. Decoding guarantees they are finite, which makes `PartialEq`
/// behave, but the trait bound cannot say so.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq, Default)]
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
    /// A flex factor was not a finite value in `0.0..=MAX_FLEX_FACTOR`.
    ///
    /// Carries the raw bits rather than the `f32` for two reasons: it keeps
    /// this type `Eq`, and a rejected NaN does not compare equal to itself, so
    /// an error holding one could never be asserted against.
    InvalidFlexFactor {
        bits: u32,
    },
    /// A minimum exceeded its corresponding maximum.
    InvalidBounds {
        min: u16,
        max: u16,
    },
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
            Self::InvalidFlexFactor { bits } => write!(
                f,
                "flex factor {} is not a finite value in 0.0..={}",
                f32::from_bits(*bits),
                limits::MAX_FLEX_FACTOR
            ),
            Self::InvalidBounds { min, max } => {
                write!(f, "minimum {min} exceeds maximum {max}")
            }
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

    /// A node key on the wire: id then generation, each little-endian, 8
    /// bytes total. Both decode sites read it through this one path.
    pub fn node_key(&mut self, while_reading: &'static str) -> Result<NodeKey, ProtocolError> {
        let id = self.u32(while_reading)?;
        let generation = self.u32(while_reading)?;
        Ok(NodeKey { id, generation })
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

    pub fn size(&mut self, while_reading: &'static str) -> Result<WireSize, ProtocolError> {
        let tag = self.u8(while_reading)?;
        let value = self.length(while_reading)?;
        match tag {
            opcode::DIM_CONTENT => Ok(WireSize::Content),
            opcode::DIM_FIXED => Ok(WireSize::Fixed(value)),
            value => Err(ProtocolError::UnknownOpcode {
                context: "dimension",
                value,
            }),
        }
    }

    /// An optional bound: a presence byte, then the value.
    ///
    /// Explicit rather than a sentinel. `u16::MAX` would have to mean
    /// "absent", which is a value `MAX_LENGTH` already rejects — so it would
    /// work, right up until someone raises `MAX_LENGTH` and silently turns
    /// "unconstrained" into a very large maximum.
    pub fn optional_length(
        &mut self,
        while_reading: &'static str,
    ) -> Result<Option<u16>, ProtocolError> {
        match self.u8(while_reading)? {
            0 => Ok(None),
            _ => Ok(Some(self.length(while_reading)?)),
        }
    }

    /// The only place an `f32` is admitted from untrusted bytes.
    ///
    /// Read as `u32` and reinterpreted, so the decoder never constructs a
    /// float it has not yet checked, and the rejected value can be reported
    /// as bits. Everything downstream — `instar-ui`, Taffy — may assume a flex
    /// factor is finite and within range because this is the only door.
    ///
    /// `-0.0` is canonicalized rather than rejected: it is a legitimate way to
    /// spell zero, it compares equal to `0.0`, and letting it through unchanged
    /// would put a value into layout that prints differently from every other
    /// zero for no reason a guest author could act on.
    pub fn flex_factor(&mut self, while_reading: &'static str) -> Result<f32, ProtocolError> {
        let bits = self.u32(while_reading)?;
        let value = f32::from_bits(bits);
        // One range check covers everything, including both things that are
        // not numbers: `contains` is false for NaN, because every comparison
        // against NaN is false, and the infinities fall outside the bounds.
        // An explicit `is_finite()` in front of this would read as though it
        // were carrying weight it is not.
        if !(0.0..=limits::MAX_FLEX_FACTOR).contains(&value) {
            return Err(ProtocolError::InvalidFlexFactor { bits });
        }
        // `-0.0` is inside the range and compares equal to `0.0`, so it
        // arrives here intact and is normalized rather than rejected.
        Ok(if value == 0.0 { 0.0 } else { value })
    }

    pub fn align(&mut self, while_reading: &'static str) -> Result<WireAlign, ProtocolError> {
        match self.u8(while_reading)? {
            opcode::ALIGN_START => Ok(WireAlign::Start),
            opcode::ALIGN_CENTER => Ok(WireAlign::Center),
            opcode::ALIGN_END => Ok(WireAlign::End),
            opcode::ALIGN_STRETCH => Ok(WireAlign::Stretch),
            value => Err(ProtocolError::UnknownOpcode {
                context: "align",
                value,
            }),
        }
    }

    /// `align_self`, where absence means "inherit the parent's `align_items`".
    pub fn optional_align(
        &mut self,
        while_reading: &'static str,
    ) -> Result<Option<WireAlign>, ProtocolError> {
        match self.u8(while_reading)? {
            0 => Ok(None),
            _ => Ok(Some(self.align(while_reading)?)),
        }
    }

    pub fn justify(&mut self, while_reading: &'static str) -> Result<WireJustify, ProtocolError> {
        match self.u8(while_reading)? {
            opcode::JUSTIFY_START => Ok(WireJustify::Start),
            opcode::JUSTIFY_CENTER => Ok(WireJustify::Center),
            opcode::JUSTIFY_END => Ok(WireJustify::End),
            opcode::JUSTIFY_SPACE_BETWEEN => Ok(WireJustify::SpaceBetween),
            opcode::JUSTIFY_SPACE_AROUND => Ok(WireJustify::SpaceAround),
            opcode::JUSTIFY_SPACE_EVENLY => Ok(WireJustify::SpaceEvenly),
            value => Err(ProtocolError::UnknownOpcode {
                context: "justify",
                value,
            }),
        }
    }

    /// A whole layout block, including the relationships between its fields.
    ///
    /// The bounds check lives here rather than in `instar-ui` because it is a
    /// statement about these bytes, not about what they mean: a minimum above
    /// its maximum is not a layout the host should have to have an opinion
    /// about.
    pub fn layout(&mut self, _while_reading: &'static str) -> Result<WireLayout, ProtocolError> {
        let layout = WireLayout {
            width: self.size("width")?,
            height: self.size("height")?,
            min_width: self.optional_length("min width")?,
            max_width: self.optional_length("max width")?,
            min_height: self.optional_length("min height")?,
            max_height: self.optional_length("max height")?,
            grow: self.flex_factor("grow")?,
            shrink: self.flex_factor("shrink")?,
            align_self: self.optional_align("align self")?,
            align_items: self.align("align items")?,
            justify_content: self.justify("justify content")?,
            padding: self.length("padding")?,
            gap: self.length("gap")?,
        };
        for (min, max) in [
            (layout.min_width, layout.max_width),
            (layout.min_height, layout.max_height),
        ] {
            if let (Some(min), Some(max)) = (min, max)
                && min > max
            {
                return Err(ProtocolError::InvalidBounds { min, max });
            }
        }
        Ok(layout)
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
fn write_size(out: &mut Vec<u8>, size: WireSize) {
    out.push(size.tag());
    out.extend_from_slice(&size.value().to_le_bytes());
}

/// A presence byte, then the value if there is one. Mirrors
/// [`Reader::optional_length`].
fn write_optional_length(out: &mut Vec<u8>, value: Option<u16>) {
    match value {
        Some(value) => {
            out.push(1);
            out.extend_from_slice(&value.to_le_bytes());
        }
        None => out.push(0),
    }
}

/// Written as bits, matching [`Reader::flex_factor`].
///
/// The encoder does not clamp. A caller that builds a nonsense factor should
/// see the decoder reject it, including in tests — an encoder that quietly
/// repaired its input would make the decoder's guard untestable through the
/// only API that produces batches.
fn write_flex_factor(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_bits().to_le_bytes());
}

fn write_layout(out: &mut Vec<u8>, layout: WireLayout) {
    write_size(out, layout.width);
    write_size(out, layout.height);
    write_optional_length(out, layout.min_width);
    write_optional_length(out, layout.max_width);
    write_optional_length(out, layout.min_height);
    write_optional_length(out, layout.max_height);
    write_flex_factor(out, layout.grow);
    write_flex_factor(out, layout.shrink);
    match layout.align_self {
        Some(align) => {
            out.push(1);
            out.push(align.tag());
        }
        None => out.push(0),
    }
    out.push(layout.align_items.tag());
    out.push(layout.justify_content.tag());
    out.extend_from_slice(&layout.padding.to_le_bytes());
    out.extend_from_slice(&layout.gap.to_le_bytes());
}

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
        self.nodes.extend_from_slice(&key.id.to_le_bytes());
        self.nodes.extend_from_slice(&key.generation.to_le_bytes());
        self.nodes.push(flags);
        if let Some(text) = text {
            write_text(&mut self.nodes, text);
        }
        write_layout(&mut self.nodes, layout);
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
        let key = reader.node_key("node key")?;
        let node_flags = reader.u8("node flags")?;
        let text = match kind {
            opcode::NODE_ROOT | opcode::NODE_COLUMN | opcode::NODE_ROW | opcode::NODE_STACK => None,
            opcode::NODE_TEXT => Some(reader.text("text content")?),
            opcode::NODE_BUTTON => Some(reader.text("button label")?),
            value => {
                return Err(ProtocolError::UnknownOpcode {
                    context: "node kind",
                    value,
                });
            }
        };
        let layout = reader.layout("layout")?;
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
        let mut out = Vec::with_capacity(14);
        out.extend_from_slice(&EVENT_MAGIC);
        out.push(PROTOCOL_VERSION);
        match self {
            WireEvent::Click { node } => {
                out.push(opcode::EVENT_CLICK);
                out.extend_from_slice(&node.id.to_le_bytes());
                out.extend_from_slice(&node.generation.to_le_bytes());
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
                node: reader.node_key("node key")?,
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

    /// A container that spans its parent's cross axis -- what `Fill` width
    /// used to say, now said as the alignment it always meant.
    fn stretched() -> WireLayout {
        WireLayout {
            align_self: Some(WireAlign::Stretch),
            padding: 8,
            gap: 4,
            ..WireLayout::default()
        }
    }

    fn sample() -> Vec<u8> {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                NodeKey::first(0),
                0,
                None,
                stretched(),
                1,
            )
            .node(
                opcode::NODE_COLUMN,
                NodeKey::first(1),
                0,
                None,
                stretched(),
                2,
            )
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(2),
                0,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey::first(3),
                flags::ENABLED,
                Some("Press me"),
                WireLayout {
                    width: WireSize::Fixed(100),
                    height: WireSize::Fixed(30),
                    padding: 4,
                    ..WireLayout::default()
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
        assert_eq!(batch.nodes[0].layout.align_self, Some(WireAlign::Stretch));
        assert_eq!(batch.nodes[0].layout.padding, 8);
        assert_eq!(batch.nodes[0].layout.gap, 4);
        assert_eq!(batch.nodes[2].text.as_deref(), Some("Clicked 0 times"));
        assert!(batch.nodes[3].is_enabled());
        assert_eq!(batch.nodes[3].layout.width, WireSize::Fixed(100));
        assert_eq!(batch.nodes[3].layout.height, WireSize::Fixed(30));
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
        let event = WireEvent::Click {
            node: NodeKey::new(7, 1),
        };
        assert_eq!(WireEvent::decode(&event.encode()).unwrap(), event);
    }

    /// Row and Stack are containers like Root and Column: they carry no text,
    /// and the decoder must not try to read any.
    #[test]
    fn row_and_stack_round_trip() {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                NodeKey::first(0),
                0,
                None,
                stretched(),
                2,
            )
            .node(opcode::NODE_ROW, NodeKey::first(1), 0, None, stretched(), 1)
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(2),
                0,
                Some("row child"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_STACK,
                NodeKey::first(3),
                0,
                None,
                stretched(),
                1,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey::first(4),
                flags::ENABLED,
                Some("stack child"),
                WireLayout::default(),
                0,
            );

        let batch = decode_batch(&encoder.finish()).unwrap();
        assert_eq!(batch.nodes[1].kind, opcode::NODE_ROW);
        assert_eq!(batch.nodes[1].text, None);
        assert_eq!(batch.nodes[1].layout, stretched());
        assert_eq!(batch.nodes[3].kind, opcode::NODE_STACK);
        assert_eq!(batch.nodes[3].text, None);
        assert_eq!(batch.nodes[3].layout, stretched());
    }

    #[test]
    fn accesskit_ids_round_trip() {
        let keys = [
            NodeKey::first(0),
            NodeKey::first(7),
            NodeKey::new(7, 1),
            NodeKey::new(u32::MAX, u32::MAX),
        ];
        for key in keys {
            assert_eq!(NodeKey::from_accesskit_id(key.to_accesskit_id()), key);
        }
        assert_eq!(
            NodeKey::new(7, 1).to_accesskit_id(),
            (1u64 << 32) | 7,
            "the generation occupies the high half and the id the low half"
        );
    }

    #[test]
    fn a_batch_round_trips_a_non_zero_generation() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROOT,
            NodeKey::new(7, 3),
            0,
            None,
            WireLayout::default(),
            0,
        );

        let batch = decode_batch(&encoder.finish()).unwrap();
        assert_eq!(batch.nodes[0].key, NodeKey::new(7, 3));
    }

    // --- Everything below is about surviving a hostile guest. ---

    #[test]
    fn rejects_bad_magic() {
        assert_eq!(decode_batch(b"XXXX\x02\x00"), Err(ProtocolError::BadMagic));
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
            NodeKey::first(0),
            0,
            None,
            WireLayout::default(),
            0,
        );
        let mut bytes = encoder.finish();
        // The width tag sits just past kind(1) + key(8) + flags(1) in the
        // node, which itself starts after magic(4) + version(1) + section(1)
        // + count(2).
        bytes[8 + 10] = 99;
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
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.push(opcode::DIM_FIXED);
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(
            decode_batch(&bytes),
            Err(ProtocolError::LengthTooLarge(_))
        ));
    }

    /// One node whose layout is `base` with `grow` overwritten by raw bits.
    ///
    /// Built through the encoder, which deliberately does not clamp, so these
    /// exercise the decoder's guard through the only API that produces
    /// batches rather than through a hand-assembled byte string that might
    /// drift from the real layout.
    fn batch_with_grow_bits(bits: u32) -> Vec<u8> {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROOT,
            NodeKey::first(0),
            0,
            None,
            WireLayout {
                grow: f32::from_bits(bits),
                ..WireLayout::default()
            },
            0,
        );
        encoder.finish()
    }

    /// The whole reason a float is allowed on this wire at all is that this
    /// one function refuses everything IEEE-754 offers that layout cannot use.
    #[test]
    fn rejects_every_flex_factor_layout_cannot_use() {
        for (name, value) in [
            ("NaN", f32::NAN),
            ("infinity", f32::INFINITY),
            ("negative infinity", f32::NEG_INFINITY),
            ("negative", -1.0),
            ("over the ceiling", limits::MAX_FLEX_FACTOR + 1.0),
        ] {
            assert!(
                matches!(
                    decode_batch(&batch_with_grow_bits(value.to_bits())),
                    Err(ProtocolError::InvalidFlexFactor { .. })
                ),
                "a {name} flex factor must be refused"
            );
        }
    }

    /// A signalling NaN, which is not `f32::NAN`'s bit pattern and is the
    /// shape a hostile guest would have to hand-assemble.
    #[test]
    fn rejects_a_signalling_nan_flex_factor() {
        assert!(matches!(
            decode_batch(&batch_with_grow_bits(0x7f80_0001)),
            Err(ProtocolError::InvalidFlexFactor { .. })
        ));
    }

    #[test]
    fn accepts_a_fractional_flex_factor_and_the_ceiling_itself() {
        for value in [0.0f32, 0.25, 0.5, 1.0, limits::MAX_FLEX_FACTOR] {
            let batch = decode_batch(&batch_with_grow_bits(value.to_bits()))
                .unwrap_or_else(|error| panic!("{value} should decode, got {error}"));
            assert_eq!(batch.nodes[0].layout.grow, value);
        }
    }

    /// `-0.0` is a legitimate way to spell zero, so it is canonicalized rather
    /// than refused. Asserting on the bits, because `-0.0 == 0.0` is true and
    /// would pass whether or not anything was canonicalized.
    #[test]
    fn canonicalizes_negative_zero() {
        let batch = decode_batch(&batch_with_grow_bits((-0.0f32).to_bits()))
            .expect("negative zero is a valid zero");
        assert_eq!(
            batch.nodes[0].layout.grow.to_bits(),
            0.0f32.to_bits(),
            "the sign bit must not survive into layout"
        );
    }

    #[test]
    fn rejects_a_minimum_above_its_maximum() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROOT,
            NodeKey::first(0),
            0,
            None,
            WireLayout {
                min_width: Some(200),
                max_width: Some(100),
                ..WireLayout::default()
            },
            0,
        );
        assert_eq!(
            decode_batch(&encoder.finish()),
            Err(ProtocolError::InvalidBounds { min: 200, max: 100 })
        );
    }

    /// Absence is a presence byte, not a sentinel, so a bound may legitimately
    /// be any value the length rule accepts — including zero, which a
    /// sentinel scheme tends to confuse with "unset".
    #[test]
    fn an_absent_bound_is_not_a_zero_bound() {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                NodeKey::first(0),
                0,
                None,
                WireLayout::default(),
                1,
            )
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(1),
                0,
                Some("x"),
                WireLayout {
                    min_width: Some(0),
                    max_width: Some(0),
                    ..WireLayout::default()
                },
                0,
            );
        let batch = decode_batch(&encoder.finish()).unwrap();
        assert_eq!(batch.nodes[0].layout.min_width, None);
        assert_eq!(batch.nodes[1].layout.min_width, Some(0));
        assert_eq!(batch.nodes[1].layout.max_width, Some(0));
    }

    #[test]
    fn the_whole_sizing_vocabulary_round_trips() {
        let layout = WireLayout {
            width: WireSize::Fixed(120),
            height: WireSize::Content,
            min_width: Some(40),
            max_width: Some(400),
            min_height: Some(10),
            max_height: Some(90),
            grow: 2.5,
            shrink: 0.25,
            align_self: Some(WireAlign::Center),
            align_items: WireAlign::Stretch,
            justify_content: WireJustify::SpaceBetween,
            padding: 7,
            gap: 3,
        };
        let mut encoder = BatchEncoder::new();
        encoder.node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, layout, 0);
        assert_eq!(
            decode_batch(&encoder.finish()).unwrap().nodes[0].layout,
            layout
        );
    }

    /// Byte offsets of a default layout's tag fields inside a one-node batch.
    ///
    /// Spelled out rather than computed from the end, because an arithmetic
    /// offset is exactly the kind of thing that keeps pointing somewhere after
    /// the format moves — and points at a *valid* byte, so the test still
    /// passes while checking the wrong field:
    ///
    /// ```text
    /// 0   magic(4) version(1) section(1) count(2)      header, 8 bytes
    /// 8   kind(1) key(8) flags(1)                      node prefix, 10
    /// 18  width: tag(1) value(2)                       layout begins
    /// 21  height: tag(1) value(2)
    /// 24  min_width(1) max_width(1)                    absent: presence only
    /// 26  min_height(1) max_height(1)
    /// 28  grow(4)
    /// 32  shrink(4)
    /// 36  align_self(1)                                absent: presence only
    /// 37  align_items(1)
    /// 38  justify_content(1)
    /// 39  padding(2) gap(2)
    /// 43  child_count(2)
    /// 45  SECTION_END(1)
    /// ```
    const DEFAULT_ALIGN_ITEMS_OFFSET: usize = 37;
    const DEFAULT_BATCH_LEN: usize = 46;

    #[test]
    fn rejects_unknown_align_and_justify_tags() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROOT,
            NodeKey::first(0),
            0,
            None,
            WireLayout::default(),
            0,
        );
        let good = encoder.finish();
        assert_eq!(
            good.len(),
            DEFAULT_BATCH_LEN,
            "the offsets below describe this exact encoding; if the layout \
             block changed shape, update the map with it"
        );

        for (offset, context) in [
            (DEFAULT_ALIGN_ITEMS_OFFSET, "align"),
            (DEFAULT_ALIGN_ITEMS_OFFSET + 1, "justify"),
        ] {
            let mut bytes = good.clone();
            bytes[offset] = 99;
            assert!(
                matches!(
                    decode_batch(&bytes),
                    Err(ProtocolError::UnknownOpcode { context: c, value: 99 }) if c == context
                ),
                "byte {offset} is the {context} tag and 99 is not one"
            );
        }
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
                NodeKey::first(id as u32),
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
            NodeKey::first(0),
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
                NodeKey::first(0),
                0,
                None,
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_ROOT,
                NodeKey::first(1),
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
            WireEvent::decode(&[&EVENT_MAGIC[..], &[PROTOCOL_VERSION, 0x09]].concat()),
            Err(ProtocolError::UnknownOpcode {
                context: "event",
                ..
            })
        ));
    }
}
