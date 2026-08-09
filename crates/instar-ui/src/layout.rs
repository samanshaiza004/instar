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
//! Six node kinds and four layout properties (see
//! [`instar_ui_protocol::WireLayout`]). No general CSS surface, no arbitrary
//! positioning. Containers are a flex column, a flex row, or one grid cell:
//! `Row` mirrors `Column` on the other axis, and `Stack` overlaps its children
//! in that cell. Grid stays an implementation detail of this module — Taffy
//! can express far more than Instar promises, and exposing that would turn an
//! internal choice into a compatibility obligation.

use std::collections::HashMap;

use instar_ui_protocol::{NodeKey, WireDimension};
use taffy::prelude::*;

use crate::text::{self, ShapedText, TextContext};
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutSnapshot {
    rects: HashMap<NodeKey, Rect>,
    /// The final render artifact for every text/button node, extracted after
    /// Taffy has produced the final geometry.
    pub text: HashMap<NodeKey, ShapedText>,
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

    pub fn text(&self, key: NodeKey) -> Option<&ShapedText> {
        self.text.get(&key)
    }

    /// Builds a snapshot directly. For tests that need specific geometry
    /// without going through a full layout pass.
    pub fn from_rects(rects: impl IntoIterator<Item = (NodeKey, Rect)>) -> Self {
        Self {
            rects: rects.into_iter().collect(),
            text: HashMap::new(),
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

/// Extra space a button reserves around its label, per side. Kept from the
/// placeholder metrics so buttons stay bigger than their labels.
pub const BUTTON_PADDING: f32 = 8.0;

fn text_available(value: AvailableSpace) -> text::Available {
    match value {
        AvailableSpace::Definite(width) => text::Available::Definite(width),
        AvailableSpace::MinContent => text::Available::MinContent,
        AvailableSpace::MaxContent => text::Available::MaxContent,
    }
}

fn dimension(value: WireDimension) -> Dimension {
    match value {
        // Fill is expressed as stretch on the cross axis (see `style`), not as
        // a dimension, so it stays `auto` here. On a row's main axis that
        // makes a Fill-width child content-sized for now: growing into free
        // main-axis space is flex grow, which is not part of this stage's
        // vocabulary.
        WireDimension::Fill | WireDimension::Content => Dimension::auto(),
        WireDimension::Fixed(px) => Dimension::length(f32::from(px)),
    }
}

/// What a node's children should treat as the cross axis.
///
/// Fill is implemented as cross-axis stretch, so the meaning of a child's
/// `Fill` dimension depends on which way its parent lays out. Under a row the
/// cross axis is height, and `Tree::from_wire` refuses `Fill` height, so a row
/// child never reaches STRETCH. A stack has no cross axis: its children keep
/// their natural size rather than being stretched to the cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParentAxis {
    Column,
    Row,
    Stack,
}

fn style(node: &Node, parent: ParentAxis) -> Style {
    let layout = node.layout;
    let fill_cross_axis = match parent {
        ParentAxis::Column => layout.width == WireDimension::Fill,
        ParentAxis::Row => layout.height == WireDimension::Fill,
        ParentAxis::Stack => false,
    };
    let (display, flex_direction, gap) = match &node.kind {
        NodeKind::Stack => (
            Display::Grid,
            FlexDirection::Row,
            Size {
                width: LengthPercentage::length(0.0),
                height: LengthPercentage::length(0.0),
            },
        ),
        NodeKind::Row => (
            Display::Flex,
            FlexDirection::Row,
            // Gap is main-axis space: width for a row, height for a column.
            Size {
                width: LengthPercentage::length(f32::from(layout.gap)),
                height: LengthPercentage::length(0.0),
            },
        ),
        _ => (
            Display::Flex,
            FlexDirection::Column,
            Size {
                width: LengthPercentage::length(0.0),
                height: LengthPercentage::length(f32::from(layout.gap)),
            },
        ),
    };
    Style {
        display,
        flex_direction,
        size: Size {
            width: dimension(layout.width),
            height: dimension(layout.height),
        },
        // Fill takes the parent's cross-axis extent; Content shrinks to fit.
        // Under a row the cross axis is height, and the `FillHeight` ban means
        // a row child never reaches STRETCH. Stack children are never
        // stretched: they overlap at their natural sizes.
        align_self: Some(if fill_cross_axis {
            AlignSelf::STRETCH
        } else {
            AlignSelf::START
        }),
        justify_self: (parent == ParentAxis::Stack).then_some(AlignSelf::START),
        padding: taffy::Rect::length(f32::from(layout.padding)),
        gap,
        grid_template_rows: if matches!(node.kind, NodeKind::Stack) {
            vec![auto()]
        } else {
            Vec::new()
        },
        grid_template_columns: if matches!(node.kind, NodeKind::Stack) {
            vec![auto()]
        } else {
            Vec::new()
        },
        grid_row: if parent == ParentAxis::Stack {
            line(1)
        } else {
            Default::default()
        },
        grid_column: if parent == ParentAxis::Stack {
            line(1)
        } else {
            Default::default()
        },
        ..Default::default()
    }
}

/// Computes geometry for every node in `tree` against `viewport`.
///
/// Deterministic: the same tree and viewport always produce the same snapshot,
/// which is what makes hit-testing reproducible and these tests meaningful.
pub fn compute(text: &mut TextContext, tree: &Tree, viewport: Viewport) -> LayoutSnapshot {
    let mut taffy: TaffyTree<MeasureContext> = TaffyTree::new();
    let mut keys: Vec<(taffy::NodeId, NodeKey)> = Vec::new();

    let root = build(&mut taffy, &tree.root, &mut keys);
    let key_by_id: HashMap<taffy::NodeId, NodeKey> = keys.iter().copied().collect();
    let id_by_key: HashMap<NodeKey, taffy::NodeId> =
        keys.iter().map(|(id, key)| (*key, *id)).collect();

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
        .compute_layout_with_measure(root, available, |known, available, id, context, _style| {
            let measured = match context.as_deref() {
                Some(context) => {
                    let key = key_by_id
                        .get(&id)
                        .copied()
                        .expect("every taffy node has a wire key");
                    let padding = if context.is_button {
                        BUTTON_PADDING * 2.0
                    } else {
                        0.0
                    };
                    let label_available = if context.is_button {
                        match available.width {
                            AvailableSpace::Definite(width) => {
                                text::Available::Definite((width - padding).max(0.0))
                            }
                            other => text_available(other),
                        }
                    } else {
                        text_available(available.width)
                    };
                    let (width, height) = text.measure(
                        key,
                        context.text.as_deref().unwrap_or(""),
                        text::ShapingStyle::default(),
                        label_available,
                    );
                    Size {
                        width: width + padding,
                        height: height + padding,
                    }
                }
                None => Size::ZERO,
            };
            Size {
                width: known.width.unwrap_or(measured.width),
                height: known.height.unwrap_or(measured.height),
            }
        })
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
    let mut shaped = HashMap::new();
    finalize_text(text, tree, &taffy, &id_by_key, &mut shaped);
    LayoutSnapshot {
        rects,
        text: shaped,
    }
}

fn finalize_text(
    text: &mut TextContext,
    tree: &Tree,
    taffy: &TaffyTree<MeasureContext>,
    id_by_key: &HashMap<NodeKey, taffy::NodeId>,
    out: &mut HashMap<NodeKey, ShapedText>,
) {
    for node in tree.iter() {
        let is_text = matches!(node.kind, NodeKind::Text { .. } | NodeKind::Button { .. });
        if !is_text {
            continue;
        }
        let Some(id) = id_by_key.get(&node.key).copied() else {
            continue;
        };
        let Ok(geometry) = taffy.layout(id) else {
            continue;
        };
        let padding = if matches!(node.kind, NodeKind::Button { .. }) {
            BUTTON_PADDING * 2.0
        } else {
            0.0
        };
        let final_width = (geometry.size.width - padding).max(0.0);
        out.insert(node.key, text.finalize(node.key, final_width).clone());
    }
}

fn build(
    taffy: &mut TaffyTree<MeasureContext>,
    node: &Node,
    keys: &mut Vec<(taffy::NodeId, NodeKey)>,
) -> taffy::NodeId {
    // The root's style is replaced wholesale in `compute`; this parent only
    // matters for its children, and the root lays out as a column.
    build_with(taffy, node, keys, ParentAxis::Column)
}

fn build_with(
    taffy: &mut TaffyTree<MeasureContext>,
    node: &Node,
    keys: &mut Vec<(taffy::NodeId, NodeKey)>,
    parent: ParentAxis,
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

    let parent_for_children = match &node.kind {
        NodeKind::Row => ParentAxis::Row,
        NodeKind::Stack => ParentAxis::Stack,
        _ => ParentAxis::Column,
    };
    let id = if node.children.is_empty() {
        match context {
            Some(context) => taffy
                .new_leaf_with_context(style(node, parent), context)
                .expect("taffy leaf"),
            None => taffy.new_leaf(style(node, parent)).expect("taffy leaf"),
        }
    } else {
        let children: Vec<taffy::NodeId> = node
            .children
            .iter()
            .map(|child| build_with(taffy, child, keys, parent_for_children))
            .collect();
        taffy
            .new_with_children(style(node, parent), &children)
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
