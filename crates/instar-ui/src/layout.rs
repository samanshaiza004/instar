//! Host-computed layout: retained tree in, [`LayoutSnapshot`] out.
//!
//! Taffy does the flexbox arithmetic; this module owns the translation on both
//! sides of it. That boundary is deliberate and tight:
//!
//! > **Taffy is an implementation detail of this module.** No Taffy type,
//! > `NodeId`, or tree handle appears in this crate's public API, and a
//! > [`LayoutSnapshot`] is an `instar-ui` product rather than protocol state.
//!
//! Keeping that true means a layout engine swap is a change to one file, and
//! it means nothing above `instar-ui` can accidentally start depending on
//! flexbox semantics that Instar has not committed to.
//!
//! # The vocabulary is deliberately tiny
//!
//! Four node kinds and four layout properties (see
//! [`instar_ui_protocol::WireLayout`]). No grid, no general CSS surface, no
//! arbitrary positioning. Everything here is a flex column, because that is
//! the entire Phase 1 commitment — Taffy can express far more, and exposing
//! that would turn an internal choice into a compatibility obligation.

use std::collections::HashMap;

use instar_ui_protocol::{NodeKey, WireDimension};
use taffy::prelude::*;

use crate::{Node, NodeKind, Tree};

/// The logical-pixel viewport layout is computed against.
///
/// Logical, never physical: `instar-ui` never sees a scale factor. The host
/// converts, per docs/PHASE-1.md's DPI split.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle in logical pixels.
///
/// Integers because hit-testing compares them and reproducibility matters more
/// here than sub-pixel precision; Taffy's rounded output is what feeds these.
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
}

/// Where every node ended up.
///
/// Produced by [`compute`], consumed by hit-testing and (later) painting. Not
/// protocol state: a guest never sends one and never receives one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutSnapshot {
    rects: HashMap<NodeKey, Rect>,
}

impl LayoutSnapshot {
    pub fn get(&self, key: NodeKey) -> Option<Rect> {
        self.rects.get(&key).copied()
    }

    pub fn len(&self) -> usize {
        self.rects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn keys(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.rects.keys().copied()
    }

    /// Builds a snapshot directly. For tests that need specific geometry
    /// without going through a full layout pass.
    pub fn from_rects(rects: impl IntoIterator<Item = (NodeKey, Rect)>) -> Self {
        Self {
            rects: rects.into_iter().collect(),
        }
    }
}

/// What a node needs in order to be measured. Taffy hands this back during the
/// measure pass.
#[derive(Debug, Clone)]
struct MeasureContext {
    text: Option<String>,
    is_button: bool,
}

/// Placeholder text metrics.
///
/// Real shaping belongs to the text renderer, which does not exist yet. These
/// constants exist so layout is *deterministic and testable* now, and are the
/// one thing in this module expected to be replaced rather than extended:
/// when a real font stack lands, `measure` takes a font context and these go
/// away. Assertions in tests are written against relative geometry (ordering,
/// containment, stacking) rather than exact pixel values, so that replacement
/// does not invalidate them.
const CHAR_WIDTH: f32 = 8.0;
const LINE_HEIGHT: f32 = 16.0;
/// Extra space a button reserves around its label.
const BUTTON_PADDING: f32 = 8.0;

fn measure(context: &MeasureContext) -> Size<f32> {
    let Some(text) = context.text.as_deref() else {
        return Size::ZERO;
    };
    // `chars()` rather than `len()`: a byte count would make non-ASCII labels
    // measure absurdly wide. Still not correct shaping -- see above.
    let width = text.chars().count() as f32 * CHAR_WIDTH;
    let padding = if context.is_button {
        BUTTON_PADDING * 2.0
    } else {
        0.0
    };
    Size {
        width: width + padding,
        height: LINE_HEIGHT + padding,
    }
}

fn dimension(value: WireDimension) -> Dimension {
    match value {
        // Fill is expressed as stretch on the cross axis (see `style`), not as
        // a dimension, so it stays `auto` here.
        WireDimension::Fill | WireDimension::Content => Dimension::auto(),
        WireDimension::Fixed(px) => Dimension::length(f32::from(px)),
    }
}

fn style(node: &Node) -> Style {
    let layout = node.layout;
    Style {
        display: Display::Flex,
        // Everything is a column. That is the whole Phase 1 layout model.
        flex_direction: FlexDirection::Column,
        size: Size {
            width: dimension(layout.width),
            height: dimension(layout.height),
        },
        // Fill takes the parent's cross-axis extent; Content shrinks to fit.
        align_self: Some(match layout.width {
            WireDimension::Fill => AlignSelf::STRETCH,
            _ => AlignSelf::START,
        }),
        padding: taffy::Rect::length(f32::from(layout.padding)),
        gap: Size {
            width: LengthPercentage::length(0.0),
            height: LengthPercentage::length(f32::from(layout.gap)),
        },
        ..Default::default()
    }
}

/// Computes geometry for every node in `tree` against `viewport`.
///
/// Deterministic: the same tree and viewport always produce the same snapshot,
/// which is what makes hit-testing reproducible and these tests meaningful.
pub fn compute(tree: &Tree, viewport: Viewport) -> LayoutSnapshot {
    let mut taffy: TaffyTree<MeasureContext> = TaffyTree::new();
    let mut keys: Vec<(taffy::NodeId, NodeKey)> = Vec::new();

    let root = build(&mut taffy, &tree.root, &mut keys);

    // The root is given the viewport exactly; a guest cannot size the window.
    let _ = taffy.set_style(
        root,
        Style {
            display: Display::Flex,
            flex_direction: FlexDirection::Column,
            size: Size {
                width: Dimension::length(viewport.width),
                height: Dimension::length(viewport.height),
            },
            padding: taffy::Rect::length(f32::from(tree.root.layout.padding)),
            gap: Size {
                width: LengthPercentage::length(0.0),
                height: LengthPercentage::length(f32::from(tree.root.layout.gap)),
            },
            ..Default::default()
        },
    );

    let available = Size {
        width: AvailableSpace::Definite(viewport.width),
        height: AvailableSpace::Definite(viewport.height),
    };

    if taffy
        .compute_layout_with_measure(
            root,
            available,
            |known, _available, _id, context, _style| {
                // A node with both dimensions already known needs no measuring.
                if let (Some(width), Some(height)) = (known.width, known.height) {
                    return Size { width, height };
                }
                let measured = context.as_deref().map(measure).unwrap_or(Size::ZERO);
                Size {
                    width: known.width.unwrap_or(measured.width),
                    height: known.height.unwrap_or(measured.height),
                }
            },
        )
        .is_err()
    {
        // Taffy fails only on a malformed tree, which `Tree` construction
        // already rules out. An empty snapshot means nothing is hit-testable,
        // which is the safe direction to fail in.
        return LayoutSnapshot::default();
    }

    // Translate to absolute coordinates. Taffy reports each node's position
    // relative to its parent, and every consumer here wants absolute.
    let mut rects = HashMap::with_capacity(keys.len());
    accumulate(&taffy, root, 0.0, 0.0, &keys, &mut rects);
    LayoutSnapshot { rects }
}

fn build(
    taffy: &mut TaffyTree<MeasureContext>,
    node: &Node,
    keys: &mut Vec<(taffy::NodeId, NodeKey)>,
) -> taffy::NodeId {
    let context = match &node.kind {
        NodeKind::Text { text } => Some(MeasureContext {
            text: Some(text.clone()),
            is_button: false,
        }),
        NodeKind::Button { label, .. } => Some(MeasureContext {
            text: Some(label.clone()),
            is_button: true,
        }),
        _ => None,
    };

    let id = if node.children.is_empty() {
        match context {
            Some(context) => taffy
                .new_leaf_with_context(style(node), context)
                .expect("taffy leaf"),
            None => taffy.new_leaf(style(node)).expect("taffy leaf"),
        }
    } else {
        let children: Vec<taffy::NodeId> = node
            .children
            .iter()
            .map(|child| build(taffy, child, keys))
            .collect();
        taffy
            .new_with_children(style(node), &children)
            .expect("taffy branch")
    };

    keys.push((id, node.key));
    id
}

fn accumulate(
    taffy: &TaffyTree<MeasureContext>,
    id: taffy::NodeId,
    parent_x: f32,
    parent_y: f32,
    keys: &[(taffy::NodeId, NodeKey)],
    out: &mut HashMap<NodeKey, Rect>,
) {
    let Ok(layout) = taffy.layout(id) else {
        return;
    };
    let x = parent_x + layout.location.x;
    let y = parent_y + layout.location.y;

    if let Some((_, key)) = keys.iter().find(|(candidate, _)| *candidate == id) {
        out.insert(
            *key,
            Rect::new(
                x.round() as i32,
                y.round() as i32,
                layout.size.width.round() as i32,
                layout.size.height.round() as i32,
            ),
        );
    }

    for child in taffy.children(id).unwrap_or_default() {
        accumulate(taffy, child, x, y, keys, out);
    }
}
