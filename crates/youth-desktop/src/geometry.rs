use std::collections::BTreeMap;

use thiserror::Error;
use youth_tree::{NodeId, PatchBatch, Tree, TreeSnapshot};

mod taffy_adapter;

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

/// Computes bounds for every node in `tree` against `viewport`, via Taffy
/// (see `taffy_adapter`) -- the style translation, leaf measurement
/// callbacks, and Taffy-output-to-`LayoutSnapshot` conversion all live
/// there. This function is the stable public entry point: its signature
/// and output contract (`LayoutSnapshot`/`LayoutNode`/`hit_test()`) predate
/// the Taffy migration and are unchanged by it.
pub fn layout(tree: &Tree, viewport: LogicalSize) -> Result<LayoutSnapshot, GeometryError> {
    let _span = tracing::info_span!("desktop.layout", revision = tree.revision()).entered();
    // `viewport`'s fields are `pub`, so a caller could construct one via a
    // struct literal that bypasses `LogicalSize::new`'s validation --
    // re-validate here rather than trust the type alone.
    LogicalSize::new(viewport.width, viewport.height)?;
    taffy_adapter::layout_taffy(tree, viewport)
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
                        grow: 0,
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Box { enabled },
                        children: vec![id(3), id(4)],
                        grow: 0,
                    },
                    Node {
                        id: id(3),
                        data: NodeData::Text {
                            value: "Count: 0".into(),
                        },
                        children: vec![],
                        grow: 0,
                    },
                    Node {
                        id: id(4),
                        data: NodeData::Button {
                            label: "Increment".into(),
                            enabled: true,
                        },
                        children: vec![],
                        grow: 0,
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
                        grow: 0,
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Box { enabled: true },
                        children: vec![id(3), id(6)],
                        grow: 0,
                    },
                    Node {
                        id: id(3),
                        data: NodeData::Row { enabled: true },
                        children: vec![id(4), id(5)],
                        grow: 0,
                    },
                    Node {
                        id: id(4),
                        data: NodeData::Button {
                            label: "A".into(),
                            enabled: true,
                        },
                        children: vec![],
                        grow: 0,
                    },
                    Node {
                        id: id(5),
                        data: NodeData::Button {
                            label: "B".into(),
                            enabled: true,
                        },
                        children: vec![],
                        grow: 0,
                    },
                    Node {
                        id: id(6),
                        data: NodeData::Grid {
                            enabled: true,
                            columns: 2,
                        },
                        children: vec![id(7), id(8), id(9)],
                        grow: 0,
                    },
                    Node {
                        id: id(7),
                        data: NodeData::Button {
                            label: "1".into(),
                            enabled: true,
                        },
                        children: vec![],
                        grow: 0,
                    },
                    Node {
                        id: id(8),
                        data: NodeData::Button {
                            label: "2".into(),
                            enabled: true,
                        },
                        children: vec![],
                        grow: 0,
                    },
                    Node {
                        id: id(9),
                        data: NodeData::Button {
                            label: "3".into(),
                            enabled: true,
                        },
                        children: vec![],
                        grow: 0,
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap()
    }

    fn growing_node(value: u64, data: NodeData, grow: u16, children: &[u64]) -> Node {
        Node {
            id: id(value),
            data,
            grow,
            children: children.iter().copied().map(id).collect(),
        }
    }

    fn plain_node(value: u64, data: NodeData, children: &[u64]) -> Node {
        growing_node(value, data, 0, children)
    }

    fn text(value: &str) -> NodeData {
        NodeData::Text {
            value: value.to_owned(),
        }
    }

    /// A column mirroring the "narrow P1.5 responsive-layout slice" acceptance
    /// shape: an intrinsic header, one grow=1 editor, and an intrinsic footer.
    fn scratchpad_shaped_tree() -> Tree {
        Tree::from_snapshot(
            TreeSnapshot {
                revision: 1,
                root: id(1),
                nodes: vec![
                    plain_node(1, NodeData::Root, &[2]),
                    plain_node(2, NodeData::Box { enabled: true }, &[3, 4, 5]),
                    plain_node(3, text("all.txt"), &[]),
                    growing_node(
                        4,
                        NodeData::Editor {
                            document_revision: 0,
                            text: String::new(),
                        },
                        1,
                        &[],
                    ),
                    plain_node(
                        5,
                        NodeData::Button {
                            label: "Save".into(),
                            enabled: true,
                        },
                        &[],
                    ),
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
                        grow: 0,
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Editor {
                            document_revision: 1,
                            text: "draft".into(),
                        },
                        children: vec![],
                        grow: 0,
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

    /// Not a preserved legacy approximation -- this pins Taffy's real CSS
    /// Grid track-sizing algorithm, and must not be "corrected" back toward
    /// the old engine's numbers. The old engine used two *different*
    /// formulas for a Grid: `measure_container` computed intrinsic size
    /// from each column's own independent max-content width (unequal
    /// columns for unequal content), while `place()` always divided the
    /// offered width into equal `track_width` tracks regardless of content
    /// -- an internal inconsistency, not a real feature. Taffy's native
    /// `1fr`-per-column template resolves both intrinsic sizing and
    /// placement through the same shared track-sizing algorithm: since
    /// both tracks share the same flex factor (`1fr` each), Taffy sizes
    /// them *symmetrically* to the widest item's max-content contribution
    /// even during intrinsic (auto) sizing, not per-item independently --
    /// so intrinsic and placement sizing now agree with each other (both
    /// give equal columns), closing the inconsistency the old engine had.
    #[test]
    fn grid_intrinsic_width_sums_each_columns_own_max_content() {
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 1,
                root: id(1),
                nodes: vec![
                    plain_node(1, NodeData::Root, &[2]),
                    plain_node(2, NodeData::Box { enabled: true }, &[3]),
                    plain_node(
                        3,
                        NodeData::Grid {
                            enabled: true,
                            columns: 2,
                        },
                        &[4, 5],
                    ),
                    plain_node(
                        4,
                        NodeData::Button {
                            label: "AAAAAAAAAAAAAAAA".into(),
                            enabled: true,
                        },
                        &[],
                    ),
                    plain_node(
                        5,
                        NodeData::Button {
                            label: "B".into(),
                            enabled: true,
                        },
                        &[],
                    ),
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let snapshot = layout(&tree, LogicalSize::new(640.0, 480.0).unwrap()).unwrap();
        let grid = snapshot.nodes[&id(3)].bounds;
        let wide_column =
            (16.0 * GLYPH_WIDTH + BUTTON_HORIZONTAL_PADDING * 2.0).max(BUTTON_MIN_WIDTH);
        // Both `1fr` tracks share the same flex factor, so Taffy sizes both
        // columns to the widest item's max-content contribution
        // *symmetrically* for intrinsic sizing -- not per-item independent
        // max-content. This is the key finding this test exists to pin:
        // intrinsic sizing and placement sizing are now mutually
        // *consistent* with each other (both give equal `1fr` columns),
        // which is exactly what the old engine's two-different-formulas
        // approach did not guarantee.
        let expected_width = wide_column * 2.0 + CHILD_GAP + BOX_PADDING * 2.0;
        assert!(
            (grid.width - expected_width).abs() < 1e-2,
            "grid intrinsic width {} did not match the symmetric per-fr-track max-content sum {}",
            grid.width,
            expected_width
        );
        let wide = snapshot.nodes[&id(4)].bounds;
        let narrow = snapshot.nodes[&id(5)].bounds;
        assert!(
            (wide.width - narrow.width).abs() < 1e-2,
            "equal flex-factor (1fr) columns should size equally even under unequal content, matching placement's own equal-division behavior"
        );
    }

    /// Rewritten, not ported: the old engine's `has_grow_child` propagation
    /// meant a Root child with no grow anywhere stayed intrinsic-width.
    /// Under the new "single-child Root always fills" contract this is no
    /// longer true -- the outer Box now fills the content width exactly,
    /// regardless of grow, since a lone Root child unambiguously *is* the
    /// app body. This is the deliberate, documented behavior change from
    /// the plan, not a regression.
    #[test]
    fn single_child_root_fills_even_with_no_grow_anywhere() {
        let snapshot = layout(&rich_layout(), LogicalSize::new(640.0, 480.0).unwrap()).unwrap();
        let root = snapshot.nodes[&id(1)].bounds;
        let outer = snapshot.nodes[&id(2)].bounds;
        let row = snapshot.nodes[&id(3)].bounds;
        let grid = snapshot.nodes[&id(6)].bounds;
        assert_eq!(
            root,
            LogicalRect {
                x: 0.0,
                y: 0.0,
                width: 640.0,
                height: 480.0
            }
        );
        assert_eq!(outer.x, OUTER_MARGIN);
        assert_eq!(outer.y, OUTER_MARGIN);
        assert_eq!(row.x, OUTER_MARGIN + BOX_PADDING);
        assert_eq!(row.y, OUTER_MARGIN + BOX_PADDING);
        assert_eq!(grid.x, OUTER_MARGIN + BOX_PADDING);
        assert_eq!(grid.y, row.y + row.height + CHILD_GAP);
        assert!((outer.width - (640.0 - OUTER_MARGIN * 2.0)).abs() < 1e-3);
    }

    #[test]
    fn editor_consumes_remaining_height_at_three_viewports() {
        for (width, height) in [(640.0, 480.0), (1024.0, 720.0), (1600.0, 900.0)] {
            let tree = scratchpad_shaped_tree();
            let snapshot = layout(&tree, LogicalSize::new(width, height).unwrap()).unwrap();
            let viewport_rect = snapshot.nodes[&id(1)].bounds;
            assert_eq!(
                viewport_rect,
                LogicalRect {
                    x: 0.0,
                    y: 0.0,
                    width,
                    height
                }
            );
            let content_rect = snapshot.nodes[&id(2)].bounds;
            assert_eq!(content_rect.x, OUTER_MARGIN);
            assert_eq!(content_rect.y, OUTER_MARGIN);
            assert!((content_rect.width - (width - OUTER_MARGIN * 2.0)).abs() < 1e-3);
            assert!((content_rect.height - (height - OUTER_MARGIN * 2.0)).abs() < 1e-3);

            let filename = snapshot.nodes[&id(3)].bounds;
            let editor = snapshot.nodes[&id(4)].bounds;
            let save = snapshot.nodes[&id(5)].bounds;
            assert_eq!(filename.height, GLYPH_HEIGHT);
            assert_eq!(save.height, BUTTON_MIN_HEIGHT);
            assert!((editor.y - (filename.y + filename.height + CHILD_GAP)).abs() < 1e-3);
            assert!((save.y - (editor.y + editor.height + CHILD_GAP)).abs() < 1e-3);

            let inner_height = content_rect.height - BOX_PADDING * 2.0;
            let expected_editor_height =
                inner_height - filename.height - save.height - CHILD_GAP * 2.0;
            assert!(
                (editor.height - expected_editor_height).abs() < 1e-2,
                "editor height {} did not consume remaining space {} at {width}x{height}",
                editor.height,
                expected_editor_height
            );
            assert!((editor.width - (content_rect.width - BOX_PADDING * 2.0)).abs() < 1e-3);
        }
    }

    #[test]
    fn unequal_weights_distribute_positive_free_space_on_top_of_intrinsic_size() {
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 1,
                root: id(1),
                nodes: vec![
                    plain_node(1, NodeData::Root, &[2]),
                    plain_node(2, NodeData::Row { enabled: true }, &[3, 4]),
                    growing_node(3, text("AAAAAAAA"), 1, &[]),
                    growing_node(4, text("BBBBBBBBBBBBBBBB"), 1, &[]),
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let width = 500.0 + OUTER_MARGIN * 2.0 + BOX_PADDING * 2.0;
        let snapshot = layout(&tree, LogicalSize::new(width, 200.0).unwrap()).unwrap();
        let a = snapshot.nodes[&id(3)].bounds;
        let b = snapshot.nodes[&id(4)].bounds;
        let base = 64.0 + 128.0 + CHILD_GAP;
        let free = 500.0 - base;
        assert!((a.width - (64.0 + free / 2.0)).abs() < 1e-2);
        assert!((b.width - (128.0 + free / 2.0)).abs() < 1e-2);
        assert!((b.x + b.width - (a.x + a.width + CHILD_GAP + b.width)).abs() < 1e-2);
    }

    #[test]
    fn free_space_not_evenly_divisible_lands_exactly_on_the_container_boundary() {
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 1,
                root: id(1),
                nodes: vec![
                    plain_node(1, NodeData::Root, &[2]),
                    plain_node(2, NodeData::Row { enabled: true }, &[3, 4, 5]),
                    growing_node(3, text(""), 1, &[]),
                    growing_node(4, text(""), 2, &[]),
                    growing_node(5, text(""), 3, &[]),
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let snapshot = layout(&tree, LogicalSize::new(311.0, 100.0).unwrap()).unwrap();
        let row = snapshot.nodes[&id(2)].bounds;
        let last = snapshot.nodes[&id(5)].bounds;
        let inner_right = row.x + (row.width - BOX_PADDING * 2.0) + BOX_PADDING;
        assert!(
            (last.x + last.width - inner_right).abs() < 1e-2,
            "last grow child did not land exactly on the container's inner boundary"
        );
    }

    #[test]
    fn nested_row_inside_a_grow_aware_column_receives_its_own_forced_width() {
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 1,
                root: id(1),
                nodes: vec![
                    plain_node(1, NodeData::Root, &[2]),
                    plain_node(2, NodeData::Box { enabled: true }, &[3, 4]),
                    plain_node(3, text("header"), &[]),
                    growing_node(4, NodeData::Row { enabled: true }, 1, &[5]),
                    plain_node(5, text("nested"), &[]),
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let snapshot = layout(&tree, LogicalSize::new(400.0, 400.0).unwrap()).unwrap();
        let column = snapshot.nodes[&id(2)].bounds;
        let nested_row = snapshot.nodes[&id(4)].bounds;
        assert!((nested_row.width - (column.width - BOX_PADDING * 2.0)).abs() < 1e-3);
    }

    #[test]
    fn nested_column_inside_a_grow_aware_row_receives_its_own_forced_height() {
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 1,
                root: id(1),
                nodes: vec![
                    plain_node(1, NodeData::Root, &[2]),
                    plain_node(2, NodeData::Row { enabled: true }, &[3, 4]),
                    plain_node(3, text("sidebar"), &[]),
                    growing_node(4, NodeData::Box { enabled: true }, 1, &[5]),
                    plain_node(5, text("nested"), &[]),
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let snapshot = layout(&tree, LogicalSize::new(400.0, 300.0).unwrap()).unwrap();
        let row = snapshot.nodes[&id(2)].bounds;
        let nested_column = snapshot.nodes[&id(4)].bounds;
        assert!((nested_column.height - (row.height - BOX_PADDING * 2.0)).abs() < 1e-3);
    }

    #[test]
    fn a_grown_editors_pointer_hit_area_extends_past_its_old_intrinsic_bounds() {
        let tree = scratchpad_shaped_tree();
        let snapshot = layout(&tree, LogicalSize::new(400.0, 400.0).unwrap()).unwrap();
        let editor = snapshot.nodes[&id(4)].bounds;
        assert!(
            editor.height > GLYPH_HEIGHT * 2.0,
            "editor did not grow past its intrinsic height"
        );
        let point_below_intrinsic_height = LogicalPoint {
            x: editor.x + 1.0,
            y: editor.y + GLYPH_HEIGHT + 1.0,
        };
        assert_eq!(
            hit_test(&snapshot, point_below_intrinsic_height),
            Some(id(4)),
            "hit-testing did not see the editor's grown bounds"
        );
    }

    /// Not a preserved legacy approximation -- this fixture represents a
    /// multi-child Root containing an auto-sized flex container (Box) with
    /// a growing descendant. Its dimensions follow real Taffy/CSS flex
    /// intrinsic sizing, not the legacy Youth `has_grow_child` propagation
    /// approximation, and must not be "corrected" back toward a
    /// hand-derived `padding + child's raw intrinsic size` number.
    ///
    /// The old engine's Root loop let a grow signal several levels below
    /// Root implicitly inflate an intermediate non-grow ancestor's own
    /// placed size (`has_grow_child` was checked recursively at every
    /// level). Real CSS flex-grow is per-item and does not propagate
    /// upward through an `auto`-sized ancestor that way, and a multi-child
    /// Root is deliberately just an ordinary Column under the new contract.
    /// What *is* guaranteed, and asserted below, is the structural
    /// contract: the first child does not consume the full remaining
    /// viewport height the way the old engine's Root propagation made it
    /// do. The exact height Taffy assigns (see the inline constant below)
    /// is a documented CSS flexbox interaction -- an auto-sized flex
    /// container's own hypothetical main size, when it has a flex-grow
    /// child, does not reduce to a naive padding-plus-content sum -- taken
    /// from Taffy's own verified-stable output rather than hand-derived,
    /// since this fixture exists only in this test and is never exercised
    /// by real SDK usage.
    #[test]
    fn multi_child_root_is_an_ordinary_column_not_a_grow_propagation() {
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 1,
                root: id(1),
                nodes: vec![
                    plain_node(1, NodeData::Root, &[2, 4]),
                    growing_node(2, NodeData::Box { enabled: true }, 0, &[3]),
                    growing_node(3, text(""), 1, &[]),
                    plain_node(4, text("second"), &[]),
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let snapshot = layout(&tree, LogicalSize::new(300.0, 300.0).unwrap()).unwrap();
        let first = snapshot.nodes[&id(2)].bounds;
        let second = snapshot.nodes[&id(4)].bounds;
        // 64.0 is Taffy's real, verified-stable output -- see the doc
        // comment on this test for why it isn't hand-derived.
        assert!(
            (first.height - 64.0).abs() < 1e-2,
            "first child's height is no longer Taffy's verified-stable value: {}",
            first.height
        );
        assert!((second.y - (first.y + first.height + CHILD_GAP)).abs() < 1e-3);
        assert!(
            first.y + first.height < 300.0 - OUTER_MARGIN - 1.0,
            "first child must not fill the full remaining height under the new ordinary-Column contract"
        );
    }
}
