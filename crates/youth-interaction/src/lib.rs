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
    Home,
    End,
}

/// A movement key claimed by a focused Editor.
///
/// A3 records routing precedence only. Cursor and selection semantics are
/// intentionally deferred until the host has a real text layout model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorMovement {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// A keyboard operation claimed by a focused Editor.
///
/// Insert, backspace, undo, redo, and paste have host-local mutation effects in
/// A4. Movement, selection, copy, and cut remain routing-only claims.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorInput {
    InsertText(String),
    Backspace,
    MoveCursor(EditorMovement),
    ExtendSelection(EditorMovement),
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InteractionChange {
    pub redraw: bool,
    pub action: Option<SemanticAction>,
    pub editor_input: Option<(NodeId, EditorInput)>,
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
        let enabled = enabled_focusables(tree);
        let mut enabled_actions = Vec::with_capacity(enabled.len().saturating_mul(2));
        for id in enabled {
            enabled_actions.push(SemanticAction::Focus(id));
            if tree.node(id).is_some_and(|node| node.data.is_button()) {
                enabled_actions.push(SemanticAction::Activate(id));
            }
        }
        InteractionSnapshot {
            focused: self.focused,
            enabled_actions,
        }
    }

    pub fn reconcile(&mut self, tree: &Tree) -> InteractionChange {
        let next = enabled_focusables(tree);
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
            editor_input: None,
        }
    }

    pub fn focus_pointer_target(&mut self, tree: &Tree, target: NodeId) -> InteractionChange {
        let previous = self.focused;
        if is_enabled_focusable(tree, target) {
            self.focused = Some(target);
        }
        InteractionChange {
            redraw: previous != self.focused,
            action: (previous != self.focused)
                .then(|| self.focused.map(SemanticAction::Focus))
                .flatten(),
            editor_input: None,
        }
    }

    pub fn key(
        &mut self,
        tree: &Tree,
        key: LogicalKey,
        modifiers: Modifiers,
        repeated: bool,
    ) -> InteractionChange {
        if let Some(editor) = focused_editor(tree, self.focused)
            && key != LogicalKey::Escape
            && let Some(input) = editor_input(&key, modifiers)
        {
            return InteractionChange {
                redraw: false,
                action: None,
                editor_input: Some((editor, input)),
            };
        }
        if repeated {
            return InteractionChange::default();
        }
        let previous = self.focused;
        let activation = match key {
            LogicalKey::Tab => {
                self.focused = traverse_linear(
                    &enabled_focusables(tree),
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
            LogicalKey::Home | LogicalKey::End => None,
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
        self.prior_enabled = enabled_focusables(tree);
        InteractionChange {
            redraw: previous != self.focused,
            action: activation.map(SemanticAction::Activate).or_else(|| {
                (previous != self.focused)
                    .then_some(self.focused)
                    .flatten()
                    .map(SemanticAction::Focus)
            }),
            editor_input: None,
        }
    }
}

fn focused_editor(tree: &Tree, focused: Option<NodeId>) -> Option<NodeId> {
    let focused = focused?;
    tree.node(focused)
        .is_some_and(|node| matches!(node.data, youth_tree::NodeData::Editor { .. }))
        .then_some(focused)
}

fn editor_input(key: &LogicalKey, modifiers: Modifiers) -> Option<EditorInput> {
    let primary = modifiers.control || modifiers.super_key;
    let movement = match key {
        LogicalKey::ArrowLeft => Some(EditorMovement::Left),
        LogicalKey::ArrowRight => Some(EditorMovement::Right),
        LogicalKey::ArrowUp => Some(EditorMovement::Up),
        LogicalKey::ArrowDown => Some(EditorMovement::Down),
        LogicalKey::Home => Some(EditorMovement::Home),
        LogicalKey::End => Some(EditorMovement::End),
        _ => None,
    };
    if let Some(movement) = movement {
        return Some(if modifiers.shift {
            EditorInput::ExtendSelection(movement)
        } else {
            EditorInput::MoveCursor(movement)
        });
    }

    match key {
        LogicalKey::Character(value) if primary && !modifiers.alt => {
            match value.to_ascii_lowercase() {
                'z' if modifiers.shift => Some(EditorInput::Redo),
                'z' => Some(EditorInput::Undo),
                'c' if !modifiers.shift => Some(EditorInput::Copy),
                'x' if !modifiers.shift => Some(EditorInput::Cut),
                'v' if !modifiers.shift => Some(EditorInput::Paste),
                _ => None,
            }
        }
        LogicalKey::Character(value) if !primary => {
            Some(EditorInput::InsertText(value.to_string()))
        }
        LogicalKey::Space => Some(EditorInput::InsertText(" ".to_owned())),
        LogicalKey::Tab => Some(EditorInput::InsertText("\t".to_owned())),
        LogicalKey::Enter => Some(EditorInput::InsertText("\n".to_owned())),
        LogicalKey::Backspace => Some(EditorInput::Backspace),
        LogicalKey::Escape
        | LogicalKey::Character(_)
        | LogicalKey::ArrowLeft
        | LogicalKey::ArrowRight
        | LogicalKey::ArrowUp
        | LogicalKey::ArrowDown
        | LogicalKey::Home
        | LogicalKey::End => None,
    }
}

fn enabled_focusables(tree: &Tree) -> Vec<NodeId> {
    let mut result = Vec::new();
    visit_enabled(tree, tree.root(), true, &mut result);
    result
}

fn visit_enabled(tree: &Tree, id: NodeId, ancestor_enabled: bool, result: &mut Vec<NodeId>) {
    let Some(node) = tree.node(id) else { return };
    let effective = ancestor_enabled && node.data.enabled();
    if node.data.is_focusable() && effective {
        result.push(id);
    }
    for child in &node.children {
        visit_enabled(tree, *child, effective, result);
    }
}

fn is_enabled_focusable(tree: &Tree, target: NodeId) -> bool {
    enabled_focusables(tree).contains(&target)
}

fn shortcut_target(tree: &Tree, shortcut: &ShortcutKey) -> Option<NodeId> {
    enabled_focusables(tree).into_iter().find(|id| {
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

    fn editor_fixture() -> Tree {
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
                            columns: 3,
                        },
                        children: vec![id(3), id(4), id(5)],
                    },
                    button(3, "Before", true, vec![]),
                    Node {
                        id: id(4),
                        data: NodeData::Editor {
                            document_revision: 7,
                            text: "draft".into(),
                        },
                        children: vec![],
                    },
                    button(5, "Cancel", true, vec![ShortcutKey::Escape]),
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap()
    }

    fn primary(shift: bool) -> Modifiers {
        Modifiers {
            shift,
            control: true,
            ..Modifiers::default()
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

    #[test]
    fn focused_editor_claims_text_deletion_movement_and_command_keys() {
        let tree = editor_fixture();
        let cases = [
            (
                LogicalKey::Character('a'),
                Modifiers::default(),
                EditorInput::InsertText("a".into()),
            ),
            (
                LogicalKey::Space,
                Modifiers::default(),
                EditorInput::InsertText(" ".into()),
            ),
            (
                LogicalKey::Backspace,
                Modifiers::default(),
                EditorInput::Backspace,
            ),
            (
                LogicalKey::Enter,
                Modifiers::default(),
                EditorInput::InsertText("\n".into()),
            ),
            (
                LogicalKey::Tab,
                Modifiers::default(),
                EditorInput::InsertText("\t".into()),
            ),
            (
                LogicalKey::ArrowLeft,
                Modifiers::default(),
                EditorInput::MoveCursor(EditorMovement::Left),
            ),
            (
                LogicalKey::ArrowRight,
                Modifiers::default(),
                EditorInput::MoveCursor(EditorMovement::Right),
            ),
            (
                LogicalKey::ArrowUp,
                Modifiers::default(),
                EditorInput::MoveCursor(EditorMovement::Up),
            ),
            (
                LogicalKey::ArrowDown,
                Modifiers::default(),
                EditorInput::MoveCursor(EditorMovement::Down),
            ),
            (
                LogicalKey::Home,
                Modifiers::default(),
                EditorInput::MoveCursor(EditorMovement::Home),
            ),
            (
                LogicalKey::End,
                Modifiers::default(),
                EditorInput::MoveCursor(EditorMovement::End),
            ),
            (
                LogicalKey::ArrowLeft,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                EditorInput::ExtendSelection(EditorMovement::Left),
            ),
            (
                LogicalKey::ArrowRight,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                EditorInput::ExtendSelection(EditorMovement::Right),
            ),
            (
                LogicalKey::ArrowUp,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                EditorInput::ExtendSelection(EditorMovement::Up),
            ),
            (
                LogicalKey::ArrowDown,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                EditorInput::ExtendSelection(EditorMovement::Down),
            ),
            (
                LogicalKey::Home,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                EditorInput::ExtendSelection(EditorMovement::Home),
            ),
            (
                LogicalKey::End,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                EditorInput::ExtendSelection(EditorMovement::End),
            ),
            (
                LogicalKey::Character('z'),
                primary(false),
                EditorInput::Undo,
            ),
            (LogicalKey::Character('z'), primary(true), EditorInput::Redo),
            (
                LogicalKey::Character('c'),
                primary(false),
                EditorInput::Copy,
            ),
            (LogicalKey::Character('x'), primary(false), EditorInput::Cut),
            (
                LogicalKey::Character('v'),
                primary(false),
                EditorInput::Paste,
            ),
            (
                LogicalKey::Character('z'),
                Modifiers {
                    super_key: true,
                    ..Modifiers::default()
                },
                EditorInput::Undo,
            ),
        ];

        for (key, modifiers, expected) in cases {
            let mut state = InteractionState::default();
            let focus = state.focus_pointer_target(&tree, id(4));
            assert_eq!(focus.action, Some(SemanticAction::Focus(id(4))));
            let change = state.key(&tree, key, modifiers, false);
            assert_eq!(change.editor_input, Some((id(4), expected)));
            assert_eq!(change.action, None);
            assert_eq!(state.focused(), Some(id(4)));
        }
    }

    #[test]
    fn focused_editor_does_not_claim_escape() {
        let tree = editor_fixture();
        let mut state = InteractionState::default();
        state.focus_pointer_target(&tree, id(4));

        let change = state.key(&tree, LogicalKey::Escape, Modifiers::default(), false);

        assert_eq!(change.editor_input, None);
        assert_eq!(change.action, Some(SemanticAction::Activate(id(5))));
        assert_eq!(state.focused(), Some(id(5)));
    }

    #[test]
    fn button_focus_keeps_existing_shortcut_and_navigation_behavior() {
        let tree = fixture(false);
        let mut state = InteractionState::default();
        state.focus_pointer_target(&tree, id(3));

        let shortcut = state.key(
            &tree,
            LogicalKey::Character('1'),
            Modifiers::default(),
            false,
        );
        assert_eq!(shortcut.editor_input, None);
        assert_eq!(shortcut.action, Some(SemanticAction::Activate(id(3))));

        let navigation = state.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        assert_eq!(navigation.editor_input, None);
        assert_eq!(navigation.action, Some(SemanticAction::Focus(id(4))));
    }

    #[test]
    fn editor_participates_in_pointer_tab_and_arrow_focus_discovery() {
        let tree = editor_fixture();

        let mut pointer = InteractionState::default();
        assert_eq!(
            pointer.focus_pointer_target(&tree, id(4)).action,
            Some(SemanticAction::Focus(id(4)))
        );

        let mut tab = InteractionState::default();
        tab.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        assert_eq!(tab.focused(), Some(id(3)));
        tab.key(&tree, LogicalKey::Tab, Modifiers::default(), false);
        assert_eq!(tab.focused(), Some(id(4)));

        let mut arrow = InteractionState::default();
        arrow.focus_pointer_target(&tree, id(3));
        let change = arrow.key(&tree, LogicalKey::ArrowRight, Modifiers::default(), false);
        assert_eq!(change.action, Some(SemanticAction::Focus(id(4))));
        assert_eq!(arrow.focused(), Some(id(4)));
    }
}
