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

use instar_ui_protocol::{NodeKey, WireAlign, WireBasis, WireDisplay, WireJustify, WireSize};
use taffy::prelude::*;

use crate::scroll::{SCROLLBAR_THICKNESS, ScrollbarStyle};
use crate::text::{self, FontRole, ShapedText, ShapingStyle, TextContext};
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
    /// Only the shaping-affecting half of the node's style reaches here.
    ///
    /// `ShapingStyle` is hashed as the text cache's key, so a field that
    /// cannot change shaping must never enter it: adding a colour would make
    /// every repaint miss the cache and re-shape the tree, and nothing would
    /// fail -- it would just get slow.
    style: ShapingStyle,
}

/// The shaping half of a node's style, and nothing else from it.
fn shaping_style(node: &Node) -> ShapingStyle {
    ShapingStyle {
        role: match node.style.text.role {
            instar_ui_protocol::WireFontRole::SystemUi => FontRole::SystemUi,
            instar_ui_protocol::WireFontRole::Monospace => FontRole::Monospace,
        },
        size: f32::from(node.style.text.size),
        weight: node.style.text.weight,
        ..ShapingStyle::default()
    }
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

/// What a parent imposes on its children beyond ordinary flex layout.
///
/// A2 deleted the previous `ParentAxis { Column, Row, Stack }`, which existed
/// only because `Fill` meant cross-axis stretch and a child had to know its
/// parent's direction to know which axis that was. Direction is no longer
/// handed down, and `Row` versus `Column` is read off the parent's own kind
/// when styling the parent.
///
/// What is left is genuinely about the parent: a `Stack` is a grid and pins
/// its children to one cell, and a `Scroll` is a viewport whose content must
/// keep its natural size. Both are things a child cannot work out for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildOf {
    Normal,
    /// A [`NodeKind::Stack`]: one grid cell, natural size, no stretch.
    StackCell,
    /// A [`NodeKind::Scroll`]: the content of a viewport.
    ScrollContent,
}

fn style(node: &Node, parent: ChildOf, scrollbars: ScrollbarStyle) -> Style {
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
        // A viewport lays its one content child out as a column, so the child
        // takes its natural height rather than being squeezed to the
        // viewport's -- the overflow is the whole point.
        NodeKind::Scroll => (
            Display::Flex,
            FlexDirection::Column,
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
    let align_self = match (layout.align_self, parent) {
        (Some(value), _) => Some(align(value)),
        (None, ChildOf::StackCell) => Some(AlignItems::START),
        (None, _) => None,
    };

    // A viewport's content keeps the size it asked for. The default shrink of
    // 1.0 would let Taffy compress it to the viewport, and content that has
    // been squeezed to fit is content that never overflows -- which would make
    // the scrollable extent permanently zero and the whole node pointless.
    let flex_shrink = match parent {
        ChildOf::ScrollContent => 0.0,
        _ => layout.shrink,
    };

    Style {
        display,
        flex_direction,
        overflow: if matches!(node.kind, NodeKind::Scroll) {
            taffy::Point {
                x: taffy::Overflow::Scroll,
                y: taffy::Overflow::Scroll,
            }
        } else {
            taffy::Point::default()
        },
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
        flex_basis: match layout.basis {
            // Taffy's own default, which is `auto`: derive the starting size
            // from `size` or from content, exactly as before this field
            // existed.
            WireBasis::Auto => Dimension::auto(),
            WireBasis::Fixed(length) => Dimension::length(f32::from(length)),
        },
        flex_grow: layout.grow,
        flex_shrink,
        align_self,
        align_items: Some(align(layout.align_items)),
        justify_content: Some(justify(layout.justify_content)),
        justify_self: (parent == ChildOf::StackCell).then_some(AlignItems::START),
        padding: {
            let mut padding = taffy::Rect::length(f32::from(layout.padding));
            // `Inset` gives the bar its own strip by narrowing the content
            // rectangle. The viewport rect itself is untouched, so
            // `Scrollbar::for_viewport` still puts the bar on the viewport's
            // edge -- it now lands on reserved space rather than over content.
            // One place decides this, and hit testing and painting both read
            // the same geometry afterwards.
            if matches!(node.kind, NodeKind::Scroll) && scrollbars == ScrollbarStyle::Inset {
                padding.right = LengthPercentage::length(
                    f32::from(layout.padding) + SCROLLBAR_THICKNESS as f32,
                );
            }
            padding
        },
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
        grid_row: if parent == ChildOf::StackCell {
            line(1)
        } else {
            Default::default()
        },
        grid_column: if parent == ChildOf::StackCell {
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
pub fn compute(
    text: &mut TextContext,
    tree: &Tree,
    viewport: Viewport,
    scrollbars: ScrollbarStyle,
) -> LayoutSnapshot {
    let mut taffy: TaffyTree<MeasureContext> = TaffyTree::new();
    let mut keys: Vec<(taffy::NodeId, NodeKey)> = Vec::new();

    let root = build(&mut taffy, &tree.root, &mut keys, scrollbars);
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
                        context.style,
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
    accumulate(&taffy, root, 0.0, 0.0, &key_by_id, &mut rects);
    let mut shaped = HashMap::new();
    finalize_text(text, tree, &taffy, &id_by_key, &mut shaped);
    LayoutSnapshot {
        rects,
        text: shaped,
    }
}

/// Re-finalizes text against geometry that has not moved.
///
/// The alignment-only path. Every rectangle in `snapshot` is still correct, so
/// there is nothing for Taffy to do; what has changed is where glyphs sit
/// inside boxes that already exist. Reuses each node's recorded width, so the
/// cached line break survives and only `Layout::align` and the extraction run.
pub fn refinalize_text(text: &mut TextContext, tree: &Tree, snapshot: &mut LayoutSnapshot) {
    for node in tree.iter() {
        if !matches!(node.kind, NodeKind::Text { .. } | NodeKind::Button { .. }) {
            continue;
        }
        let Some(rect) = snapshot.get(node.key) else {
            continue;
        };
        let padding = if matches!(node.kind, NodeKind::Button { .. }) {
            BUTTON_PADDING * 2.0
        } else {
            0.0
        };
        let final_width = (rect.width as f32 - padding).max(0.0);
        snapshot.text.insert(
            node.key,
            text.finalize(node.key, final_width, node.style.text_layout.align.into())
                .clone(),
        );
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
        out.insert(
            node.key,
            text.finalize(node.key, final_width, node.style.text_layout.align.into())
                .clone(),
        );
    }
}

fn build(
    taffy: &mut TaffyTree<MeasureContext>,
    node: &Node,
    keys: &mut Vec<(taffy::NodeId, NodeKey)>,
    scrollbars: ScrollbarStyle,
) -> taffy::NodeId {
    // The root's style is replaced wholesale in `compute`, and the root is
    // never anyone's special child.
    build_with(taffy, node, keys, ChildOf::Normal, scrollbars)
}

fn build_with(
    taffy: &mut TaffyTree<MeasureContext>,
    node: &Node,
    keys: &mut Vec<(taffy::NodeId, NodeKey)>,
    parent: ChildOf,
    scrollbars: ScrollbarStyle,
) -> taffy::NodeId {
    let context = match &node.kind {
        NodeKind::Text { text } => Some(MeasureContext {
            text: Some(text.clone()),
            is_button: false,
            style: shaping_style(node),
        }),
        NodeKind::Button { label, .. } => Some(MeasureContext {
            text: Some(label.clone()),
            is_button: true,
            style: shaping_style(node),
        }),
        _ => None,
    };

    let child_context = match node.kind {
        NodeKind::Stack => ChildOf::StackCell,
        NodeKind::Scroll => ChildOf::ScrollContent,
        _ => ChildOf::Normal,
    };
    // `Display::None` is absent from layout, and so is everything under it.
    // Nothing is built rather than building it and hiding it: a node Taffy
    // never sees produces no rect, and a key with no rect is already
    // unhittable and unpaintable everywhere downstream. The alternative --
    // Taffy's own `Display::None` -- would leave the subtree in the tree with
    // zero-sized rects, which is a different and much easier thing to
    // accidentally hit.
    let children: Vec<&Node> = node
        .children
        .iter()
        .filter(|child| child.layout.display != WireDisplay::None)
        .collect();

    let id = if children.is_empty() {
        match context {
            Some(context) => taffy
                .new_leaf_with_context(style(node, parent, scrollbars), context)
                .expect("taffy leaf"),
            None => taffy
                .new_leaf(style(node, parent, scrollbars))
                .expect("taffy leaf"),
        }
    } else {
        let ids: Vec<taffy::NodeId> = children
            .iter()
            .map(|child| build_with(taffy, child, keys, child_context, scrollbars))
            .collect();
        taffy
            .new_with_children(style(node, parent, scrollbars), &ids)
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
    key_by_id: &HashMap<taffy::NodeId, NodeKey>,
    out: &mut HashMap<NodeKey, Rect>,
) {
    let Ok(layout) = taffy.layout(id) else {
        return;
    };
    let x = parent_x + layout.location.x;
    let y = parent_y + layout.location.y;

    if let Some(key) = key_by_id.get(&id) {
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
        accumulate(taffy, child, x, y, key_by_id, out);
    }
}

#[cfg(test)]
mod intrinsic_sizing {
    use super::*;
    use crate::{Node, TextContext, Tree};

    /// A viewport can be bounded by its parent instead of by a literal.
    ///
    /// A `Scroll` is a flex item like any other, and its automatic minimum
    /// size is what decides whether `grow` and `shrink` can reach it. With
    /// Taffy's default (visible) overflow that minimum is the content's own
    /// size, so a viewport with tall content could never be squeezed to its
    /// parent -- it sized to the content and the surrounding layout broke
    /// around it. The only way to bound one was a fixed height, which does not
    /// follow a resized window.
    ///
    /// Declaring the overflow is what CSS does for the same reason: a scroll
    /// container's automatic minimum size is zero, because clipping is the
    /// entire point of it.
    #[test]
    fn a_viewport_fills_the_space_it_is_given_and_its_content_does_not() {
        use crate::{WireAlign, WireLayout, WireSize};
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::text(3, "header").with_layout(WireLayout {
                    height: WireSize::Fixed(20),
                    ..WireLayout::default()
                }),
                Node::scroll(
                    1,
                    Node::column(
                        2,
                        vec![Node::text(6, "tall").with_layout(WireLayout {
                            height: WireSize::Fixed(600),
                            ..WireLayout::default()
                        })],
                    ),
                )
                .with_layout(WireLayout {
                    grow: 1.0,
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        ));
        let mut text = TextContext::new();
        let snapshot = compute(
            &mut text,
            &tree,
            Viewport::new(480.0, 320.0),
            ScrollbarStyle::Overlay,
        );

        let header = snapshot.get(NodeKey::first(3)).expect("the header");
        let viewport = snapshot.get(NodeKey::first(1)).expect("the scroll");
        assert_eq!(
            header.height + viewport.height,
            320,
            "the viewport takes exactly what the header left of the window, \
             rather than sizing to its 600pt content"
        );
        assert!(
            viewport.height < 600,
            "and is therefore smaller than its content, or there is nothing \
             to scroll: got {}",
            viewport.height
        );
        assert_eq!(
            snapshot.get(NodeKey::first(2)).expect("its content").height,
            600,
            "and the content keeps its own height -- squeezing it to the \
             viewport would leave nothing to scroll"
        );
    }

    /// Two keys with different labels and the same basis end up the same
    /// width.
    ///
    /// The Calculator's actual requirement, and the reason `basis` exists:
    /// `grow` distributes *free space*, computed from a starting size, so with
    /// `grow` alone a key labelled `0` is narrower than one labelled `00`.
    ///
    /// Asserts the final rectangles, never the style values. Whether the
    /// automatic content-based minimum also has to be lifted is exactly the
    /// kind of thing a style-level assertion would fail to notice.
    #[test]
    fn siblings_sharing_a_basis_end_up_the_same_width() {
        use crate::{WireBasis, WireLayout};
        let equal_share = WireLayout {
            basis: WireBasis::Fixed(0),
            grow: 1.0,
            // Load-bearing, and measured rather than assumed. CSS gives a
            // flex item an automatic content-based minimum in the main axis,
            // and Taffy honours it: at a row width where the equal share is
            // narrower than the longest label, `basis: 0` alone gives 25 and
            // 117 rather than 40 and 40.
            //
            // Two statements, deliberately not one. `basis: 0` says where
            // distribution starts; `min_width: 0` says it may go past content.
            // Folding the second into the first would let a sizing field
            // silently override a constraint field.
            min_width: Some(0),
            ..WireLayout::default()
        };
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::row(
                    1,
                    vec![
                        Node::button(2, "0").with_layout(equal_share),
                        Node::button(3, "00").with_layout(equal_share),
                        Node::button(4, "000000000000").with_layout(equal_share),
                    ],
                )
                // Narrow on purpose. At 300 the equal share is wider than
                // every label, the content minimum never binds, and the test
                // passes with `min_width` removed -- proving nothing about
                // the constraint it is here to justify.
                .with_layout(WireLayout {
                    width: WireSize::Fixed(120),
                    ..WireLayout::default()
                }),
            ],
        ));
        let mut text = TextContext::new();
        let snapshot = compute(
            &mut text,
            &tree,
            Viewport::new(400.0, 300.0),
            ScrollbarStyle::Overlay,
        );

        let width = |id: u32| snapshot.get(NodeKey::first(id)).expect("laid out").width;
        assert_eq!(
            (width(2), width(3), width(4)),
            (40, 40, 40),
            "an equal share is an equal share whatever the label says, even \
             when the share is narrower than the label"
        );
    }

    /// `Auto` and `Fixed(0)` are different things, not two spellings of one.
    #[test]
    fn an_auto_basis_still_starts_from_content() {
        use crate::{WireBasis, WireLayout};
        let from_content = WireLayout {
            basis: WireBasis::Auto,
            grow: 1.0,
            ..WireLayout::default()
        };
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::row(
                    1,
                    vec![
                        Node::button(2, "0").with_layout(from_content),
                        Node::button(3, "000000").with_layout(from_content),
                    ],
                )
                .with_layout(WireLayout {
                    width: WireSize::Fixed(300),
                    ..WireLayout::default()
                }),
            ],
        ));
        let mut text = TextContext::new();
        let snapshot = compute(
            &mut text,
            &tree,
            Viewport::new(400.0, 300.0),
            ScrollbarStyle::Overlay,
        );
        let width = |id: u32| snapshot.get(NodeKey::first(id)).expect("laid out").width;
        assert!(
            width(3) > width(2),
            "Auto derives the starting size from content, so a longer label \
             stays wider: {} against {}",
            width(3),
            width(2)
        );
    }

    /// A basis is a starting size, and the difference between two of them
    /// survives an equal distribution of what is left.
    #[test]
    fn different_bases_keep_their_difference() {
        use crate::{WireBasis, WireLayout};
        let with_basis = |length: u16| WireLayout {
            basis: WireBasis::Fixed(length),
            grow: 1.0,
            min_width: Some(0),
            ..WireLayout::default()
        };
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::row(
                    1,
                    vec![
                        Node::button(2, "a").with_layout(with_basis(20)),
                        Node::button(3, "b").with_layout(with_basis(40)),
                    ],
                )
                .with_layout(WireLayout {
                    width: WireSize::Fixed(200),
                    ..WireLayout::default()
                }),
            ],
        ));
        let mut text = TextContext::new();
        let snapshot = compute(
            &mut text,
            &tree,
            Viewport::new(400.0, 300.0),
            ScrollbarStyle::Overlay,
        );
        let width = |id: u32| snapshot.get(NodeKey::first(id)).expect("laid out").width;
        assert_eq!(
            width(3) - width(2),
            20,
            "equal grow shares the surplus equally, so the 20px head start is \
             exactly what remains between them"
        );
    }

    /// `Inset` reserves the bar its own strip; `Overlay` does not.
    ///
    /// The Gallery's nested experiment showed that styling can make a nested
    /// viewport obviously distinct, and that two overlay bars are *still*
    /// indistinguishable once it is. So the fix is not viewport chrome, it is
    /// where the bar lives.
    #[test]
    fn inset_narrows_the_content_rectangle_and_overlay_does_not() {
        use crate::{ScrollbarStyle, WireAlign, WireLayout, WireSize};
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    10,
                    Node::column(
                        11,
                        vec![Node::text(12, "tall").with_layout(WireLayout {
                            height: WireSize::Fixed(600),
                            ..WireLayout::default()
                        })],
                    )
                    .with_layout(WireLayout {
                        align_self: Some(WireAlign::Stretch),
                        ..WireLayout::default()
                    }),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(100),
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        ));

        let mut text = TextContext::new();
        let overlay = tree.layout_with(
            &mut text,
            Viewport::new(400.0, 300.0),
            ScrollbarStyle::Overlay,
        );
        let inset = tree.layout_with(
            &mut text,
            Viewport::new(400.0, 300.0),
            ScrollbarStyle::Inset,
        );

        let viewport = NodeKey::first(10);
        let content = NodeKey::first(11);

        assert_eq!(
            overlay.get(viewport).unwrap(),
            inset.get(viewport).unwrap(),
            "the viewport itself is the same rectangle under both -- the bar \
             sits on its edge either way, and only the room left for content \
             differs"
        );
        assert_eq!(
            overlay.get(content).unwrap().width,
            400,
            "overlay lets content have the whole width, and paints over it"
        );
        assert_eq!(
            inset.get(content).unwrap().width,
            400 - SCROLLBAR_THICKNESS,
            "inset gives the bar its own strip, so nothing is hidden beneath \
             chrome"
        );
    }

    /// The invariant that survives the policy: a bar is on the edge of the
    /// viewport it scrolls, at any nesting depth.
    ///
    /// `Inset` changes how wide the usable content rectangle is. It must never
    /// shove a nested bar sideways because an outer bar happens to exist, and
    /// nesting depth must not enter the geometry at all — that would leave no
    /// obvious answer at three levels and stop a bar corresponding to the
    /// thing it scrolls.
    #[test]
    fn a_nested_bar_stays_on_its_own_viewports_edge_under_both_policies() {
        use crate::{ScrollbarStyle, WireAlign, WireLayout, WireSize};
        let bounded = |height: u16| WireLayout {
            height: WireSize::Fixed(height),
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let tall = |id: u32, height: u16| {
            Node::text(id, "tall").with_layout(WireLayout {
                height: WireSize::Fixed(height),
                ..WireLayout::default()
            })
        };
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    10,
                    Node::column(
                        11,
                        vec![
                            Node::scroll(20, Node::column(21, vec![tall(22, 400)]))
                                .with_layout(bounded(80)),
                            tall(13, 600),
                        ],
                    )
                    .with_layout(WireLayout {
                        align_self: Some(WireAlign::Stretch),
                        ..WireLayout::default()
                    }),
                )
                .with_layout(bounded(150)),
            ],
        ));

        let mut text = TextContext::new();
        for scrollbars in [ScrollbarStyle::Overlay, ScrollbarStyle::Inset] {
            let snapshot = tree.layout_with(&mut text, Viewport::new(400.0, 300.0), scrollbars);
            for (viewport, content_height) in [(NodeKey::first(10), 600), (NodeKey::first(20), 400)]
            {
                let rect = snapshot.get(viewport).expect("a viewport");
                let bar = crate::Scrollbar::for_viewport(rect, content_height, 0)
                    .expect("both viewports overflow");
                assert_eq!(
                    bar.track.x + bar.track.width,
                    rect.x + rect.width,
                    "{scrollbars:?}: {viewport:?}'s bar must sit on its own \
                     right edge, whatever encloses it"
                );
            }
        }
    }

    /// A node sized from its own text is never narrower than that text.
    ///
    /// Taffy rounds computed layout to integers, so a box sized from a
    /// fractional measurement lands a fraction of a pixel short -- and the
    /// finalize pass then re-breaks the label to that rounded width. The label
    /// wrapped or did not according to which way its fraction fell, which is
    /// why it stayed hidden: the counter guest's strings happened to round up.
    ///
    /// Several labels, deliberately, because a single string proves only that
    /// one fraction fell the right way.
    #[test]
    fn a_label_given_all_the_room_it_asked_for_does_not_wrap() {
        let labels = [
            "Ordinary button",
            "Scroll past this",
            "Nothing pressed yet",
            "Offscreen button",
            "Unavailable",
            "Click me",
            "Crash on purpose",
        ];
        let children: Vec<Node> = labels
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let id = i as u32 + 1;
                if i % 2 == 0 {
                    Node::button(id, *label)
                } else {
                    Node::text(id, *label)
                }
            })
            .collect();

        let tree = Tree::new(Node::root(0, children));
        let mut text = TextContext::new();
        // Far more room than any of these needs, so a wrap can only come from
        // the node being sized short of its own measurement.
        let snapshot = compute(
            &mut text,
            &tree,
            Viewport::new(2000.0, 2000.0),
            ScrollbarStyle::Overlay,
        );

        for (i, label) in labels.iter().enumerate() {
            let key = NodeKey::first(i as u32 + 1);
            assert!(
                snapshot.get(key).is_some(),
                "{label} should have been laid out"
            );
            assert_eq!(
                text.line_count(key),
                1,
                "{label:?} wrapped despite having room -- its box was rounded \
                 below the width it was measured at"
            );
        }
    }
}
