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

pub mod accessibility;
pub mod diff;
pub mod focus;
pub mod layout;
pub mod ledger;
pub mod scroll;
pub mod text;

pub use accessibility::{A11yProjection, project as project_accessibility};
pub use diff::{ChangeSet, diff};
pub use focus::{FocusMove, FocusState, RevealAlignment, focusable_order, reveal};
pub use instar_ui_protocol as protocol;
pub use instar_ui_protocol::{
    NodeKey, ProtocolError, WireAlign, WireBasis, WireBorder, WireColor, WireCursor, WireDisplay,
    WireFontRole, WireJustify, WireLayout, WireOverflow, WirePaintStyle, WireSize, WireStyle,
    WireTextAlign, WireTextLayout, WireTextStyle, WireVisibility, limits,
};
pub use layout::{BUTTON_PADDING, LayoutSnapshot, Rect, Viewport};
pub use ledger::{KeyLedger, MAX_NODE_IDS};
pub use scroll::{
    MIN_THUMB_LENGTH, SCROLLBAR_THICKNESS, ScrollDeltaPixels, ScrollOffset, ScrollOutcome,
    ScrollState, Scrollbar, ScrollbarPart, ScrollbarStyle, ThumbDrag,
};
pub use text::{
    Alignment, Available, FontFace, FontRole, Glyph, ShapedRun, ShapedText, ShapingStyle,
    TextContext, TextStats,
};

use instar_ui_protocol::{BatchEncoder, WireBatch, WireEvent, WireNode, flags, opcode};

/// What a node is, semantically. Presentation is not described here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// The single outermost node, sized to the viewport.
    Root,
    /// Stacks children vertically. Not interactive.
    Column,
    /// Stacks children horizontally. Not interactive.
    Row,
    /// Lays children on top of one another at the content-box origin. Later
    /// children paint over earlier ones. Not interactive.
    Stack,
    /// A retained viewport over exactly one content child.
    ///
    /// The scroll offset is host-owned and appears nowhere on the wire: a
    /// guest describes what is scrollable, never where it is scrolled to.
    /// Not interactive itself — the content inside it is.
    Scroll,
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

    /// A stable name for this kind, for errors and diagnostics.
    ///
    /// Not `Debug`: this is used in messages a guest author reads, and it must
    /// not start reporting a node's *contents* because a variant gained a
    /// field.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Column => "column",
            Self::Row => "row",
            Self::Stack => "stack",
            Self::Scroll => "scroll",
            Self::Text { .. } => "text",
            Self::Button { .. } => "button",
        }
    }
}

/// One node in the retained tree.
///
/// Not `Eq`: [`WireLayout`] carries flex factors, and `f32` has no total
/// equality. Decoding guarantees they are finite, which makes `PartialEq`
/// behave, but the trait bound cannot say so.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub key: NodeKey,
    pub kind: NodeKind,
    /// Layout *intent*. The host turns this into geometry; the guest never
    /// states a rectangle.
    pub layout: WireLayout,
    /// Appearance, grouped by what a change to it costs. See [`WireStyle`].
    pub style: WireStyle,
    pub children: Vec<Node>,
}

impl Node {
    /// A container that spans its parent's cross axis.
    ///
    /// Every container constructor here wants the same thing and used to spell
    /// it `width: Fill`, which only read correctly under a column. `Stretch`
    /// says it once and means it under either direction.
    fn container(key: u32, kind: NodeKind, children: Vec<Node>) -> Self {
        Self {
            key: NodeKey::first(key),
            kind,
            layout: WireLayout {
                align_self: Some(WireAlign::Stretch),
                ..WireLayout::default()
            },
            style: WireStyle::default(),
            children,
        }
    }

    pub fn root(key: u32, children: Vec<Node>) -> Self {
        Self::container(key, NodeKind::Root, children)
    }

    pub fn column(key: u32, children: Vec<Node>) -> Self {
        Self::container(key, NodeKind::Column, children)
    }

    /// A horizontal container. Spans its parent's cross axis, because the
    /// common `Row` is a toolbar or header rather than something that hugs its
    /// children.
    pub fn row(key: u32, children: Vec<Node>) -> Self {
        Self::container(key, NodeKind::Row, children)
    }

    /// An overlapping container. Spans its parent's cross axis, because the
    /// common `Stack` is an overlay over whatever it covers.
    pub fn stack(key: u32, children: Vec<Node>) -> Self {
        Self::container(key, NodeKind::Stack, children)
    }

    /// A retained viewport over one content child.
    ///
    /// One child, not a list: see [`TreeError::ScrollArity`]. Clips by
    /// definition, so `Overflow` is not a separate decision here.
    pub fn scroll(key: u32, content: Node) -> Self {
        Self::container(key, NodeKind::Scroll, vec![content])
    }

    pub fn text(key: u32, text: impl Into<String>) -> Self {
        Self {
            key: NodeKey::first(key),
            kind: NodeKind::Text { text: text.into() },
            layout: WireLayout::default(),
            style: WireStyle::default(),
            children: Vec::new(),
        }
    }

    pub fn button(key: u32, label: impl Into<String>) -> Self {
        Self {
            key: NodeKey::first(key),
            kind: NodeKind::Button {
                label: label.into(),
                enabled: true,
            },
            layout: WireLayout::default(),
            style: WireStyle::default(),
            children: Vec::new(),
        }
    }

    pub fn with_layout(mut self, layout: WireLayout) -> Self {
        self.layout = layout;
        self
    }

    /// Share of surplus main-axis space. See [`WireLayout::grow`].
    pub fn with_grow(mut self, grow: f32) -> Self {
        self.layout.grow = grow;
        self
    }

    /// This node's own cross-axis placement, overriding the parent's
    /// `align_items`.
    pub fn with_align_self(mut self, align: WireAlign) -> Self {
        self.layout.align_self = Some(align);
        self
    }

    /// Absent from layout, paint, hit-testing and accessibility, along with
    /// everything under it.
    pub fn display_none(mut self) -> Self {
        self.layout.display = WireDisplay::None;
        self
    }

    /// Keeps its space and shows nothing, for itself and its whole subtree.
    pub fn hidden(mut self) -> Self {
        self.layout.visibility = WireVisibility::Hidden;
        self
    }

    pub fn with_style(mut self, style: WireStyle) -> Self {
        self.style = style;
        self
    }

    /// Paint-only. Changing this must not reshape or re-lay-out anything.
    pub fn with_foreground(mut self, color: WireColor) -> Self {
        self.style.paint.foreground = Some(color);
        self
    }

    pub fn with_background(mut self, color: WireColor) -> Self {
        self.style.paint.background = Some(color);
        self
    }

    pub fn with_border(mut self, width: u16, color: WireColor) -> Self {
        self.style.paint.border = Some(WireBorder { width, color });
        self
    }

    pub fn with_corner_radius(mut self, radius: u16) -> Self {
        self.style.paint.corner_radius = radius;
        self
    }

    /// Shaping-affecting: this one *does* invalidate the text cache.
    pub fn with_font_size(mut self, size: u16) -> Self {
        self.style.text.size = size;
        self
    }

    pub fn with_font_weight(mut self, weight: u16) -> Self {
        self.style.text.weight = weight;
        self
    }

    pub fn with_font_role(mut self, role: WireFontRole) -> Self {
        self.style.text.role = role;
        self
    }

    /// Interaction-only. Invalidates nothing that is measured or drawn.
    pub fn with_cursor(mut self, cursor: WireCursor) -> Self {
        self.style.cursor = cursor;
        self
    }

    /// Clips descendants to this node's rectangle. Does not scroll.
    pub fn clipped(mut self) -> Self {
        self.layout.overflow = WireOverflow::Clip;
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
    /// A snapshot named the same node id twice. The check is on the id, not
    /// the whole `NodeKey`: `(7, 0)` and `(7, 1)` are distinct keys but still
    /// two live nodes under one id.
    #[error("duplicate node key {0}")]
    DuplicateKey(NodeKey),
    #[error("{key} is a fresh id, but a never-seen id must start at generation 0")]
    BadFirstGeneration { key: NodeKey },
    #[error(
        "{key} is live at generation {live}; a live id must be resent at exactly that generation"
    )]
    GenerationChanged { key: NodeKey, live: u32 },
    #[error(
        "{key} was retired at generation {retired}; a retired id may only return at a higher generation"
    )]
    GenerationNotAdvanced { key: NodeKey, retired: u32 },
    #[error(
        "this snapshot would bring the count of distinct ids ever observed to {observed}, over the {} id limit",
        crate::ledger::MAX_NODE_IDS
    )]
    TooManyNodeIds { observed: usize },
    #[error("the root node is a {0}, but a tree's root must be a root node")]
    BadRoot(&'static str),
    #[error("{0} is a root node, but only the outermost node may be one")]
    NestedRoot(NodeKey),
    /// A key that named one kind of node in the retained tree names another in
    /// the new snapshot.
    ///
    /// Refused rather than treated as a replacement. The host holds transient
    /// state against keys — focus, scroll offset, an in-flight press — and
    /// silently swapping the node behind a key would move that state onto an
    /// unrelated control. A guest that wants a different node should use a
    /// different key.
    /// A `Scroll` had other than exactly one child.
    ///
    /// One child gives one unambiguous content extent — a union of several
    /// overlapping boxes has more than one defensible answer — and it stops
    /// `Scroll` quietly becoming a layout container as well as a viewport. An
    /// app that wants several things puts a `Stack` or a `Column` there.
    #[error("{key} is a scroll with {children} children; a scroll takes exactly one")]
    ScrollArity { key: NodeKey, children: usize },
    #[error("{key} was a {was} and is now a {now}; reuse of a key for a different kind of node")]
    KindChanged {
        key: NodeKey,
        was: &'static str,
        now: &'static str,
    },
}

/// A committed UI tree.
#[derive(Debug, Clone, PartialEq)]
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
            if !seen.insert(node.key.id) {
                // Duplicate ids would make hit-test results ambiguous and
                // make a targeted update land on the wrong node. Generations
                // do not make two live nodes of one id sensible.
                return Err(TreeError::DuplicateKey(node.key));
            }
        }

        // Structural rules the wire deliberately does not enforce: exactly one
        // root, and it is outermost. The protocol layer reports what the bytes
        // say; whether that is a sensible interface is this crate's call.
        match batch.nodes[0].kind {
            opcode::NODE_ROOT => {}
            opcode::NODE_COLUMN => return Err(TreeError::BadRoot("column")),
            opcode::NODE_ROW => return Err(TreeError::BadRoot("row")),
            opcode::NODE_STACK => return Err(TreeError::BadRoot("stack")),
            opcode::NODE_SCROLL => return Err(TreeError::BadRoot("scroll")),
            opcode::NODE_TEXT => return Err(TreeError::BadRoot("text")),
            _ => return Err(TreeError::BadRoot("button")),
        }
        for node in &batch.nodes[1..] {
            if node.kind == opcode::NODE_ROOT {
                return Err(TreeError::NestedRoot(node.key));
            }
        }
        // Checked on the flat wire nodes rather than after assembly, because
        // `child_count` is what the guest actually said and is available
        // before anything is built.
        for node in &batch.nodes {
            if node.kind == opcode::NODE_SCROLL && node.child_count != 1 {
                return Err(TreeError::ScrollArity {
                    key: node.key,
                    children: node.child_count as usize,
                });
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
    pub fn layout(&self, text: &mut TextContext, viewport: Viewport) -> LayoutSnapshot {
        self.layout_with(text, viewport, ScrollbarStyle::default())
    }

    /// Layout under an explicit scrollbar presentation policy.
    ///
    /// [`ScrollbarStyle::Inset`] narrows a viewport's content rectangle to
    /// make room for its chrome, which is a layout fact and therefore has to
    /// be decided here rather than at paint time. The policy is the host's to
    /// choose; the type lives here because layout is what has to honour it.
    pub fn layout_with(
        &self,
        text: &mut TextContext,
        viewport: Viewport,
        scrollbars: ScrollbarStyle,
    ) -> LayoutSnapshot {
        layout::compute(text, self, viewport, scrollbars)
    }

    /// Finds the innermost interactive node containing `(x, y)`.
    ///
    /// Depth-first, last-child-first, so a node drawn later (on top) wins over
    /// an earlier sibling it overlaps. Non-interactive nodes never match but
    /// are still descended into, so a button inside a container is reachable.
    /// Nodes absent from the snapshot have no geometry and cannot be hit.
    /// Hit-tests a tree with no viewport scrolled anywhere.
    ///
    /// Equivalent to [`Tree::hit_test_scrolled`] with an empty
    /// [`ScrollState`], which is what an unscrolled interface has. Kept as the
    /// simple spelling because most callers and most tests have nothing to
    /// scroll.
    pub fn hit_test(&self, layout: &LayoutSnapshot, x: i32, y: i32) -> Option<&Node> {
        self.hit_test_scrolled(layout, &ScrollState::new(), x, y)
    }

    /// Hit-tests against the host's current scroll offsets.
    ///
    /// The offsets are a separate argument for the same reason the
    /// [`LayoutSnapshot`] is: they are host presentation state, not something
    /// the tree carries or the guest can influence.
    pub fn hit_test_scrolled(
        &self,
        layout: &LayoutSnapshot,
        scroll: &ScrollState,
        x: i32,
        y: i32,
    ) -> Option<&Node> {
        hit_test_node(&self.root, layout, scroll, x, y, None)
    }
}

fn assemble(nodes: &[WireNode], cursor: &mut usize) -> Result<Node, TreeError> {
    let wire = nodes.get(*cursor).ok_or(TreeError::Empty)?;
    *cursor += 1;

    let kind = match wire.kind {
        opcode::NODE_ROOT => NodeKind::Root,
        opcode::NODE_COLUMN => NodeKind::Column,
        opcode::NODE_ROW => NodeKind::Row,
        opcode::NODE_STACK => NodeKind::Stack,
        opcode::NODE_SCROLL => NodeKind::Scroll,
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
        style: wire.style,
        children,
    })
}

fn encode_node(encoder: &mut BatchEncoder, node: &Node) {
    let (kind, text, node_flags) = match &node.kind {
        NodeKind::Root => (opcode::NODE_ROOT, None, 0),
        NodeKind::Column => (opcode::NODE_COLUMN, None, 0),
        NodeKind::Row => (opcode::NODE_ROW, None, 0),
        NodeKind::Stack => (opcode::NODE_STACK, None, 0),
        NodeKind::Scroll => (opcode::NODE_SCROLL, None, 0),
        NodeKind::Text { text } => (opcode::NODE_TEXT, Some(text.as_str()), 0),
        NodeKind::Button { label, enabled } => (
            opcode::NODE_BUTTON,
            Some(label.as_str()),
            if *enabled { flags::ENABLED } else { 0 },
        ),
    };
    encoder.node_styled(
        kind,
        node.key,
        node_flags,
        text,
        node.layout,
        node.style,
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

/// The overlap of two rectangles, or an empty rect if they do not meet.
///
/// Empty rather than `None`, because [`rect_contains`] already answers "no" for
/// a non-positive extent — so an empty intersection needs no special case at
/// the call site, and nested clips compose by intersecting all the way down.
pub fn rect_intersection(a: Rect, b: Rect) -> Rect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x.saturating_add(a.width)).min(b.x.saturating_add(b.width));
    let bottom = (a.y.saturating_add(a.height)).min(b.y.saturating_add(b.height));
    Rect::new(x, y, (right - x).max(0), (bottom - y).max(0))
}

/// Whether a node and its subtree take part in presentation at all.
///
/// `Display::None` and `Visibility::Hidden` differ only in whether space is
/// reserved, and that difference is layout's business. To paint, hit-testing,
/// and accessibility they are the same answer, and both cover the whole
/// subtree.
pub fn is_presented(node: &Node) -> bool {
    node.layout.display != WireDisplay::None && node.layout.visibility != WireVisibility::Hidden
}

/// Hit-tests one node, carrying the clip its ancestors imposed.
///
/// `clip` is `None` when nothing above has clipped, which is not the same as
/// clipping to the root: with `Overflow::Visible` a child that overflows its
/// parent is genuinely reachable outside it, and that is what makes `Clip`
/// mean something.
///
/// This is a behaviour change A3 had to make deliberately. The previous
/// version returned early unless the point was inside the *parent's* rect
/// before descending, which quietly clipped every node in the tree — so
/// `Overflow::Visible` would have been a property that did nothing, and
/// `Clip` a property that changed nothing.
fn hit_test_node<'a>(
    node: &'a Node,
    layout: &LayoutSnapshot,
    scroll: &ScrollState,
    x: i32,
    y: i32,
    clip: Option<Rect>,
) -> Option<&'a Node> {
    if !is_presented(node) {
        return None;
    }
    let Some(rect) = layout.get(node.key) else {
        // No geometry means nothing to hit, and nothing beneath it either --
        // an unlaid-out subtree is not on screen.
        return None;
    };

    // One ordering, extended rather than duplicated:
    //
    //     ancestor clip  ->  this node's clip  ->  translate  ->  descend
    //
    // A `Scroll` clips because it is a viewport, so it joins `Overflow::Clip`
    // at the same step instead of introducing a second clipping path. Only the
    // translation is new, and it sits between the clip and the descent for the
    // reason the clip came first: reversed, the pointer lands inside a child's
    // translated rect while being outside the viewport that owns it, and
    // content scrolled out of view answers hits.
    let clips = node.layout.overflow == WireOverflow::Clip || matches!(node.kind, NodeKind::Scroll);
    let clip = if clips {
        Some(match clip {
            Some(outer) => rect_intersection(outer, rect),
            None => rect,
        })
    } else {
        clip
    };
    if let Some(clip) = clip
        && !rect_contains(clip, x, y)
    {
        return None;
    }

    // Whether the pointer is on *this* node, decided before any translation,
    // because this node's own rect lives in the space the pointer is still in.
    // A `Scroll` is not interactive today so this cannot currently be observed
    // — which is exactly why it is worth getting right now rather than leaving
    // a latent coordinate mismatch for whatever becomes interactive next.
    let hit_self = rect_contains(rect, x, y) && node.kind.is_interactive();

    // Into content coordinates. The offset is how far the content has moved
    // *up and left*, so a pointer at a given viewport position corresponds to
    // a content position further down and right by the same amount.
    //
    // The clip travels with it. It was measured in the untranslated space, and
    // leaving it behind would compare translated points against untranslated
    // bounds — which looks correct until something *above* the scroll also
    // clips, the nested case that is easiest to leave untested.
    let (x, y, clip) = match node.kind {
        NodeKind::Scroll => {
            let offset = scroll.get(node.key);
            let moved = clip.map(|clip| {
                Rect::new(
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

    for child in node.children.iter().rev() {
        if let Some(hit) = hit_test_node(child, layout, scroll, x, y, clip) {
            return Some(hit);
        }
    }
    hit_self.then_some(node)
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

    fn layout(tree: &Tree) -> LayoutSnapshot {
        let mut text = TextContext::new();
        tree.layout(&mut text, VIEWPORT)
    }

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
        let layout = layout(&tree);

        for key in [0, 1, 2, 3, 4] {
            assert!(
                layout.get(NodeKey::first(key)).is_some(),
                "every node should have geometry, {key} did not"
            );
        }

        let root = layout.get(NodeKey::first(0)).unwrap();
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
        assert_eq!(layout(&tree), layout(&tree));
    }

    /// Assertions are relative rather than exact, so they survive font stack
    /// changes.
    #[test]
    fn a_column_stacks_its_children_without_overlap() {
        let layout = layout(&sample());
        let text = layout.get(NodeKey::first(2)).unwrap();
        let press = layout.get(NodeKey::first(3)).unwrap();
        let reset = layout.get(NodeKey::first(4)).unwrap();

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
        let layout = layout(&sample());
        let root = layout.get(NodeKey::first(0)).unwrap();
        for key in [1, 2, 3, 4] {
            let child = layout.get(NodeKey::first(key)).unwrap();
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
                align_self: Some(WireAlign::Stretch),
                padding: 20,
                ..WireLayout::default()
            }),
        );
        let layout = layout(&padded);
        let child = layout.get(NodeKey::first(1)).unwrap();
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
                            align_self: Some(WireAlign::Stretch),
                            gap,
                            ..WireLayout::default()
                        },
                    ),
                ],
            ))
        };
        let tight = layout(&build(0));
        let loose = layout(&build(10));

        let tight_second = tight.get(NodeKey::first(3)).unwrap();
        let loose_second = loose.get(NodeKey::first(3)).unwrap();
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
                width: WireSize::Fixed(120),
                height: WireSize::Fixed(40),
                ..WireLayout::default()
            })],
        ));
        let button = layout(&tree).get(NodeKey::first(1)).unwrap();
        assert_eq!((button.width, button.height), (120, 40));
    }

    /// What `fill_width_is_wider_than_content_width` used to assert, said in
    /// the vocabulary that replaced `Fill`. Spanning the parent is cross-axis
    /// alignment, and under the root — a column — the cross axis is width.
    #[test]
    fn stretching_is_wider_than_content_width() {
        let build = |align_self: Option<WireAlign>| {
            Tree::new(Node::root(
                0,
                vec![Node::button(1, "x").with_layout(WireLayout {
                    align_self,
                    ..WireLayout::default()
                })],
            ))
        };
        let content = layout(&build(None)).get(NodeKey::first(1)).unwrap();
        let stretched = layout(&build(Some(WireAlign::Stretch)))
            .get(NodeKey::first(1))
            .unwrap();
        assert!(
            stretched.width > content.width,
            "stretch ({}) should exceed content ({}) for a one-character label",
            stretched.width,
            content.width
        );
    }

    /// `grow` is the *other* axis, and the two must not be confused again:
    /// growing changes the main-axis extent and leaves the cross axis alone.
    #[test]
    fn grow_expands_along_the_main_axis_not_the_cross_axis() {
        let build = |grow: f32| {
            Tree::new(Node::root(
                0,
                vec![Node::button(1, "x").with_layout(WireLayout {
                    grow,
                    ..WireLayout::default()
                })],
            ))
        };
        let inert = layout(&build(0.0)).get(NodeKey::first(1)).unwrap();
        let grown = layout(&build(1.0)).get(NodeKey::first(1)).unwrap();
        assert!(
            grown.height > inert.height,
            "under a column the main axis is height: {} should exceed {}",
            grown.height,
            inert.height
        );
        assert_eq!(
            grown.width, inert.width,
            "growing must not touch the cross axis"
        );
    }

    /// A row of three fixed-width children inside a fixed-width parent, so the
    /// surplus is a known quantity and the split can be asserted as a ratio.
    fn grown_row(factors: [f32; 3]) -> LayoutSnapshot {
        let children = factors
            .iter()
            .enumerate()
            .map(|(index, grow)| {
                Node::text(index as u32 + 2, "x").with_layout(WireLayout {
                    width: WireSize::Fixed(10),
                    grow: *grow,
                    ..WireLayout::default()
                })
            })
            .collect();
        let tree = Tree::new(Node::root(
            0,
            vec![Node::row(1, children).with_layout(WireLayout {
                width: WireSize::Fixed(400),
                ..WireLayout::default()
            })],
        ));
        layout(&tree)
    }

    /// The surplus a [`grown_row`] has to distribute: the container's fixed
    /// width less the three children's preferred widths.
    const ROW_SURPLUS: f32 = 400.0 - 30.0;

    /// Checks each child's share against the exact fraction it is owed.
    ///
    /// Per share, not as a ratio between the two: they are rounded to whole
    /// pixels independently, so a ratio compounds two roundings and can be
    /// 2px out while both shares are individually correct. Comparing each
    /// against its own real-valued target keeps the tolerance at the one pixel
    /// that rounding actually costs, and keeps the assertion about Instar's
    /// distribution rather than Taffy's rounding rule.
    fn assert_shares(snapshot: &LayoutSnapshot, factors: [f32; 3]) {
        let total: f32 = factors.iter().sum();
        for (index, factor) in factors.iter().enumerate() {
            let key = NodeKey::first(index as u32 + 2);
            let share = snapshot.get(key).unwrap().width - 10;
            let expected = ROW_SURPLUS * factor / total;
            assert!(
                (share as f32 - expected).abs() <= 1.0,
                "child {index} with grow {factor} took {share} of {ROW_SURPLUS}, \
                 owed about {expected:.1}"
            );
        }
    }

    #[test]
    fn grow_splits_surplus_in_proportion() {
        let snapshot = grown_row([1.0, 2.0, 0.0]);
        assert_shares(&snapshot, [1.0, 2.0, 0.0]);
        assert_eq!(
            snapshot.get(NodeKey::first(4)).unwrap().width,
            10,
            "grow: 0 stays at its preferred size"
        );
        let total: i32 = [2, 3, 4]
            .iter()
            .map(|key| snapshot.get(NodeKey::first(*key)).unwrap().width)
            .sum();
        assert_eq!(total, 400, "and the surplus is fully consumed");
    }

    /// The reason the wire carries a float here rather than an integer.
    #[test]
    fn a_fractional_grow_distributes_proportionally() {
        assert_shares(&grown_row([0.5, 1.5, 0.0]), [0.5, 1.5, 0.0]);
    }

    #[test]
    fn shrink_contracts_children_that_would_overflow() {
        let build = |shrink: f32| {
            Tree::new(Node::root(
                0,
                vec![
                    Node::row(
                        1,
                        vec![Node::text(2, "x").with_layout(WireLayout {
                            width: WireSize::Fixed(600),
                            shrink,
                            ..WireLayout::default()
                        })],
                    )
                    .with_layout(WireLayout {
                        width: WireSize::Fixed(200),
                        ..WireLayout::default()
                    }),
                ],
            ))
        };
        let rigid = layout(&build(0.0)).get(NodeKey::first(2)).unwrap();
        let yielding = layout(&build(1.0)).get(NodeKey::first(2)).unwrap();

        assert_eq!(
            rigid.width, 600,
            "shrink: 0 overflows rather than giving way"
        );
        assert_eq!(
            yielding.width, 200,
            "the default shrink contracts to the container"
        );
    }

    #[test]
    fn minimum_and_maximum_bound_the_computed_size() {
        let build = |min: Option<u16>, max: Option<u16>| {
            Tree::new(Node::root(
                0,
                vec![Node::text(1, "hi").with_layout(WireLayout {
                    min_width: min,
                    max_width: max,
                    ..WireLayout::default()
                })],
            ))
        };
        let natural = layout(&build(None, None)).get(NodeKey::first(1)).unwrap();
        assert!(
            natural.width < 300,
            "the fixture assumes 'hi' measures narrower than its minimum"
        );

        assert_eq!(
            layout(&build(Some(300), None))
                .get(NodeKey::first(1))
                .unwrap()
                .width,
            300,
            "a minimum wins over a smaller content size"
        );
        assert_eq!(
            layout(&build(None, Some(4)))
                .get(NodeKey::first(1))
                .unwrap()
                .width,
            4,
            "a maximum caps a larger content size"
        );
    }

    #[test]
    fn align_items_applies_to_children_that_state_no_align_self() {
        let build = |align_items: WireAlign, child: Option<WireAlign>| {
            let mut text = Node::text(2, "x");
            if let Some(child) = child {
                text = text.with_align_self(child);
            }
            Tree::new(Node::root(
                0,
                vec![Node::column(1, vec![text]).with_layout(WireLayout {
                    width: WireSize::Fixed(400),
                    align_items,
                    ..WireLayout::default()
                })],
            ))
        };

        let start = layout(&build(WireAlign::Start, None))
            .get(NodeKey::first(2))
            .unwrap();
        let inherited_stretch = layout(&build(WireAlign::Stretch, None))
            .get(NodeKey::first(2))
            .unwrap();
        assert!(
            inherited_stretch.width > start.width,
            "a child with no align_self takes the parent's align_items"
        );

        let overridden = layout(&build(WireAlign::Stretch, Some(WireAlign::Start)))
            .get(NodeKey::first(2))
            .unwrap();
        assert_eq!(
            overridden.width, start.width,
            "the child's own align_self overrides the parent's align_items"
        );
    }

    #[test]
    fn justify_content_distributes_along_the_main_axis() {
        let build = |justify: WireJustify| {
            Tree::new(Node::root(
                0,
                vec![
                    Node::row(1, vec![Node::text(2, "a"), Node::text(3, "b")]).with_layout(
                        WireLayout {
                            width: WireSize::Fixed(400),
                            justify_content: justify,
                            ..WireLayout::default()
                        },
                    ),
                ],
            ))
        };

        let start = layout(&build(WireJustify::Start));
        let centred = layout(&build(WireJustify::Center));
        let between = layout(&build(WireJustify::SpaceBetween));

        assert_eq!(
            start.get(NodeKey::first(2)).unwrap().x,
            0,
            "Start leaves the first child at the origin"
        );
        assert!(
            centred.get(NodeKey::first(2)).unwrap().x > 0,
            "Center moves the group inward"
        );

        let first = between.get(NodeKey::first(2)).unwrap();
        let second = between.get(NodeKey::first(3)).unwrap();
        assert_eq!(first.x, 0, "SpaceBetween pins the first child to the edge");
        assert_eq!(
            second.x + second.width,
            400,
            "and the last to the other edge"
        );
    }

    #[test]
    fn longer_text_measures_wider() {
        let build = |text: &str| Tree::new(Node::root(0, vec![Node::text(1, text)]));
        let short = layout(&build("hi")).get(NodeKey::first(1)).unwrap();
        let long = layout(&build("a much longer line of text"))
            .get(NodeKey::first(1))
            .unwrap();
        assert!(
            long.width > short.width,
            "intrinsic measurement should make longer text wider"
        );
    }

    #[test]
    fn a_narrower_viewport_narrows_filled_nodes() {
        let tree = sample();
        let mut text = TextContext::new();
        let wide = tree.layout(&mut text, Viewport::new(800.0, 300.0));
        let narrow = tree.layout(&mut text, Viewport::new(200.0, 300.0));
        assert!(
            narrow.get(NodeKey::first(1)).unwrap().width
                < wide.get(NodeKey::first(1)).unwrap().width,
            "a filled column should follow the viewport"
        );
    }

    #[test]
    fn a_row_places_children_left_to_right_without_overlap() {
        let tree = Tree::new(Node::root(
            0,
            vec![Node::row(
                1,
                vec![Node::text(2, "a"), Node::text(3, "b"), Node::text(4, "c")],
            )],
        ));
        let layout = layout(&tree);
        let a = layout.get(NodeKey::first(2)).unwrap();
        let b = layout.get(NodeKey::first(3)).unwrap();
        let c = layout.get(NodeKey::first(4)).unwrap();

        assert!(
            a.x < b.x && b.x < c.x,
            "children run left to right in declaration order: {a:?} {b:?} {c:?}"
        );
        assert!(
            a.x + a.width <= b.x && b.x + b.width <= c.x,
            "row siblings must not overlap horizontally"
        );
        assert_eq!(
            (a.y, b.y, c.y),
            (a.y, a.y, a.y),
            "row siblings share one line: {a:?} {b:?} {c:?}"
        );
    }

    #[test]
    fn a_rows_gap_separates_siblings_horizontally_only() {
        let build = |gap: u16| {
            Tree::new(Node::root(
                0,
                vec![
                    Node::row(1, vec![Node::text(2, "a"), Node::text(3, "b")]).with_layout(
                        WireLayout {
                            align_self: Some(WireAlign::Stretch),
                            gap,
                            ..WireLayout::default()
                        },
                    ),
                ],
            ))
        };
        let tight = layout(&build(0));
        let loose = layout(&build(10));

        let tight_second = tight.get(NodeKey::first(3)).unwrap();
        let loose_second = loose.get(NodeKey::first(3)).unwrap();
        assert_eq!(
            loose_second.x - tight_second.x,
            10,
            "a 10px gap should push the second child right by exactly 10px"
        );
        assert_eq!(
            loose_second.y, tight_second.y,
            "a row's gap is main-axis space and must not add vertical space"
        );
    }

    #[test]
    fn a_stack_places_every_child_at_the_same_origin() {
        let tree = Tree::new(Node::root(
            0,
            vec![Node::stack(
                1,
                vec![
                    Node::text(2, "small"),
                    Node::text(3, "a much longer line of text"),
                ],
            )],
        ));
        let layout = layout(&tree);
        let small = layout.get(NodeKey::first(2)).unwrap();
        let long = layout.get(NodeKey::first(3)).unwrap();

        assert_eq!(
            (small.x, small.y),
            (long.x, long.y),
            "every stack child starts at the content-box origin: {small:?} {long:?}"
        );
    }

    #[test]
    fn a_stack_sizes_to_its_largest_child() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::stack(
                    1,
                    vec![
                        Node::text(2, "small"),
                        Node::text(3, "a much longer line of text"),
                    ],
                )
                .with_layout(WireLayout {
                    width: WireSize::Content,
                    ..WireLayout::default()
                }),
            ],
        ));
        let layout = layout(&tree);
        let stack = layout.get(NodeKey::first(1)).unwrap();
        let small = layout.get(NodeKey::first(2)).unwrap();
        let long = layout.get(NodeKey::first(3)).unwrap();

        assert_eq!(
            (stack.width, stack.height),
            (long.width, long.height),
            "the stack should take the largest child's size, not the sum: \
             stack {stack:?}, small {small:?}, long {long:?}"
        );
        assert!(
            small.width < long.width,
            "children must keep their natural size rather than stretch to the \
             cell: {small:?} {long:?}"
        );
    }

    // --- Hit-testing, against host-computed geometry. ---

    #[test]
    fn hit_test_finds_the_button_the_host_placed() {
        let tree = sample();
        let layout = layout(&tree);
        let button = layout.get(NodeKey::first(3)).unwrap();

        let hit = tree.hit_test(&layout, button.x + 1, button.y + 1);
        assert_eq!(
            hit.map(|n| n.key),
            Some(NodeKey::first(3)),
            "a point inside the button's computed rect should hit it"
        );
    }

    // --- A3: Display, Visibility, Overflow. ---

    /// Root > column > [button 2, button 3], where the column may be
    /// suppressed and button 3 is what we aim at.
    fn suppressible(suppress: fn(Node) -> Node) -> Tree {
        Tree::new(Node::root(
            0,
            vec![suppress(Node::column(
                1,
                vec![Node::button(2, "first"), Node::button(3, "second")],
            ))],
        ))
    }

    #[test]
    fn display_none_leaves_layout_entirely() {
        let visible = layout(&suppressible(|node| node));
        let hidden = layout(&suppressible(Node::display_none));

        assert!(
            visible.get(NodeKey::first(3)).is_some(),
            "the fixture's button is laid out when nothing suppresses it"
        );
        for key in [1, 2, 3] {
            assert_eq!(
                hidden.get(NodeKey::first(key)),
                None,
                "node {key} is under Display::None and must have no geometry"
            );
        }
    }

    #[test]
    fn visibility_hidden_keeps_its_space() {
        let visible = layout(&suppressible(|node| node));
        let hidden = layout(&suppressible(Node::hidden));

        assert_eq!(
            hidden.get(NodeKey::first(3)),
            visible.get(NodeKey::first(3)),
            "Hidden still participates in layout -- that is the whole \
             difference from Display::None"
        );
    }

    #[test]
    fn neither_display_none_nor_hidden_can_be_hit() {
        let reference = suppressible(|node| node);
        let snapshot = layout(&reference);
        let target = snapshot.get(NodeKey::first(3)).unwrap();
        let (x, y) = (target.x + 1, target.y + 1);
        assert_eq!(
            reference.hit_test(&snapshot, x, y).map(|node| node.key),
            Some(NodeKey::first(3)),
            "the fixture is hittable before anything suppresses it"
        );

        for (what, tree) in [
            ("Display::None", suppressible(Node::display_none)),
            ("Visibility::Hidden", suppressible(Node::hidden)),
        ] {
            // Hidden keeps its geometry, so this deliberately hit-tests
            // against the *visible* snapshot: the point is that suppression
            // refuses the hit even where the rectangle still exists.
            assert_eq!(
                tree.hit_test(&snapshot, x, y),
                None,
                "a descendant of {what} must not be hittable"
            );
        }
    }

    /// The rule CSS has and Instar deliberately does not.
    #[test]
    fn a_visible_child_of_a_hidden_parent_stays_hidden() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::column(
                    1,
                    vec![Node::button(2, "press").with_layout(WireLayout {
                        visibility: WireVisibility::Visible,
                        ..WireLayout::default()
                    })],
                )
                .hidden(),
            ],
        ));
        let snapshot = layout(&suppressible(|node| node));
        let target = snapshot.get(NodeKey::first(2)).unwrap();
        assert_eq!(
            tree.hit_test(&snapshot, target.x + 1, target.y + 1),
            None,
            "suppression is subtree-wide; a descendant cannot opt back in"
        );
    }

    /// A child placed outside its parent, so clipping has something to clip.
    fn overflowing(clip: bool) -> Tree {
        let inner = Node::button(2, "out").with_layout(WireLayout {
            width: WireSize::Fixed(80),
            height: WireSize::Fixed(20),
            ..WireLayout::default()
        });
        let mut parent = Node::column(1, vec![inner]).with_layout(WireLayout {
            width: WireSize::Fixed(40),
            height: WireSize::Fixed(10),
            ..WireLayout::default()
        });
        if clip {
            parent = parent.clipped();
        }
        Tree::new(Node::root(0, vec![parent]))
    }

    #[test]
    fn overflow_clip_restricts_hit_testing_and_visible_does_not() {
        let open = overflowing(false);
        let snapshot = layout(&open);
        let child = snapshot.get(NodeKey::first(2)).unwrap();
        let parent = snapshot.get(NodeKey::first(1)).unwrap();

        // A point inside the child but past the parent's bottom edge.
        let (x, y) = (child.x + 1, parent.y + parent.height + 1);
        assert!(
            y < child.y + child.height,
            "the fixture must actually overflow for this to test anything"
        );

        assert_eq!(
            open.hit_test(&snapshot, x, y).map(|node| node.key),
            Some(NodeKey::first(2)),
            "Overflow::Visible means an overflowing child is genuinely reachable"
        );
        assert_eq!(
            overflowing(true).hit_test(&snapshot, x, y),
            None,
            "Overflow::Clip refuses the same point"
        );
    }

    #[test]
    fn overflow_clip_does_not_change_layout() {
        assert_eq!(
            layout(&overflowing(true)).get(NodeKey::first(2)),
            layout(&overflowing(false)).get(NodeKey::first(2)),
            "clipping is a paint and hit-test rule, never a layout one"
        );
    }

    #[test]
    fn nested_clips_intersect() {
        let inner_rect = Rect::new(0, 0, 100, 100);
        let outer_rect = Rect::new(0, 0, 50, 50);
        assert_eq!(
            rect_intersection(outer_rect, inner_rect),
            Rect::new(0, 0, 50, 50),
            "the intersection is the tighter of the two"
        );
        assert_eq!(
            rect_intersection(Rect::new(0, 0, 10, 10), Rect::new(20, 20, 10, 10)),
            Rect::new(20, 20, 0, 0),
            "disjoint rectangles intersect to something empty, which \
             rect_contains already answers no for"
        );
    }

    // --- B1: the retained viewport. ---

    /// A 100-tall viewport over 400-tall content, with a button at content
    /// y = 200 — well outside the viewport until something scrolls.
    ///
    /// The numbers are concrete on purpose. A property test over arbitrary
    /// offsets would pass against a sign error as readily as against the right
    /// answer; "200 minus 150 is 50" does not.
    fn scrolled_fixture() -> Tree {
        Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    1,
                    Node::column(
                        2,
                        vec![
                            Node::text(3, "spacer").with_layout(WireLayout {
                                height: WireSize::Fixed(200),
                                ..WireLayout::default()
                            }),
                            Node::button(4, "target").with_layout(WireLayout {
                                height: WireSize::Fixed(40),
                                ..WireLayout::default()
                            }),
                            Node::text(5, "tail").with_layout(WireLayout {
                                height: WireSize::Fixed(160),
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
        ))
    }

    #[test]
    fn the_fixture_puts_its_target_outside_the_viewport() {
        let snapshot = layout(&scrolled_fixture());
        assert_eq!(
            snapshot.get(NodeKey::first(1)).unwrap().height,
            100,
            "the viewport is 100 tall"
        );
        assert_eq!(
            snapshot.get(NodeKey::first(4)).unwrap().y,
            200,
            "and the target sits at content y = 200, outside it"
        );
    }

    #[test]
    fn scrolling_brings_content_into_the_viewport_and_hit_testing_follows() {
        let tree = scrolled_fixture();
        let snapshot = layout(&tree);
        let mut scroll = ScrollState::new();
        scroll.set(NodeKey::first(1), ScrollOffset::new(0, 150));

        // 200 in content space, minus an offset of 150, is 50 in the viewport.
        assert_eq!(
            tree.hit_test_scrolled(&snapshot, &scroll, 5, 50)
                .map(|node| node.key),
            Some(NodeKey::first(4)),
            "a click at viewport y = 50 must reach the button at content y = 200"
        );
        assert_eq!(
            tree.hit_test_scrolled(&snapshot, &scroll, 5, 200),
            None,
            "and the button's unscrolled position is now outside the viewport, \
             so nothing is there"
        );
    }

    #[test]
    fn nothing_outside_the_viewport_can_be_hit_however_far_it_scrolls() {
        let tree = scrolled_fixture();
        let snapshot = layout(&tree);
        let mut scroll = ScrollState::new();
        scroll.set(NodeKey::first(1), ScrollOffset::new(0, 150));

        for y in [101, 150, 260, 399] {
            assert_eq!(
                tree.hit_test_scrolled(&snapshot, &scroll, 5, y),
                None,
                "y = {y} is past the viewport's bottom edge and must not hit"
            );
        }
    }

    #[test]
    fn an_unscrolled_viewport_behaves_as_though_scroll_did_not_exist() {
        let tree = scrolled_fixture();
        let snapshot = layout(&tree);
        assert_eq!(
            tree.hit_test(&snapshot, 5, 50),
            None,
            "at offset zero the target is still at 200 and the viewport ends at 100"
        );
    }

    /// The composition A3's ordering promised: ancestor clip, viewport clip,
    /// translation, descent. A parallel clipping path inside `Scroll` would
    /// pass every test above and fail this one.
    #[test]
    fn an_ancestor_clip_still_binds_inside_a_scrolled_viewport() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                // Clips to 40 tall, around a viewport that is 100 tall.
                Node::column(
                    1,
                    vec![
                        Node::scroll(
                            2,
                            Node::column(
                                3,
                                vec![
                                    Node::text(4, "spacer").with_layout(WireLayout {
                                        height: WireSize::Fixed(200),
                                        ..WireLayout::default()
                                    }),
                                    Node::button(5, "target").with_layout(WireLayout {
                                        height: WireSize::Fixed(40),
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
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(40),
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                })
                .clipped(),
            ],
        ));
        let snapshot = layout(&tree);
        let mut scroll = ScrollState::new();
        // 200 - 180 = 20, so the 40-tall button spans viewport y 20..60 --
        // straddling the ancestor's 40px clip, which is what makes the two
        // assertions below disagree.
        scroll.set(NodeKey::first(2), ScrollOffset::new(0, 180));

        assert_eq!(
            tree.hit_test_scrolled(&snapshot, &scroll, 5, 30)
                .map(|node| node.key),
            Some(NodeKey::first(5)),
            "inside both the ancestor clip and the viewport, the scrolled \
             button is reachable"
        );
        assert_eq!(
            tree.hit_test_scrolled(&snapshot, &scroll, 5, 50),
            None,
            "the same button, past the ancestor's 40px clip: the viewport \
             alone would have allowed this, so only a composed clip refuses it"
        );
    }

    /// The A3 half of the retirement invariant.
    #[test]
    fn hiding_a_pressed_node_retires_the_press() {
        let reference = suppressible(|node| node);
        let snapshot = layout(&reference);
        let target = snapshot.get(NodeKey::first(3)).unwrap();

        for (what, tree) in [
            ("Display::None", suppressible(Node::display_none)),
            ("Visibility::Hidden", suppressible(Node::hidden)),
        ] {
            let mut interaction = Interaction::new();
            interaction.on_press(
                &reference,
                &snapshot,
                &ScrollState::new(),
                target.x + 1,
                target.y + 1,
            );
            assert_eq!(
                interaction.pressed(),
                Some(NodeKey::first(3)),
                "the press lands before anything is suppressed"
            );

            interaction.retire_hidden(&tree);
            assert_eq!(
                interaction.pressed(),
                None,
                "{what} must retire a press against its subtree"
            );
        }
    }

    #[test]
    fn an_unaffected_press_survives_a_commit_that_hides_something_else() {
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::button(2, "kept"),
                Node::column(3, vec![Node::button(4, "gone")]).hidden(),
            ],
        ));
        let snapshot = layout(&Tree::new(Node::root(
            0,
            vec![
                Node::button(2, "kept"),
                Node::column(3, vec![Node::button(4, "gone")]),
            ],
        )));
        let target = snapshot.get(NodeKey::first(2)).unwrap();

        let mut interaction = Interaction::new();
        interaction.on_press(
            &tree,
            &snapshot,
            &ScrollState::new(),
            target.x + 1,
            target.y + 1,
        );
        assert_eq!(interaction.pressed(), Some(NodeKey::first(2)));

        interaction.retire_hidden(&tree);
        assert_eq!(
            interaction.pressed(),
            Some(NodeKey::first(2)),
            "retirement must not become a blanket cancel"
        );
    }

    #[test]
    fn a_disabled_button_is_not_hit() {
        let tree = sample();
        let layout = layout(&tree);
        let reset = layout.get(NodeKey::first(4)).unwrap();
        assert_eq!(
            tree.hit_test(&layout, reset.x + 1, reset.y + 1),
            None,
            "a disabled button must not be hit-testable even though it has geometry"
        );
    }

    #[test]
    fn text_is_not_interactive() {
        let tree = sample();
        let layout = layout(&tree);
        let text = layout.get(NodeKey::first(2)).unwrap();
        assert_eq!(tree.hit_test(&layout, text.x + 1, text.y + 1), None);
    }

    #[test]
    fn a_point_outside_everything_hits_nothing() {
        let tree = sample();
        let layout = layout(&tree);
        assert_eq!(tree.hit_test(&layout, 5_000, 5_000), None);
    }

    #[test]
    fn a_node_without_layout_cannot_be_hit() {
        let tree = sample();
        // A snapshot missing the button entirely: nothing to hit.
        let layout = LayoutSnapshot::from_rects([(NodeKey::first(0), Rect::new(0, 0, 400, 300))]);
        assert_eq!(tree.hit_test(&layout, 10, 10), None);
    }

    #[test]
    fn hit_testing_a_stack_returns_the_last_matching_child() {
        let tree = Tree::new(Node::root(
            0,
            vec![Node::stack(
                1,
                vec![Node::button(2, "back"), Node::button(3, "front")],
            )],
        ));
        let layout = layout(&tree);
        let stack = layout.get(NodeKey::first(1)).unwrap();

        let hit = tree.hit_test(&layout, stack.x + 1, stack.y + 1);
        assert_eq!(
            hit.map(|node| node.key),
            Some(NodeKey::first(3)),
            "the last child paints on top and wins the hit-test"
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
            UiAction::ButtonActivated(NodeKey::first(2)).to_event(),
            WireEvent::Click {
                node: NodeKey::first(2)
            }
        );
    }

    // --- Semantic validation, distinct from wire validation. ---

    #[test]
    fn rejects_duplicate_keys() {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                NodeKey::first(1),
                0,
                None,
                WireLayout::default(),
                1,
            )
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(1),
                0,
                Some("same key"),
                WireLayout::default(),
                0,
            );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::DuplicateKey(NodeKey::first(1)))
        );
    }

    /// Generations do not make one id two nodes: `(7, 0)` and `(7, 1)` in
    /// one snapshot are still the same id twice.
    #[test]
    fn rejects_duplicate_ids_even_at_different_generations() {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                NodeKey::first(1),
                0,
                None,
                WireLayout::default(),
                1,
            )
            .node(
                opcode::NODE_TEXT,
                NodeKey::new(1, 1),
                0,
                Some("same id"),
                WireLayout::default(),
                0,
            );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::DuplicateKey(NodeKey::new(1, 1)))
        );
    }

    #[test]
    fn rejects_a_tree_whose_root_is_not_a_root_node() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_COLUMN,
            NodeKey::first(0),
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
    fn rejects_a_tree_whose_root_is_a_row() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROW,
            NodeKey::first(0),
            0,
            None,
            WireLayout::default(),
            0,
        );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::BadRoot("row"))
        );
    }

    #[test]
    fn rejects_a_tree_whose_root_is_a_stack() {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_STACK,
            NodeKey::first(0),
            0,
            None,
            WireLayout::default(),
            0,
        );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::BadRoot("stack"))
        );
    }

    #[test]
    fn rejects_a_nested_root() {
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
                opcode::NODE_ROOT,
                NodeKey::first(1),
                0,
                None,
                WireLayout::default(),
                0,
            );
        assert_eq!(
            Tree::decode(&encoder.finish()),
            Err(TreeError::NestedRoot(NodeKey::first(1)))
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
    /// The node a press landed on, if any, and what pressed it.
    pressed: Option<Press>,
}

/// What began a press.
///
/// A press has to remember this. With one shared capture slot a pointer
/// release would complete a Space press and a Space release would complete a
/// pointer press — two input paths using one another's state, which produces
/// activations nobody performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressSource {
    Pointer,
    Keyboard,
}

/// A press in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Press {
    pub key: NodeKey,
    pub source: PressSource,
}

impl Interaction {
    pub fn new() -> Self {
        Self::default()
    }

    /// The node currently drawn as pressed, whatever pressed it.
    ///
    /// Presentation does not care about the source: a button held with Space
    /// looks the same as one held with the mouse.
    pub fn pressed(&self) -> Option<NodeKey> {
        self.pressed.map(|press| press.key)
    }

    /// The press including its source, for the release paths that must only
    /// complete their own.
    pub fn press(&self) -> Option<Press> {
        self.pressed
    }

    /// Space went down on the focused control.
    ///
    /// Captures it, exactly as a pointer press captures what it landed on, so
    /// the release completes against *this* node rather than whatever is
    /// focused by then. Returns whether anything changed, which is the
    /// caller's cue to repaint — a repeat while already held changes nothing.
    pub fn on_keyboard_press(&mut self, tree: &Tree, focused: Option<NodeKey>) -> bool {
        let Some(key) = focused else {
            return false;
        };
        if !tree
            .find(key)
            .is_some_and(|node| node.kind.is_interactive())
        {
            return false;
        }
        if self.pressed.map(|press| press.key) == Some(key) {
            return false;
        }
        self.pressed = Some(Press {
            key,
            source: PressSource::Keyboard,
        });
        true
    }

    /// Space came up.
    ///
    /// Activates only when the captured key is still the focused, interactive
    /// one — and only when the capture was a keyboard press, so a Space
    /// release cannot complete a press the mouse started. The capture is
    /// cleared either way, because the key is no longer down.
    pub fn on_keyboard_release(
        &mut self,
        tree: &Tree,
        focused: Option<NodeKey>,
    ) -> Option<UiAction> {
        let press = self.pressed?;
        if press.source != PressSource::Keyboard {
            return None;
        }
        self.pressed = None;
        if focused != Some(press.key) {
            return None;
        }
        tree.find(press.key)
            .filter(|node| node.kind.is_interactive())
            .map(|_| UiAction::ButtonActivated(press.key))
    }

    /// Enter activates outright: there is no held state to show, so there is
    /// nothing to capture and nothing a release could complete.
    pub fn on_enter(&self, tree: &Tree, focused: Option<NodeKey>) -> Option<UiAction> {
        let key = focused?;
        tree.find(key)
            .filter(|node| node.kind.is_interactive())
            .map(|_| UiAction::ButtonActivated(key))
    }

    /// Drops a keyboard press whose captured node is no longer the focused,
    /// interactive one.
    ///
    /// The general form of the rule this stage keeps rediscovering: any
    /// transient interaction naming a `NodeKey` is retired when that key stops
    /// being eligible. Focus moving away, the node being disabled, hidden or
    /// removed, and a commit that replaces its generation all land here.
    pub fn retire_keyboard_press(&mut self, tree: &Tree, focused: Option<NodeKey>) -> bool {
        let Some(press) = self.pressed else {
            return false;
        };
        if press.source != PressSource::Keyboard {
            return false;
        }
        let still_eligible = focused == Some(press.key)
            && tree
                .find(press.key)
                .is_some_and(|node| node.kind.is_interactive())
            && reachable_for_interaction(&tree.root, press.key);
        if still_eligible {
            return false;
        }
        self.pressed = None;
        true
    }

    /// A press landed at `(x, y)`. Records the target; activates nothing.
    ///
    /// Activation deliberately waits for the release, matching every desktop
    /// convention: pressing a button and dragging away must not activate it.
    pub fn on_press(
        &mut self,
        tree: &Tree,
        layout: &LayoutSnapshot,
        scroll: &ScrollState,
        x: i32,
        y: i32,
    ) {
        self.pressed = tree
            .hit_test_scrolled(layout, scroll, x, y)
            .map(|node| Press {
                key: node.key,
                source: PressSource::Pointer,
            });
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
        scroll: &ScrollState,
        x: i32,
        y: i32,
    ) -> Option<UiAction> {
        // Only a pointer press. A pointer release completing a Space press
        // would activate a button the user is still holding the keyboard on.
        let press = self
            .pressed
            .filter(|press| press.source == PressSource::Pointer)?;
        self.pressed = None;
        let released_on = tree.hit_test_scrolled(layout, scroll, x, y)?.key;
        (released_on == press.key).then_some(UiAction::ButtonActivated(press.key))
    }

    /// Abandons any in-progress press.
    ///
    /// The host calls this when geometry is invalidated: the press was
    /// recorded against a layout that no longer exists, so completing it could
    /// activate a node that has since moved elsewhere.
    pub fn cancel(&mut self) {
        self.pressed = None;
    }

    /// Drops any state referring to a node the guest removed.
    ///
    /// # The invariant
    ///
    /// > Any host transient state referencing a removed [`NodeKey`] is retired
    /// > before the new snapshot becomes interactive.
    ///
    /// Without this, a press survives the disappearance of the node it landed
    /// on, and a guest that later reuses the same key gets the press completed
    /// against a control the user never touched:
    ///
    /// ```text
    /// press node 7  ->  guest removes node 7  ->  guest re-adds node 7
    ///               ->  release  ->  the NEW node 7 activates
    /// ```
    ///
    /// Nothing else catches that. `KindChanged` does not fire, because a
    /// button replaced by a button is the same kind; the geometry barrier does
    /// not fire, because the scale never changed.
    ///
    /// As focus, hover, pointer capture, and scroll offsets arrive they retire
    /// here too — this is deliberately the one place that answers "the node is
    /// gone, forget everything about it".
    ///
    /// # What this cannot reach
    ///
    /// A [`UiAction`] that has already been encoded and queued for the guest.
    /// By then it is opaque bytes, but the event carries the node's generation
    /// alongside its id. A guest comparing the delivered key against its own
    /// live keys rejects an activation for a node it has since replaced.
    pub fn retire(&mut self, removed: &[NodeKey]) {
        if self
            .pressed
            .is_some_and(|press| removed.contains(&press.key))
        {
            self.pressed = None;
        }
    }

    /// Drops any state referring to a node the guest made non-interactive.
    ///
    /// # The invariant
    ///
    /// > When a subtree becomes non-interactive through `Display::None` or
    /// > `Visibility::Hidden`, any host transient state referencing that
    /// > subtree is retired before the new state becomes interactive.
    ///
    /// The same class of rule as [`Interaction::retire`], for a different
    /// event. Deletion and hiding are genuinely different — a hidden node is
    /// still in the tree and still live at its generation, so the ledger has
    /// no opinion about it and `removed` never mentions it — but they share
    /// the only thing that matters here:
    ///
    /// ```text
    /// press a button  ->  guest hides it  ->  release
    ///                 ->  a press completes against something the user can
    ///                     neither see nor reach
    /// ```
    ///
    /// This lands in A3 rather than Stage 3 because the alternative is
    /// shipping the hole and waiting for focus, hover, and pointer capture to
    /// each fall into it. They retire here too as they arrive.
    pub fn retire_hidden(&mut self, tree: &Tree) {
        let Some(press) = self.pressed else {
            return;
        };
        // Walking from the root, because suppression is inherited: a node may
        // be perfectly visible itself and sit under a hidden ancestor. Looking
        // the key up directly would miss exactly that case.
        if !reachable_for_interaction(&tree.root, press.key) {
            self.pressed = None;
        }
    }
}

/// Whether `key` names a node that interaction can still reach.
///
/// False when the node is gone, and false when anything on the path to it is
/// `Display::None` or `Visibility::Hidden` — which is what makes suppression
/// subtree-wide rather than per-node.
fn reachable_for_interaction(node: &Node, key: NodeKey) -> bool {
    if !is_presented(node) {
        return false;
    }
    node.key == key
        || node
            .children
            .iter()
            .any(|child| reachable_for_interaction(child, key))
}
