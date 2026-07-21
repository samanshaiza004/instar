//! Renderer-independent focus and logical keyboard policy for Youth.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use youth_tree::{BoxLayout, NodeId, ShortcutKey, Tree};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticAction {
    Focus(NodeId),
    Activate(NodeId),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InteractionSnapshot {
    pub focused: Option<NodeId>,
    pub enabled_actions: Vec<SemanticAction>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalKey {
    Character(char),
    Enter,
    Escape,
    Backspace,
    Space,
    Tab,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InteractionChange {
    pub redraw: bool,
    pub action: Option<SemanticAction>,
}

#[derive(Clone, Debug, Default)]
pub struct InteractionState {
    focused: Option<NodeId>,
    prior_enabled: Vec<NodeId>,
}

impl InteractionState {
    #[must_use]
    pub const fn focused(&self) -> Option<NodeId> {
        self.focused
    }

    #[must_use]
    pub fn snapshot(&self, tree: &Tree) -> InteractionSnapshot {
        let enabled = enabled_buttons(tree);
        let mut enabled_actions = Vec::with_capacity(enabled.len().saturating_mul(2));
        for id in enabled {
            enabled_actions.push(SemanticAction::Focus(id));
            enabled_actions.push(SemanticAction::Activate(id));
        }
        InteractionSnapshot {
            focused: self.focused,
            enabled_actions,
        }
    }

    pub fn reconcile(&mut self, tree: &Tree) -> InteractionChange {
        let next = enabled_buttons(tree);
        let previous_focus = self.focused;
        if self.focused.is_none_or(|focused| !next.contains(&focused)) {
            self.focused = previous_focus.and_then(|focused| {
                self.prior_enabled
                    .iter()
                    .position(|id| *id == focused)
                    .and_then(|position| {
                        next.get(position)
                            .or_else(|| position.checked_sub(1).and_then(|index| next.get(index)))
                    })
                    .copied()
            });
        }
        self.prior_enabled = next;
        InteractionChange {
            redraw: previous_focus != self.focused,
            action: self.focused.map(SemanticAction::Focus),
        }
    }

    pub fn focus_pointer_target(&mut self, tree: &Tree, target: NodeId) -> InteractionChange {
        let previous = self.focused;
        if is_enabled_button(tree, target) {
            self.focused = Some(target);
        }
        InteractionChange {
            redraw: previous != self.focused,
            action: (previous != self.focused)
                .then(|| self.focused.map(SemanticAction::Focus))
                .flatten(),
        }
    }

    pub fn key(
        &mut self,
        tree: &Tree,
        key: LogicalKey,
        modifiers: Modifiers,
        repeated: bool,
    ) -> InteractionChange {
        if repeated {
            return InteractionChange::default();
        }
        let previous = self.focused;
        let activation = match key {
            LogicalKey::Tab => {
                self.focused = traverse_linear(
                    &enabled_buttons(tree),
                    self.focused,
                    if modifiers.shift { -1 } else { 1 },
                );
                None
            }
            LogicalKey::ArrowLeft => {
                self.focused = arrow(tree, self.focused, AxisDirection::Left);
                None
            }
            LogicalKey::ArrowRight => {
                self.focused = arrow(tree, self.focused, AxisDirection::Right);
                None
            }
            LogicalKey::ArrowUp => {
                self.focused = arrow(tree, self.focused, AxisDirection::Up);
                None
            }
            LogicalKey::ArrowDown => {
                self.focused = arrow(tree, self.focused, AxisDirection::Down);
                None
            }
            LogicalKey::Space => self.focused,
            LogicalKey::Enter => shortcut_target(tree, &ShortcutKey::Enter).or(self.focused),
            LogicalKey::Escape => shortcut_target(tree, &ShortcutKey::Escape),
            LogicalKey::Backspace => shortcut_target(tree, &ShortcutKey::Backspace),
            LogicalKey::Character(value) if !modifiers.control && !modifiers.super_key => {
                shortcut_target(tree, &ShortcutKey::Character(value.to_string()))
            }
            LogicalKey::Character(_) => None,
        };
        if let Some(target) = activation {
            self.focused = Some(target);
        }
        self.prior_enabled = enabled_buttons(tree);
        InteractionChange {
            redraw: previous != self.focused,
            action: activation.map(SemanticAction::Activate).or_else(|| {
                (previous != self.focused)
                    .then_some(self.focused)
                    .flatten()
                    .map(SemanticAction::Focus)
            }),
        }
    }
}

fn enabled_buttons(tree: &Tree) -> Vec<NodeId> {
    let mut result = Vec::new();
    visit_enabled(tree, tree.root(), true, &mut result);
    result
}

fn visit_enabled(tree: &Tree, id: NodeId, ancestor_enabled: bool, result: &mut Vec<NodeId>) {
    let Some(node) = tree.node(id) else { return };
    let effective = ancestor_enabled && node.data.enabled();
    if node.data.is_button() && effective {
        result.push(id);
    }
    for child in &node.children {
        visit_enabled(tree, *child, effective, result);
    }
}

fn is_enabled_button(tree: &Tree, target: NodeId) -> bool {
    enabled_buttons(tree).contains(&target)
}

fn shortcut_target(tree: &Tree, shortcut: &ShortcutKey) -> Option<NodeId> {
    enabled_buttons(tree).into_iter().find(|id| {
        tree.node(*id)
            .is_some_and(|node| node.data.shortcuts().contains(shortcut))
    })
}

fn traverse_linear(
    enabled: &[NodeId],
    focused: Option<NodeId>,
    direction: isize,
) -> Option<NodeId> {
    if enabled.is_empty() {
        return None;
    }
    let Some(position) = focused.and_then(|id| enabled.iter().position(|item| *item == id)) else {
        return if direction > 0 {
            enabled.first()
        } else {
            enabled.last()
        }
        .copied();
    };
    position
        .checked_add_signed(direction)
        .and_then(|index| enabled.get(index))
        .copied()
        .or(focused)
}

#[derive(Clone, Copy)]
enum AxisDirection {
    Left,
    Right,
    Up,
    Down,
}

fn arrow(tree: &Tree, focused: Option<NodeId>, direction: AxisDirection) -> Option<NodeId> {
    let focused = focused?;
    let parents = parent_map(tree);
    let mut descendant = focused;
    let mut ancestor = parents.get(&focused).copied();
    while let Some(parent) = ancestor {
        let node = tree.node(parent)?;
        if let Some(layout) = node.data.box_layout()
            && supports(layout, direction)
            && let Some(position) = node.children.iter().position(|child| *child == descendant)
        {
            let step = grid_step(layout, direction);
            let mut candidate = position.checked_add_signed(step);
            while let Some(index) = candidate.filter(|index| *index < node.children.len()) {
                if grid_direction_valid(layout, position, index, direction)
                    && let Some(target) = first_enabled_in(tree, node.children[index])
                {
                    return Some(target);
                }
                candidate = index.checked_add_signed(step);
            }
            return Some(focused);
        }
        descendant = parent;
        ancestor = parents.get(&parent).copied();
    }
    Some(focused)
}

fn supports(layout: BoxLayout, direction: AxisDirection) -> bool {
    matches!(
        (layout, direction),
        (BoxLayout::Row, AxisDirection::Left | AxisDirection::Right)
            | (BoxLayout::Column, AxisDirection::Up | AxisDirection::Down)
            | (BoxLayout::Grid { .. }, _)
    )
}

fn grid_step(layout: BoxLayout, direction: AxisDirection) -> isize {
    let distance = match (layout, direction) {
        (BoxLayout::Grid { columns }, AxisDirection::Up | AxisDirection::Down) => {
            isize::from(columns)
        }
        _ => 1,
    };
    match direction {
        AxisDirection::Left | AxisDirection::Up => -distance,
        AxisDirection::Right | AxisDirection::Down => distance,
    }
}

fn grid_direction_valid(
    layout: BoxLayout,
    origin: usize,
    candidate: usize,
    direction: AxisDirection,
) -> bool {
    let BoxLayout::Grid { columns } = layout else {
        return true;
    };
    let columns = usize::from(columns);
    match direction {
        AxisDirection::Left | AxisDirection::Right => origin / columns == candidate / columns,
        AxisDirection::Up | AxisDirection::Down => origin % columns == candidate % columns,
    }
}

fn first_enabled_in(tree: &Tree, root: NodeId) -> Option<NodeId> {
    let mut values = Vec::new();
    visit_enabled(tree, root, true, &mut values);
    values.into_iter().next()
}

fn parent_map(tree: &Tree) -> BTreeMap<NodeId, NodeId> {
    let mut parents = BTreeMap::new();
    let mut stack = vec![tree.root()];
    while let Some(parent) = stack.pop() {
        if let Some(node) = tree.node(parent) {
            for child in node.children.iter().rev() {
                parents.insert(*child, parent);
                stack.push(*child);
            }
        }
    }
    parents
}

#[cfg(test)]
mod tests {
    use super::*;
    use youth_tree::{Node, NodeData, ShortcutKey, TreeSnapshot};

    fn id(value: u64) -> NodeId {
        NodeId::new(value).unwrap()
    }

    fn fixture(disable_middle: bool) -> Tree {
        Tree::from_snapshot(
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
                        data: NodeData::Grid {
                            enabled: true,
                            columns: 2,
                        },
                        children: vec![id(3), id(4), id(5), id(6)],
                    },
                    button(3, "1", true, vec![ShortcutKey::Character("1".into())]),
                    button(4, "2", !disable_middle, vec![]),
                    button(5, "C", true, vec![ShortcutKey::Escape]),
                    button(6, "=", true, vec![ShortcutKey::Enter]),
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap()
    }

    fn button(id_value: u64, label: &str, enabled: bool, shortcuts: Vec<ShortcutKey>) -> Node {
        Node {
            id: id(id_value),
            data: NodeData::ShortcutButton {
                label: label.into(),
                enabled,
                shortcuts,
            },
            children: vec![],
        }
    }

    #[test]
    fn tab_is_non_wrapping_and_skips_disabled_buttons() {
        let tree = fixture(true);
        let mut state = InteractionState::default();
        state.reconcile(&tree);
        state.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        assert_eq!(state.focused(), Some(id(3)));
        state.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        assert_eq!(state.focused(), Some(id(5)));
        state.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        state.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        assert_eq!(state.focused(), Some(id(6)));
    }

    #[test]
    fn grid_arrows_are_row_major_and_do_not_wrap() {
        let tree = fixture(false);
        let mut state = InteractionState::default();
        state.focus_pointer_target(&tree, id(3));
        state.key(&tree, LogicalKey::ArrowRight, Modifiers::default(), false);
        assert_eq!(state.focused(), Some(id(4)));
        state.key(&tree, LogicalKey::ArrowRight, Modifiers::default(), false);
        assert_eq!(state.focused(), Some(id(4)));
        state.key(&tree, LogicalKey::ArrowDown, Modifiers::default(), false);
        assert_eq!(state.focused(), Some(id(6)));
        state.key(&tree, LogicalKey::ArrowLeft, Modifiers::default(), false);
        assert_eq!(state.focused(), Some(id(5)));
    }

    #[test]
    fn logical_shortcuts_obey_modifier_repeat_and_default_rules() {
        let tree = fixture(false);
        let mut state = InteractionState::default();
        assert_eq!(
            state
                .key(
                    &tree,
                    LogicalKey::Character('1'),
                    Modifiers::default(),
                    false
                )
                .action,
            Some(SemanticAction::Activate(id(3)))
        );
        assert_eq!(
            state
                .key(
                    &tree,
                    LogicalKey::Character('1'),
                    Modifiers {
                        control: true,
                        ..Modifiers::default()
                    },
                    false,
                )
                .action,
            None
        );
        assert_eq!(
            state
                .key(&tree, LogicalKey::Enter, Modifiers::default(), false)
                .action,
            Some(SemanticAction::Activate(id(6)))
        );
        assert_eq!(
            state
                .key(&tree, LogicalKey::Escape, Modifiers::default(), false)
                .action,
            Some(SemanticAction::Activate(id(5)))
        );
        assert_eq!(
            state
                .key(&tree, LogicalKey::Escape, Modifiers::default(), true)
                .action,
            None
        );
    }

    #[test]
    fn focus_reconciliation_uses_next_then_previous_semantic_position() {
        let first = fixture(false);
        let second = fixture(true);
        let mut state = InteractionState::default();
        state.reconcile(&first);
        state.focus_pointer_target(&first, id(4));
        state.reconcile(&second);
        assert_eq!(state.focused(), Some(id(5)));
    }
}
