use std::collections::BTreeMap;

use thiserror::Error;
use youth_tree::{BoxLayout, NodeData, NodeId, PatchBatch, Tree, TreeSnapshot};

pub const OUTER_MARGIN: f64 = 24.0;
pub const BOX_PADDING: f64 = 16.0;
pub const CHILD_GAP: f64 = 12.0;
pub const GLYPH_WIDTH: f64 = 8.0;
pub const GLYPH_HEIGHT: f64 = 12.0;
pub const BUTTON_HORIZONTAL_PADDING: f64 = 12.0;
pub const BUTTON_VERTICAL_PADDING: f64 = 8.0;
pub const BUTTON_MIN_WIDTH: f64 = 80.0;
pub const BUTTON_MIN_HEIGHT: f64 = 32.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalSize {
    pub width: f64,
    pub height: f64,
}

impl LogicalSize {
    pub fn new(width: f64, height: f64) -> Result<Self, GeometryError> {
        if !width.is_finite() || !height.is_finite() || width < 0.0 || height < 0.0 {
            return Err(GeometryError::InvalidViewport);
        }
        Ok(Self { width, height })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl LogicalRect {
    #[must_use]
    pub fn contains(self, point: LogicalPoint) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + self.width
            && point.y < self.y + self.height
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionKind {
    None,
    Button,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutNode {
    pub bounds: LogicalRect,
    pub effective_enabled: bool,
    pub interaction: InteractionKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutSnapshot {
    pub tree_revision: u64,
    pub viewport: LogicalSize,
    pub nodes: BTreeMap<NodeId, LayoutNode>,
    hit_order: Vec<NodeId>,
}

#[derive(Debug, Error)]
pub enum GeometryError {
    #[error("viewport must have finite, non-negative dimensions")]
    InvalidViewport,
    #[error("renderer mirror rejected a semantic snapshot: {0}")]
    InvalidSnapshot(#[from] youth_tree::ValidationError),
    #[error("renderer mirror rejected a committed patch: {0}")]
    InvalidPatch(#[from] youth_tree::PatchError),
    #[error("validated tree is internally incomplete")]
    MissingNode,
}

#[derive(Clone, Debug)]
pub struct RendererMirror {
    tree: Tree,
    limits: youth_tree::Limits,
}

impl RendererMirror {
    pub fn from_snapshot(
        snapshot: TreeSnapshot,
        limits: youth_tree::Limits,
    ) -> Result<Self, GeometryError> {
        let tree = Tree::from_snapshot(snapshot, &limits)?;
        Ok(Self { tree, limits })
    }

    pub fn replace(&mut self, snapshot: TreeSnapshot) -> Result<(), GeometryError> {
        self.tree = Tree::from_snapshot(snapshot, &self.limits)?;
        Ok(())
    }

    pub fn apply(&mut self, batch: PatchBatch) -> Result<(), GeometryError> {
        self.tree.apply(batch, &self.limits)?;
        Ok(())
    }

    #[must_use]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }
}

pub fn layout(tree: &Tree, viewport: LogicalSize) -> Result<LayoutSnapshot, GeometryError> {
    let _span = tracing::info_span!("desktop.layout", revision = tree.revision()).entered();
    LogicalSize::new(viewport.width, viewport.height)?;
    let mut snapshot = LayoutSnapshot {
        tree_revision: tree.revision(),
        viewport,
        nodes: BTreeMap::new(),
        hit_order: Vec::with_capacity(tree.node_count()),
    };
    let root = tree.root();
    snapshot.nodes.insert(
        root,
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
    snapshot.hit_order.push(root);
    let root_node = tree.node(root).ok_or(GeometryError::MissingNode)?;
    let mut y = OUTER_MARGIN;
    for child in &root_node.children {
        let size = measure(tree, *child)?;
        place(
            tree,
            *child,
            LogicalPoint { x: OUTER_MARGIN, y },
            size.width.min((viewport.width - OUTER_MARGIN).max(0.0)),
            true,
            false,
            &mut snapshot,
        )?;
        y += size.height + CHILD_GAP;
    }
    Ok(snapshot)
}

pub fn hit_test(snapshot: &LayoutSnapshot, point: LogicalPoint) -> Option<NodeId> {
    let _span =
        tracing::info_span!("desktop.hit_test", revision = snapshot.tree_revision).entered();
    if !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    snapshot.hit_order.iter().rev().copied().find(|id| {
        snapshot.nodes.get(id).is_some_and(|node| {
            node.effective_enabled
                && node.interaction == InteractionKind::Button
                && node.bounds.contains(point)
        })
    })
}

fn measure(tree: &Tree, id: NodeId) -> Result<LogicalSize, GeometryError> {
    let node = tree.node(id).ok_or(GeometryError::MissingNode)?;
    match &node.data {
        NodeData::Root => Ok(LogicalSize {
            width: 0.0,
            height: 0.0,
        }),
        NodeData::Text { value } | NodeData::AlignedText { value, .. } => Ok(LogicalSize {
            width: value.chars().count() as f64 * GLYPH_WIDTH,
            height: GLYPH_HEIGHT,
        }),
        NodeData::Editor { text, .. } => Ok(LogicalSize {
            width: text.chars().count() as f64 * GLYPH_WIDTH,
            height: GLYPH_HEIGHT,
        }),
        NodeData::TextDocumentEditor { .. } => Ok(LogicalSize {
            width: GLYPH_WIDTH,
            height: GLYPH_HEIGHT,
        }),
        NodeData::Countdown { .. } | NodeData::AlignedCountdown { .. } => Ok(LogicalSize {
            width: 5.0 * GLYPH_WIDTH,
            height: GLYPH_HEIGHT,
        }),
        NodeData::Button { label, .. } | NodeData::ShortcutButton { label, .. } => {
            Ok(LogicalSize {
                width: (label.chars().count() as f64 * GLYPH_WIDTH
                    + BUTTON_HORIZONTAL_PADDING * 2.0)
                    .max(BUTTON_MIN_WIDTH),
                height: (GLYPH_HEIGHT + BUTTON_VERTICAL_PADDING * 2.0).max(BUTTON_MIN_HEIGHT),
            })
        }
        NodeData::Box { .. } | NodeData::Row { .. } | NodeData::Grid { .. } => {
            measure_container(tree, node.children.as_slice(), node.data.box_layout())
        }
    }
}

fn measure_container(
    tree: &Tree,
    children: &[NodeId],
    layout: Option<BoxLayout>,
) -> Result<LogicalSize, GeometryError> {
    let sizes = children
        .iter()
        .map(|child| measure(tree, *child))
        .collect::<Result<Vec<_>, _>>()?;
    let gaps = |count: usize| CHILD_GAP * count.saturating_sub(1) as f64;
    let (width, height) = match layout.unwrap_or(BoxLayout::Column) {
        BoxLayout::Column => (
            sizes.iter().map(|size| size.width).fold(0.0, f64::max),
            sizes.iter().map(|size| size.height).sum::<f64>() + gaps(sizes.len()),
        ),
        BoxLayout::Row => (
            sizes.iter().map(|size| size.width).sum::<f64>() + gaps(sizes.len()),
            sizes.iter().map(|size| size.height).fold(0.0, f64::max),
        ),
        BoxLayout::Grid { columns } => {
            let columns = usize::from(columns);
            let mut column_widths = vec![0.0_f64; columns];
            let mut row_heights = vec![0.0_f64; sizes.len().div_ceil(columns)];
            for (index, size) in sizes.iter().enumerate() {
                column_widths[index % columns] = column_widths[index % columns].max(size.width);
                row_heights[index / columns] = row_heights[index / columns].max(size.height);
            }
            (
                column_widths.iter().sum::<f64>() + gaps(column_widths.len()),
                row_heights.iter().sum::<f64>() + gaps(row_heights.len()),
            )
        }
    };
    Ok(LogicalSize {
        width: width + BOX_PADDING * 2.0,
        height: height + BOX_PADDING * 2.0,
    })
}

fn place(
    tree: &Tree,
    id: NodeId,
    origin: LogicalPoint,
    available_width: f64,
    ancestor_enabled: bool,
    stretch_width: bool,
    snapshot: &mut LayoutSnapshot,
) -> Result<LogicalSize, GeometryError> {
    let LogicalPoint { x, y } = origin;
    let node = tree.node(id).ok_or(GeometryError::MissingNode)?;
    let measured = measure(tree, id)?;
    let width = if stretch_width {
        available_width.max(0.0)
    } else {
        measured.width.min(available_width.max(0.0))
    };
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
    snapshot.nodes.insert(
        id,
        LayoutNode {
            bounds: LogicalRect {
                x,
                y,
                width,
                height: measured.height,
            },
            effective_enabled,
            interaction: if node.data.is_focusable() {
                InteractionKind::Button
            } else {
                InteractionKind::None
            },
        },
    );
    snapshot.hit_order.push(id);
    if let Some(box_layout) = node.data.box_layout() {
        let child_x = x + BOX_PADDING;
        let child_width = (width - BOX_PADDING * 2.0).max(0.0);
        match box_layout {
            BoxLayout::Column => {
                let mut child_y = y + BOX_PADDING;
                for child in &node.children {
                    let child_size = place(
                        tree,
                        *child,
                        LogicalPoint {
                            x: child_x,
                            y: child_y,
                        },
                        child_width,
                        effective_enabled,
                        false,
                        snapshot,
                    )?;
                    child_y += child_size.height + CHILD_GAP;
                }
            }
            BoxLayout::Row => {
                let mut next_x = child_x;
                for child in &node.children {
                    let child_size = place(
                        tree,
                        *child,
                        LogicalPoint {
                            x: next_x,
                            y: y + BOX_PADDING,
                        },
                        (child_x + child_width - next_x).max(0.0),
                        effective_enabled,
                        false,
                        snapshot,
                    )?;
                    next_x += child_size.width + CHILD_GAP;
                }
            }
            BoxLayout::Grid { columns } => {
                let columns = usize::from(columns);
                let track_width = ((child_width - CHILD_GAP * columns.saturating_sub(1) as f64)
                    / columns as f64)
                    .max(0.0);
                let sizes = node
                    .children
                    .iter()
                    .map(|child| measure(tree, *child))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut row_y = y + BOX_PADDING;
                for row_start in (0..node.children.len()).step_by(columns) {
                    let row_end = (row_start + columns).min(node.children.len());
                    let row_height = sizes[row_start..row_end]
                        .iter()
                        .map(|size| size.height)
                        .fold(0.0, f64::max);
                    for index in row_start..row_end {
                        let column = index - row_start;
                        place(
                            tree,
                            node.children[index],
                            LogicalPoint {
                                x: child_x + column as f64 * (track_width + CHILD_GAP),
                                y: row_y,
                            },
                            track_width,
                            effective_enabled,
                            true,
                            snapshot,
                        )?;
                    }
                    row_y += row_height + CHILD_GAP;
                }
            }
        }
    }
    Ok(LogicalSize {
        width,
        height: measured.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use youth_tree::{Node, NodeData};

    fn id(value: u64) -> NodeId {
        NodeId::new(value).unwrap()
    }

    fn counter(enabled: bool) -> Tree {
        Tree::from_snapshot(
            TreeSnapshot {
                revision: 3,
                root: id(1),
                nodes: vec![
                    Node {
                        id: id(1),
                        data: NodeData::Root,
                        children: vec![id(2)],
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Box { enabled },
                        children: vec![id(3), id(4)],
                    },
                    Node {
                        id: id(3),
                        data: NodeData::Text {
                            value: "Count: 0".into(),
                        },
                        children: vec![],
                    },
                    Node {
                        id: id(4),
                        data: NodeData::Button {
                            label: "Increment".into(),
                            enabled: true,
                        },
                        children: vec![],
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap()
    }

    fn rich_layout() -> Tree {
        Tree::from_snapshot(
            TreeSnapshot {
                revision: 4,
                root: id(1),
                nodes: vec![
                    Node {
                        id: id(1),
                        data: NodeData::Root,
                        children: vec![id(2)],
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Box { enabled: true },
                        children: vec![id(3), id(6)],
                    },
                    Node {
                        id: id(3),
                        data: NodeData::Row { enabled: true },
                        children: vec![id(4), id(5)],
                    },
                    Node {
                        id: id(4),
                        data: NodeData::Button {
                            label: "A".into(),
                            enabled: true,
                        },
                        children: vec![],
                    },
                    Node {
                        id: id(5),
                        data: NodeData::Button {
                            label: "B".into(),
                            enabled: true,
                        },
                        children: vec![],
                    },
                    Node {
                        id: id(6),
                        data: NodeData::Grid {
                            enabled: true,
                            columns: 2,
                        },
                        children: vec![id(7), id(8), id(9)],
                    },
                    Node {
                        id: id(7),
                        data: NodeData::Button {
                            label: "1".into(),
                            enabled: true,
                        },
                        children: vec![],
                    },
                    Node {
                        id: id(8),
                        data: NodeData::Button {
                            label: "2".into(),
                            enabled: true,
                        },
                        children: vec![],
                    },
                    Node {
                        id: id(9),
                        data: NodeData::Button {
                            label: "3".into(),
                            enabled: true,
                        },
                        children: vec![],
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap()
    }

    #[test]
    fn root_box_text_and_button_have_reference_geometry() {
        let layout = layout(&counter(true), LogicalSize::new(320.0, 180.0).unwrap()).unwrap();
        assert_eq!(layout.nodes[&id(1)].bounds.width, 320.0);
        assert_eq!(layout.nodes[&id(2)].bounds.x, OUTER_MARGIN);
        assert_eq!(layout.nodes[&id(3)].bounds.x, OUTER_MARGIN + BOX_PADDING);
        assert_eq!(layout.nodes[&id(4)].bounds.height, BUTTON_MIN_HEIGHT);
        assert_eq!(
            layout.nodes[&id(4)].bounds.y,
            OUTER_MARGIN + BOX_PADDING + GLYPH_HEIGHT + CHILD_GAP
        );
    }

    #[test]
    fn hit_testing_obeys_bounds_and_disabled_ancestors() {
        let enabled = layout(&counter(true), LogicalSize::new(320.0, 180.0).unwrap()).unwrap();
        let button = enabled.nodes[&id(4)].bounds;
        assert_eq!(
            hit_test(
                &enabled,
                LogicalPoint {
                    x: button.x + 1.0,
                    y: button.y + 1.0
                }
            ),
            Some(id(4))
        );
        assert_eq!(hit_test(&enabled, LogicalPoint { x: 0.0, y: 0.0 }), None);
        let disabled = layout(&counter(false), LogicalSize::new(320.0, 180.0).unwrap()).unwrap();
        assert_eq!(
            hit_test(
                &disabled,
                LogicalPoint {
                    x: button.x + 1.0,
                    y: button.y + 1.0
                }
            ),
            None
        );
    }

    #[test]
    fn editor_is_an_interactive_pointer_target() {
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 0,
                root: id(1),
                nodes: vec![
                    Node {
                        id: id(1),
                        data: NodeData::Root,
                        children: vec![id(2)],
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Editor {
                            document_revision: 1,
                            text: "draft".into(),
                        },
                        children: vec![],
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let layout = layout(&tree, LogicalSize::new(320.0, 180.0).unwrap()).unwrap();
        let editor = layout.nodes[&id(2)].bounds;

        assert_eq!(
            hit_test(
                &layout,
                LogicalPoint {
                    x: editor.x + 1.0,
                    y: editor.y + 1.0,
                }
            ),
            Some(id(2))
        );
    }

    #[test]
    fn tiny_zero_and_invalid_viewports_are_safe() {
        assert!(layout(&counter(true), LogicalSize::new(0.0, 0.0).unwrap()).is_ok());
        assert!(layout(&counter(true), LogicalSize::new(1.0, 1.0).unwrap()).is_ok());
        assert!(LogicalSize::new(f64::NAN, 1.0).is_err());
        assert!(LogicalSize::new(1.0, f64::INFINITY).is_err());
        assert!(LogicalSize::new(-1.0, 1.0).is_err());
    }

    #[test]
    fn row_and_grid_geometry_is_deterministic_and_row_major() {
        let layout = layout(&rich_layout(), LogicalSize::new(640.0, 480.0).unwrap()).unwrap();
        let first_row = layout.nodes[&id(4)].bounds;
        let second_row = layout.nodes[&id(5)].bounds;
        assert_eq!(second_row.x, first_row.x + first_row.width + CHILD_GAP);
        assert_eq!(second_row.y, first_row.y);

        let first = layout.nodes[&id(7)].bounds;
        let second = layout.nodes[&id(8)].bounds;
        let third = layout.nodes[&id(9)].bounds;
        assert_eq!(first.width, second.width);
        assert_eq!(second.x, first.x + first.width + CHILD_GAP);
        assert_eq!(second.y, first.y);
        assert_eq!(third.x, first.x);
        assert_eq!(third.y, first.y + BUTTON_MIN_HEIGHT + CHILD_GAP);
    }
}
