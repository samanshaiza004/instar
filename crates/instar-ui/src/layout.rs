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

use instar_ui_protocol::{NodeKey, WireAlign, WireJustify, WireSize};
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

fn dimension(value: WireSize) -> Dimension {
    match value {
        WireSize::Content => Dimension::auto(),
        WireSize::Fixed(px) => Dimension::length(f32::from(px)),
    }
}

fn bound(value: Option<u16>) -> Dimension {
    match value {
        Some(px) => Dimension::length(f32::from(px)),
        None => Dimension::auto(),
    }
}

fn align(value: WireAlign) -> AlignItems {
    match value {
        WireAlign::Start => AlignItems::START,
        WireAlign::Center => AlignItems::CENTER,
        WireAlign::End => AlignItems::END,
        WireAlign::Stretch => AlignItems::STRETCH,
    }
}

fn justify(value: WireJustify) -> JustifyContent {
    match value {
        WireJustify::Start => JustifyContent::START,
        WireJustify::Center => JustifyContent::CENTER,
        WireJustify::End => JustifyContent::END,
        WireJustify::SpaceBetween => JustifyContent::SPACE_BETWEEN,
        WireJustify::SpaceAround => JustifyContent::SPACE_AROUND,
        WireJustify::SpaceEvenly => JustifyContent::SPACE_EVENLY,
    }
}

/// A2 collapsed this from three variants to one question.
///
/// It began as `ParentAxis { Column, Row, Stack }`, because `Fill` meant
/// cross-axis stretch and a child therefore had to know which way its parent
/// laid out before it could know which axis `Fill` referred to. Deleting
/// `Fill` deleted that: a child now states `align_self` directly, and
/// `Row` versus `Column` is read off the *parent's own* kind when styling the
/// parent, never handed down.
///
/// What survives is placement inside a [`NodeKind::Stack`], which is a grid
/// and needs its children pinned to the single cell. That is a yes-or-no
/// question, so it is a bool.
fn style(node: &Node, parent_is_stack: bool) -> Style {
    let layout = node.layout;
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

    // `None` means "inherit the parent's align_items", which is exactly what
    // Taffy does with an absent `align_self` — so absence is passed through
    // rather than resolved here. The one exception is a stack child, which
    // overlaps at its natural size unless it asked for something else;
    // inheriting a stretch there would defeat the point of the stack.
    let align_self = match (layout.align_self, parent_is_stack) {
        (Some(value), _) => Some(align(value)),
        (None, true) => Some(AlignItems::START),
        (None, false) => None,
    };

    Style {
        display,
        flex_direction,
        size: Size {
            width: dimension(layout.width),
            height: dimension(layout.height),
        },
        min_size: Size {
            width: bound(layout.min_width),
            height: bound(layout.min_height),
        },
        max_size: Size {
            width: bound(layout.max_width),
            height: bound(layout.max_height),
        },
        // Finite and in range because `Reader::flex_factor` is the only way
        // one reaches this crate.
        flex_grow: layout.grow,
        flex_shrink: layout.shrink,
        align_self,
        align_items: Some(align(layout.align_items)),
        justify_content: Some(justify(layout.justify_content)),
        justify_self: parent_is_stack.then_some(AlignItems::START),
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
        grid_row: if parent_is_stack {
            line(1)
        } else {
            Default::default()
        },
        grid_column: if parent_is_stack {
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
            // Carried across explicitly, because this style *replaces* the one
            // `style()` built rather than amending it, and an absent
            // `align_items` is not neutral: Taffy follows CSS and treats it as
            // stretch. Every child used to set `align_self` outright, so the
            // omission was invisible; now that a child may say `None` to mean
            // "inherit", inheriting the wrong default silently stretches the
            // entire interface.
            align_items: Some(align(tree.root.layout.align_items)),
            justify_content: Some(justify(tree.root.layout.justify_content)),
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
    // The root's style is replaced wholesale in `compute`, and the root is
    // never a stack child.
    build_with(taffy, node, keys, false)
}

fn build_with(
    taffy: &mut TaffyTree<MeasureContext>,
    node: &Node,
    keys: &mut Vec<(taffy::NodeId, NodeKey)>,
    parent_is_stack: bool,
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

    let children_are_stacked = matches!(node.kind, NodeKind::Stack);
    let id = if node.children.is_empty() {
        match context {
            Some(context) => taffy
                .new_leaf_with_context(style(node, parent_is_stack), context)
                .expect("taffy leaf"),
            None => taffy
                .new_leaf(style(node, parent_is_stack))
                .expect("taffy leaf"),
        }
    } else {
        let children: Vec<taffy::NodeId> = node
            .children
            .iter()
            .map(|child| build_with(taffy, child, keys, children_are_stacked))
            .collect();
        taffy
            .new_with_children(style(node, parent_is_stack), &children)
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
