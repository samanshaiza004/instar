use youth_tree::NodeId;

use crate::{LayoutSnapshot, LogicalPoint, hit_test};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputChange {
    pub redraw: bool,
    pub activation: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PointerState {
    pub cursor: Option<LogicalPoint>,
    pub hovered: Option<NodeId>,
    pub armed: Option<NodeId>,
    pub pressed: bool,
}

impl PointerState {
    pub fn move_to(&mut self, point: LogicalPoint, layout: &LayoutSnapshot) -> InputChange {
        let old_hover = self.hovered;
        let old_pressed = self.pressed;
        self.cursor = Some(point);
        self.hovered = hit_test(layout, point);
        self.pressed = self.armed.is_some() && self.armed == self.hovered;
        InputChange {
            redraw: old_hover != self.hovered || old_pressed != self.pressed,
            activation: None,
        }
    }

    pub fn leave(&mut self) -> InputChange {
        let redraw = self.hovered.is_some() || self.pressed;
        self.cursor = None;
        self.hovered = None;
        self.pressed = false;
        InputChange {
            redraw,
            activation: None,
        }
    }

    pub fn press_primary(&mut self) -> InputChange {
        let old = self.pressed;
        self.armed = self.hovered;
        self.pressed = self.armed.is_some();
        InputChange {
            redraw: old != self.pressed,
            activation: None,
        }
    }

    pub fn release_primary(&mut self, layout: &LayoutSnapshot) -> InputChange {
        let hovered = self.cursor.and_then(|cursor| hit_test(layout, cursor));
        let activation = self.armed.filter(|armed| Some(*armed) == hovered);
        let redraw = self.armed.is_some() || self.pressed;
        self.armed = None;
        self.pressed = false;
        self.hovered = hovered;
        InputChange { redraw, activation }
    }

    pub fn reconcile_layout(&mut self, layout: &LayoutSnapshot) -> InputChange {
        let old_hover = self.hovered;
        let old_pressed = self.pressed;
        self.hovered = self.cursor.and_then(|cursor| hit_test(layout, cursor));
        if self.armed.is_some_and(|armed| {
            layout
                .nodes
                .get(&armed)
                .is_none_or(|node| !node.effective_enabled)
        }) {
            self.armed = None;
        }
        self.pressed = self.armed.is_some() && self.armed == self.hovered;
        InputChange {
            redraw: old_hover != self.hovered || old_pressed != self.pressed,
            activation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LogicalSize, layout};
    use youth_tree::{Node, NodeData, Tree, TreeSnapshot};

    fn id(value: u64) -> NodeId {
        NodeId::new(value).unwrap()
    }

    fn fixture(enabled: bool) -> LayoutSnapshot {
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
                        data: NodeData::Box { enabled },
                        children: vec![id(4)],
                    },
                    Node {
                        id: id(4),
                        data: NodeData::Button {
                            label: "Go".into(),
                            enabled: true,
                        },
                        children: vec![],
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        layout(&tree, LogicalSize::new(320.0, 180.0).unwrap()).unwrap()
    }

    fn inside(layout: &LayoutSnapshot) -> LogicalPoint {
        let bounds = layout.nodes[&id(4)].bounds;
        LogicalPoint {
            x: bounds.x + 1.0,
            y: bounds.y + 1.0,
        }
    }

    #[test]
    fn press_release_inside_activates_once() {
        let layout = fixture(true);
        let mut pointer = PointerState::default();
        assert!(pointer.move_to(inside(&layout), &layout).redraw);
        assert!(pointer.press_primary().redraw);
        assert_eq!(pointer.release_primary(&layout).activation, Some(id(4)));
        assert_eq!(pointer.release_primary(&layout).activation, None);
    }

    #[test]
    fn release_outside_and_press_outside_cancel() {
        let layout = fixture(true);
        let mut pointer = PointerState::default();
        pointer.move_to(inside(&layout), &layout);
        pointer.press_primary();
        pointer.move_to(LogicalPoint { x: 0.0, y: 0.0 }, &layout);
        assert_eq!(pointer.release_primary(&layout).activation, None);
        pointer.press_primary();
        pointer.move_to(inside(&layout), &layout);
        assert_eq!(pointer.release_primary(&layout).activation, None);
    }

    #[test]
    fn disabled_ancestors_never_arm() {
        let layout = fixture(false);
        let mut pointer = PointerState::default();
        pointer.move_to(inside(&layout), &layout);
        pointer.press_primary();
        assert_eq!(pointer.armed, None);
        assert_eq!(pointer.release_primary(&layout).activation, None);
    }

    #[test]
    fn one_hundred_clicks_remain_ordered_and_uncoalesced() {
        let layout = fixture(true);
        let mut pointer = PointerState::default();
        pointer.move_to(inside(&layout), &layout);
        let activations = (0..100)
            .filter_map(|_| {
                pointer.press_primary();
                pointer.release_primary(&layout).activation
            })
            .collect::<Vec<_>>();
        assert_eq!(activations, vec![id(4); 100]);
    }
}
