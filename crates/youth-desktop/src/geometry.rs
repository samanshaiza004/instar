use std::collections::BTreeMap;

use thiserror::Error;
use youth_tree::{NodeData, NodeId, PatchBatch, Tree, TreeSnapshot};

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
            OUTER_MARGIN,
            y,
            size.width.min((viewport.width - OUTER_MARGIN).max(0.0)),
            true,
            &mut snapshot,
        )?;
        y += size.height + CHILD_GAP;
    }
    Ok(snapshot)
}

pub fn hit_test(snapshot: &LayoutSnapshot, point: LogicalPoint) -> Option<NodeId> {
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
        NodeData::Text { value } => Ok(LogicalSize {
            width: value.chars().count() as f64 * GLYPH_WIDTH,
            height: GLYPH_HEIGHT,
        }),
        NodeData::Button { label, .. } => Ok(LogicalSize {
            width: (label.chars().count() as f64 * GLYPH_WIDTH + BUTTON_HORIZONTAL_PADDING * 2.0)
                .max(BUTTON_MIN_WIDTH),
            height: (GLYPH_HEIGHT + BUTTON_VERTICAL_PADDING * 2.0).max(BUTTON_MIN_HEIGHT),
        }),
        NodeData::Box { .. } => {
            let mut width: f64 = 0.0;
            let mut height = BOX_PADDING * 2.0;
            for (index, child) in node.children.iter().enumerate() {
                let child = measure(tree, *child)?;
                width = width.max(child.width);
                height += child.height;
                if index + 1 < node.children.len() {
                    height += CHILD_GAP;
                }
            }
            Ok(LogicalSize {
                width: width + BOX_PADDING * 2.0,
                height,
            })
        }
    }
}

fn place(
    tree: &Tree,
    id: NodeId,
    x: f64,
    y: f64,
    available_width: f64,
    ancestor_enabled: bool,
    snapshot: &mut LayoutSnapshot,
) -> Result<LogicalSize, GeometryError> {
    let node = tree.node(id).ok_or(GeometryError::MissingNode)?;
    let measured = measure(tree, id)?;
    let width = measured.width.min(available_width.max(0.0));
    let own_enabled = match &node.data {
        NodeData::Box { enabled } | NodeData::Button { enabled, .. } => *enabled,
        NodeData::Root | NodeData::Text { .. } => true,
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
            interaction: if matches!(node.data, NodeData::Button { .. }) {
                InteractionKind::Button
            } else {
                InteractionKind::None
            },
        },
    );
    snapshot.hit_order.push(id);
    if matches!(node.data, NodeData::Box { .. }) {
        let child_x = x + BOX_PADDING;
        let child_width = (width - BOX_PADDING * 2.0).max(0.0);
        let mut child_y = y + BOX_PADDING;
        for child in &node.children {
            let child_size = place(
                tree,
                *child,
                child_x,
                child_y,
                child_width,
                effective_enabled,
                snapshot,
            )?;
            child_y += child_size.height + CHILD_GAP;
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
    fn tiny_zero_and_invalid_viewports_are_safe() {
        assert!(layout(&counter(true), LogicalSize::new(0.0, 0.0).unwrap()).is_ok());
        assert!(layout(&counter(true), LogicalSize::new(1.0, 1.0).unwrap()).is_ok());
        assert!(LogicalSize::new(f64::NAN, 1.0).is_err());
        assert!(LogicalSize::new(1.0, f64::INFINITY).is_err());
        assert!(LogicalSize::new(-1.0, 1.0).is_err());
    }
}
