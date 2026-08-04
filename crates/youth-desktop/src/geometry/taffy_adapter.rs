//! A Taffy-backed implementation of [`crate::geometry::layout`], built
//! alongside the existing hand-rolled engine so the two can be diagnosed
//! against each other (Gate L). See the plan at
//! `~/.claude/plans/utilize-codex-orchestration-for-implemen-purrfect-wilkes.md`
//! for the full design rationale -- in short: Root becomes a genuine Taffy
//! flex node (not a hand-written sequential loop), every load-bearing Taffy
//! default that diverges from today's engine is set explicitly, and
//! byte-exact float parity with the old engine is not the acceptance bar
//! (Taffy uses `f32`, the old engine `f64`, and real CSS flexbox resolves
//! negative/positive free space with a different per-item order).

use std::collections::HashMap;

use taffy::prelude::{
    AlignItems, AvailableSpace, BoxSizing, Display, FlexDirection, Rect, Size, Style, length,
};
use taffy::{NodeId as TaffyNodeId, Point, TaffyTree};

use youth_tree::{BoxLayout, NodeData, NodeId, Tree};

use crate::geometry::{
    BOX_PADDING, BUTTON_HORIZONTAL_PADDING, BUTTON_MIN_HEIGHT, BUTTON_MIN_WIDTH,
    BUTTON_VERTICAL_PADDING, CHILD_GAP, GLYPH_HEIGHT, GLYPH_WIDTH, GeometryError, InteractionKind,
    LayoutNode, LayoutSnapshot, LogicalRect, LogicalSize, OUTER_MARGIN,
};

/// A Taffy-backed alternative to [`crate::geometry::layout`], sharing its
/// exact signature and output contract (`LayoutSnapshot`/`LayoutNode`/
/// `hit_test()`'s public shape) but built entirely on `taffy::TaffyTree`
/// rather than the hand-rolled `measure()`/`place()` pair. Not wired into
/// any production call site yet -- see the diagnostic harness in this
/// module's tests, and `geometry.rs`'s own test module, for how the two
/// engines are compared.
pub(crate) fn layout_taffy(
    tree: &Tree,
    viewport: LogicalSize,
) -> Result<LayoutSnapshot, GeometryError> {
    let mut taffy: TaffyTree<NodeId> = TaffyTree::new();
    taffy.disable_rounding();

    let root_id = tree.root();
    let root_node = tree.node(root_id).ok_or(GeometryError::MissingNode)?;

    let mut forward: HashMap<NodeId, TaffyNodeId> = HashMap::with_capacity(tree.node_count());
    let root_taffy_id = build_root(tree, &mut taffy, root_node, viewport, &mut forward)?;

    taffy
        .compute_layout_with_measure(
            root_taffy_id,
            Size {
                width: AvailableSpace::Definite(viewport.width as f32),
                height: AvailableSpace::Definite(viewport.height as f32),
            },
            |known_dimensions, _available_space, _taffy_id, context, _style| {
                let Some(&mut youth_id) = context else {
                    return Size::ZERO;
                };
                let Some(node) = tree.node(youth_id) else {
                    return Size::ZERO;
                };
                let (intrinsic_width, intrinsic_height) = leaf_intrinsic_size(&node.data);
                Size {
                    width: known_dimensions.width.unwrap_or(intrinsic_width as f32),
                    height: known_dimensions.height.unwrap_or(intrinsic_height as f32),
                }
            },
        )
        .map_err(|_| GeometryError::MissingNode)?;

    let mut snapshot = LayoutSnapshot {
        tree_revision: tree.revision(),
        viewport,
        nodes: std::collections::BTreeMap::new(),
        hit_order: Vec::with_capacity(tree.node_count()),
    };
    snapshot.nodes.insert(
        root_id,
        LayoutNode {
            bounds: LogicalRect {
                x: 0.0,
                y: 0.0,
                width: viewport.width,
                height: viewport.height,
            },
            effective_enabled: true,
            interaction: InteractionKind::None,
        },
    );
    snapshot.hit_order.push(root_id);
    for &child in &root_node.children {
        extract_subtree(
            tree,
            &forward,
            &taffy,
            child,
            (0.0, 0.0),
            true,
            &mut snapshot,
        )?;
    }
    Ok(snapshot)
}

/// Root's own Taffy node. A single-child Root (the only shape any real SDK
/// tree emits) unconditionally forces its child to fill it, regardless of
/// that child's own `grow` value -- there is no competition to weight, a
/// lone child obviously *is* the app body. A Root with zero or 2+ children
/// (only ever exercised by `youth-tree`'s own validation fixtures) becomes
/// an ordinary Column: intrinsic-sized, stacked children, growing only via
/// `node.grow` -- which validation already forbids directly under Root, so
/// this degenerates to plain stacking, unchanged from today.
fn build_root(
    tree: &Tree,
    taffy: &mut TaffyTree<NodeId>,
    root_node: &youth_tree::Node,
    viewport: LogicalSize,
    forward: &mut HashMap<NodeId, TaffyNodeId>,
) -> Result<TaffyNodeId, GeometryError> {
    let size = Size {
        width: length(viewport.width as f32),
        height: length(viewport.height as f32),
    };
    // Root's own padding is OUTER_MARGIN, not BOX_PADDING -- distinct from
    // every other container, which is why this doesn't just reuse
    // `container_style()` unmodified. Root also needs `BorderBox` (Taffy's
    // own default, overriding `container_style()`'s `ContentBox`): Root is
    // the one node with an *explicit* declared size (the viewport itself),
    // and under `ContentBox` an explicit size is interpreted as the
    // content area with padding added on top -- expanding the border box
    // past the viewport and handing children the full declared size as
    // available space instead of `viewport - 2*OUTER_MARGIN`. Every other
    // container is auto-sized (no declared `size`), where content-vs-border
    // box makes no difference, so this override is Root-specific.
    let padding = uniform_padding(OUTER_MARGIN);
    if let [single_child] = root_node.children.as_slice() {
        let child_taffy_id = build_node(tree, taffy, *single_child, forward)?;
        let mut child_style = taffy
            .style(child_taffy_id)
            .map_err(|_| GeometryError::MissingNode)?
            .clone();
        child_style.flex_grow = 1.0;
        child_style.align_self = Some(AlignItems::STRETCH);
        taffy
            .set_style(child_taffy_id, child_style)
            .map_err(|_| GeometryError::MissingNode)?;
        let style = Style {
            size,
            padding,
            box_sizing: BoxSizing::BorderBox,
            ..container_style(FlexDirection::Column)
        };
        taffy
            .new_with_children(style, &[child_taffy_id])
            .map_err(|_| GeometryError::MissingNode)
    } else {
        let child_ids = root_node
            .children
            .iter()
            .map(|&child| build_node(tree, taffy, child, forward))
            .collect::<Result<Vec<_>, _>>()?;
        let style = Style {
            size,
            padding,
            box_sizing: BoxSizing::BorderBox,
            ..container_style(FlexDirection::Column)
        };
        taffy
            .new_with_children(style, &child_ids)
            .map_err(|_| GeometryError::MissingNode)
    }
}

/// Builds one Taffy node (leaf or container) for `id` and every descendant,
/// applying `id`'s own `grow`/cross-axis-stretch to its own style. Legal to
/// apply unconditionally: `youth-tree`'s own validation already guarantees
/// `grow > 0` only ever occurs under a `Box`/`Row` parent, so by the time a
/// node is reached here its parent (wherever it ends up in the Taffy tree)
/// is already known to be grow-aware.
fn build_node(
    tree: &Tree,
    taffy: &mut TaffyTree<NodeId>,
    id: NodeId,
    forward: &mut HashMap<NodeId, TaffyNodeId>,
) -> Result<TaffyNodeId, GeometryError> {
    let node = tree.node(id).ok_or(GeometryError::MissingNode)?;
    let taffy_id = match node.data.box_layout() {
        Some(BoxLayout::Column) => {
            let child_ids = node
                .children
                .iter()
                .map(|&child| build_node(tree, taffy, child, forward))
                .collect::<Result<Vec<_>, _>>()?;
            taffy
                .new_with_children(container_style(FlexDirection::Column), &child_ids)
                .map_err(|_| GeometryError::MissingNode)?
        }
        Some(BoxLayout::Row) => {
            let child_ids = node
                .children
                .iter()
                .map(|&child| build_node(tree, taffy, child, forward))
                .collect::<Result<Vec<_>, _>>()?;
            taffy
                .new_with_children(container_style(FlexDirection::Row), &child_ids)
                .map_err(|_| GeometryError::MissingNode)?
        }
        Some(BoxLayout::Grid { columns }) => {
            let child_ids = node
                .children
                .iter()
                .map(|&child| build_node(tree, taffy, child, forward))
                .collect::<Result<Vec<_>, _>>()?;
            taffy
                .new_with_children(grid_style(columns), &child_ids)
                .map_err(|_| GeometryError::MissingNode)?
        }
        None => taffy
            .new_leaf_with_context(leaf_style(), id)
            .map_err(|_| GeometryError::MissingNode)?,
    };
    if node.grow > 0 {
        let mut style = taffy
            .style(taffy_id)
            .map_err(|_| GeometryError::MissingNode)?
            .clone();
        style.flex_grow = f32::from(node.grow);
        style.align_self = Some(AlignItems::STRETCH);
        taffy
            .set_style(taffy_id, style)
            .map_err(|_| GeometryError::MissingNode)?;
    }
    forward.insert(id, taffy_id);
    Ok(taffy_id)
}

/// Every load-bearing Taffy default that diverges from today's engine is
/// set here explicitly, not left implicit:
/// - `align_items: FlexStart` -- Taffy's own CSS default is `Stretch`,
///   which would cross-axis-stretch every child unconditionally; only a
///   grow child's own `align_self: Stretch` (set in `build_node`) should
///   stretch.
/// - `box_sizing: ContentBox` -- the old engine always adds padding on top
///   of content size, never eats into a fixed size (Taffy's default is
///   `BorderBox`).
/// - `overflow: Visible` -- matches today (no layout-level clipping
///   concept exists anywhere in the old engine); also Taffy's own default,
///   set explicitly per the plan's "don't accidentally inherit" rule.
/// - `flex_shrink` is deliberately left at Taffy's own default (`1.0`):
///   the old engine never implemented real cross-sibling shrink
///   distribution, so its apparent "no shrink" was an accident (children
///   could silently overflow a too-small container) rather than a
///   preserved product requirement -- Taffy's real flex-shrink is adopted
///   as a genuine correctness improvement.
fn container_style(direction: FlexDirection) -> Style {
    let gap = match direction {
        FlexDirection::Row | FlexDirection::RowReverse => Size {
            width: length(CHILD_GAP as f32),
            height: length(0.0_f32),
        },
        FlexDirection::Column | FlexDirection::ColumnReverse => Size {
            width: length(0.0_f32),
            height: length(CHILD_GAP as f32),
        },
    };
    Style {
        display: Display::Flex,
        flex_direction: direction,
        padding: uniform_padding(BOX_PADDING),
        gap,
        align_items: Some(AlignItems::FLEX_START),
        box_sizing: BoxSizing::ContentBox,
        overflow: Point {
            x: taffy::style::Overflow::Visible,
            y: taffy::style::Overflow::Visible,
        },
        ..Default::default()
    }
}

/// Uses Taffy's native Grid intrinsic-sizing algorithm throughout (both
/// measurement and placement) via a `1fr`-per-column template -- the old
/// engine's `measure_container` used a different, inconsistent formula for
/// a Grid's own intrinsic size (per-column max-content) than its placement
/// formula (equal division); that inconsistency is deliberately not
/// preserved.
fn grid_style(columns: u8) -> Style {
    Style {
        display: Display::Grid,
        grid_template_columns: vec![taffy::style_helpers::repeat(
            u16::from(columns),
            vec![taffy::style_helpers::fr(1.0_f32)],
        )],
        padding: uniform_padding(BOX_PADDING),
        gap: Size {
            width: length(CHILD_GAP as f32),
            height: length(CHILD_GAP as f32),
        },
        align_items: Some(AlignItems::FLEX_START),
        box_sizing: BoxSizing::ContentBox,
        overflow: Point {
            x: taffy::style::Overflow::Visible,
            y: taffy::style::Overflow::Visible,
        },
        ..Default::default()
    }
}

fn leaf_style() -> Style {
    Style {
        box_sizing: BoxSizing::ContentBox,
        overflow: Point {
            x: taffy::style::Overflow::Visible,
            y: taffy::style::Overflow::Visible,
        },
        ..Default::default()
    }
}

fn uniform_padding(value: f64) -> Rect<taffy::style::LengthPercentage> {
    Rect {
        left: length(value as f32),
        right: length(value as f32),
        top: length(value as f32),
        bottom: length(value as f32),
    }
}

/// Mirrors `geometry::measure`'s per-kind intrinsic sizing, but only for
/// leaf kinds -- containers (`Root`/`Box`/`Row`/`Grid`) are never leaves in
/// the Taffy tree, so this is never called for them.
fn leaf_intrinsic_size(data: &NodeData) -> (f64, f64) {
    match data {
        NodeData::Text { value } | NodeData::AlignedText { value, .. } => {
            (value.chars().count() as f64 * GLYPH_WIDTH, GLYPH_HEIGHT)
        }
        NodeData::Editor { text, .. } => (text.chars().count() as f64 * GLYPH_WIDTH, GLYPH_HEIGHT),
        NodeData::TextDocumentEditor { .. } => (GLYPH_WIDTH, GLYPH_HEIGHT),
        NodeData::Countdown { .. } | NodeData::AlignedCountdown { .. } => {
            (5.0 * GLYPH_WIDTH, GLYPH_HEIGHT)
        }
        NodeData::Button { label, .. } | NodeData::ShortcutButton { label, .. } => (
            (label.chars().count() as f64 * GLYPH_WIDTH + BUTTON_HORIZONTAL_PADDING * 2.0)
                .max(BUTTON_MIN_WIDTH),
            (GLYPH_HEIGHT + BUTTON_VERTICAL_PADDING * 2.0).max(BUTTON_MIN_HEIGHT),
        ),
        NodeData::Root | NodeData::Box { .. } | NodeData::Row { .. } | NodeData::Grid { .. } => {
            (0.0, 0.0)
        }
    }
}

/// Pre-order DFS (root-first, children left-to-right) over `tree`'s own
/// child order, accumulating each node's absolute origin from Taffy's
/// parent-relative `Layout::location`. This traversal order -- not Taffy's
/// own internal node iteration -- is what `hit_order` must reproduce, since
/// it's load-bearing for `hit_test()`'s reverse-iterate semantics.
fn extract_subtree(
    tree: &Tree,
    forward: &HashMap<NodeId, TaffyNodeId>,
    taffy: &TaffyTree<NodeId>,
    id: NodeId,
    parent_origin: (f64, f64),
    ancestor_enabled: bool,
    snapshot: &mut LayoutSnapshot,
) -> Result<(), GeometryError> {
    let node = tree.node(id).ok_or(GeometryError::MissingNode)?;
    let taffy_id = *forward.get(&id).ok_or(GeometryError::MissingNode)?;
    let layout = taffy
        .layout(taffy_id)
        .map_err(|_| GeometryError::MissingNode)?;
    let x = parent_origin.0 + f64::from(layout.location.x);
    let y = parent_origin.1 + f64::from(layout.location.y);
    let width = f64::from(layout.size.width);
    let height = f64::from(layout.size.height);

    let own_enabled = match &node.data {
        NodeData::Box { enabled }
        | NodeData::Row { enabled }
        | NodeData::Grid { enabled, .. }
        | NodeData::Button { enabled, .. }
        | NodeData::ShortcutButton { enabled, .. } => *enabled,
        NodeData::Root
        | NodeData::Text { .. }
        | NodeData::AlignedText { .. }
        | NodeData::Editor { .. }
        | NodeData::TextDocumentEditor { .. }
        | NodeData::Countdown { .. }
        | NodeData::AlignedCountdown { .. } => true,
    };
    let effective_enabled = ancestor_enabled && own_enabled;
    let interaction = if node.data.is_focusable() {
        InteractionKind::Button
    } else {
        InteractionKind::None
    };
    snapshot.nodes.insert(
        id,
        LayoutNode {
            bounds: LogicalRect {
                x,
                y,
                width,
                height,
            },
            effective_enabled,
            interaction,
        },
    );
    snapshot.hit_order.push(id);
    for &child in &node.children {
        extract_subtree(
            tree,
            forward,
            taffy,
            child,
            (x, y),
            effective_enabled,
            snapshot,
        )?;
    }
    Ok(())
}
