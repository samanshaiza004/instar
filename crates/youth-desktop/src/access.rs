//! Maps Youth's semantic tree to an AccessKit accessibility tree.
//!
//! Every `youth_tree::NodeId` becomes the numerically identical
//! `accesskit::NodeId`, so no lookup table is needed between the two id
//! spaces. An Editor's own node additionally gets `TextRun`-role children
//! (allocated by Parley via [`youth_runtime::EditorAccessibility`]) in a
//! disjoint namespace -- see
//! `youth_runtime::editor_session::editor_accessibility_snapshots`'s docs
//! for the id scheme.

use std::collections::HashMap;

use accesskit::{
    Action, Node, NodeId as AccessNodeId, Rect as AccessRect, Role, Tree as AccessTree, TreeId,
    TreeUpdate,
};
use youth_runtime::{EditorAccessibility, PresentationReader, resolve_countdown_display};
use youth_tree::{NodeData, NodeId, Tree as SemanticTree};

use crate::{InteractionState, LayoutSnapshot};

/// Maps a `youth_tree::NodeId` to the `accesskit::NodeId` used for the same
/// node throughout this module.
#[must_use]
pub fn access_node_id(id: NodeId) -> AccessNodeId {
    AccessNodeId(id.get())
}

struct BuildContext<'a> {
    tree: &'a SemanticTree,
    layout: &'a LayoutSnapshot,
    interaction: &'a InteractionState,
    presentation: Option<&'a PresentationReader>,
    scale_factor: f64,
    scroll_offsets: &'a HashMap<NodeId, f32>,
}

/// Builds a full-window accessibility tree from the current semantic tree,
/// layout, focus state, and live Editor presentations -- read-only,
/// host-local, and cheap enough to build fresh on every accessibility sync
/// rather than incrementally diffed.
#[must_use]
pub fn build_tree_update(
    tree: &SemanticTree,
    layout: &LayoutSnapshot,
    interaction: &InteractionState,
    presentation: Option<&PresentationReader>,
    scale_factor: f64,
    scroll_offsets: &HashMap<NodeId, f32>,
) -> TreeUpdate {
    let context = BuildContext {
        tree,
        layout,
        interaction,
        presentation,
        scale_factor,
        scroll_offsets,
    };
    let mut nodes = Vec::new();
    visit(&context, tree.root(), &mut nodes);
    let focus = context
        .interaction
        .focused()
        .map(access_node_id)
        .unwrap_or_else(|| access_node_id(tree.root()));
    TreeUpdate {
        nodes,
        tree: Some(AccessTree::new(access_node_id(tree.root()))),
        tree_id: TreeId::ROOT,
        focus,
    }
}

fn visit(context: &BuildContext<'_>, id: NodeId, nodes: &mut Vec<(AccessNodeId, Node)>) {
    let Some(semantic) = context.tree.node(id) else {
        return;
    };
    let bounds = physical_bounds(context, id);

    if matches!(semantic.data, NodeData::Editor { .. }) {
        build_editor_node(context, id, bounds, nodes);
        for &child in &semantic.children {
            visit(context, child, nodes);
        }
        return;
    }

    let mut node = match &semantic.data {
        NodeData::Root => Node::new(Role::Window),
        NodeData::Box { .. } | NodeData::Row { .. } | NodeData::Grid { .. } => {
            Node::new(Role::GenericContainer)
        }
        NodeData::Text { value } | NodeData::AlignedText { value, .. } => {
            let mut node = Node::new(Role::Label);
            node.set_value(value.clone());
            node
        }
        NodeData::Countdown {
            schedule,
            precision,
            format,
        }
        | NodeData::AlignedCountdown {
            schedule,
            precision,
            format,
            ..
        } => {
            let mut node = Node::new(Role::Label);
            let value = context
                .presentation
                .map_or_else(String::new, |presentation| {
                    let record = presentation.schedule(schedule.id);
                    let now = presentation.now_epoch_millis();
                    resolve_countdown_display(*schedule, *precision, *format, record.as_ref(), now)
                });
            node.set_value(value);
            node
        }
        NodeData::Button { label, enabled } | NodeData::ShortcutButton { label, enabled, .. } => {
            let mut node = Node::new(Role::Button);
            node.set_label(label.clone());
            if *enabled {
                node.add_action(Action::Focus);
                node.add_action(Action::Click);
            }
            node
        }
        NodeData::Editor { .. } => unreachable!("Editor nodes are handled above"),
    };
    if let Some(bounds) = bounds {
        node.set_bounds(bounds);
    }
    node.set_children(
        semantic
            .children
            .iter()
            .copied()
            .map(access_node_id)
            .collect::<Vec<_>>(),
    );
    nodes.push((access_node_id(id), node));

    for &child in &semantic.children {
        visit(context, child, nodes);
    }
}

fn physical_bounds(context: &BuildContext<'_>, id: NodeId) -> Option<AccessRect> {
    let rect = context.layout.nodes.get(&id)?.bounds;
    Some(AccessRect {
        x0: rect.x * context.scale_factor,
        y0: rect.y * context.scale_factor,
        x1: (rect.x + rect.width) * context.scale_factor,
        y1: (rect.y + rect.height) * context.scale_factor,
    })
}

/// Builds an Editor's own node plus its `TextRun` children, translating
/// each child's bounds from the engine's own rect/scroll-independent space
/// into window-physical coordinates -- the same transform
/// `draw_editor_presentation` applies when painting, so a screen reader's
/// notion of where the text is matches what's actually on screen.
fn build_editor_node(
    context: &BuildContext<'_>,
    id: NodeId,
    bounds: Option<AccessRect>,
    nodes: &mut Vec<(AccessNodeId, Node)>,
) {
    let Some(EditorAccessibility {
        mut node,
        extra_nodes,
    }) = context
        .presentation
        .and_then(|presentation| presentation.editor_accessibility(id))
    else {
        // No live session synced yet (e.g. the very first accessibility
        // update before the runtime's presentation cache has populated):
        // a minimal, still-focusable node rather than omitting it.
        let mut node = Node::new(Role::MultilineTextInput);
        node.add_action(Action::Focus);
        if let Some(bounds) = bounds {
            node.set_bounds(bounds);
        }
        nodes.push((access_node_id(id), node));
        return;
    };

    node.add_action(Action::Focus);
    if let Some(bounds) = bounds {
        node.set_bounds(bounds);
    }
    nodes.push((access_node_id(id), node));

    let scroll_offset_y =
        f64::from(context.scroll_offsets.get(&id).copied().unwrap_or(0.0)) * context.scale_factor;
    let origin_x = bounds.map_or(0.0, |bounds| bounds.x0);
    let origin_y = bounds.map_or(0.0, |bounds| bounds.y0) - scroll_offset_y;
    for (child_id, mut child) in extra_nodes {
        if let Some(child_bounds) = child.bounds() {
            child.set_bounds(translate_bounds(child_bounds, origin_x, origin_y));
        }
        nodes.push((child_id, child));
    }
}

/// Shifts `bounds` (in the engine's own rect/scroll-independent space) by
/// `(origin_x, origin_y)`, matching `draw_editor_presentation`'s paint-time
/// origin math exactly: it adds the physical rect origin (already
/// scroll-adjusted) directly to unscaled engine-space coordinates, with no
/// further DPI scaling of the text content itself.
fn translate_bounds(bounds: AccessRect, origin_x: f64, origin_y: f64) -> AccessRect {
    AccessRect {
        x0: origin_x + bounds.x0,
        y0: origin_y + bounds.y0,
        x1: origin_x + bounds.x1,
        y1: origin_y + bounds.y1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{LogicalSize, layout};
    use youth_tree::{Node, TreeSnapshot};

    fn id(value: u64) -> NodeId {
        NodeId::new(value).unwrap()
    }

    fn fixture() -> SemanticTree {
        SemanticTree::from_snapshot(
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
                        data: NodeData::Box { enabled: true },
                        children: vec![id(3), id(4), id(5), id(6)],
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
                    Node {
                        id: id(5),
                        data: NodeData::Button {
                            label: "Disabled".into(),
                            enabled: false,
                        },
                        children: vec![],
                    },
                    Node {
                        id: id(6),
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
        .unwrap()
    }

    fn build(tree: &SemanticTree, interaction: &InteractionState, scale_factor: f64) -> TreeUpdate {
        let layout = layout(tree, LogicalSize::new(320.0, 180.0).unwrap()).unwrap();
        build_tree_update(
            tree,
            &layout,
            interaction,
            None,
            scale_factor,
            &HashMap::new(),
        )
    }

    #[test]
    fn every_semantic_node_becomes_one_accessibility_node_with_matching_id() {
        let tree = fixture();
        let update = build(&tree, &InteractionState::default(), 1.0);

        let ids: Vec<AccessNodeId> = update.nodes.iter().map(|(id, _)| *id).collect();
        for youth_id in [1_u64, 2, 3, 4, 5, 6] {
            assert!(
                ids.contains(&AccessNodeId(youth_id)),
                "node {youth_id} must have a matching accessibility node"
            );
        }
        assert_eq!(update.tree.as_ref().unwrap().root, access_node_id(id(1)));
    }

    #[test]
    fn roles_values_and_actions_match_each_node_kind() {
        let tree = fixture();
        let update = build(&tree, &InteractionState::default(), 1.0);
        let node = |target: u64| {
            update
                .nodes
                .iter()
                .find(|(node_id, _)| *node_id == AccessNodeId(target))
                .map(|(_, node)| node)
                .unwrap()
        };

        assert_eq!(node(1).role(), Role::Window);
        assert_eq!(node(2).role(), Role::GenericContainer);

        let text = node(3);
        assert_eq!(text.role(), Role::Label);
        assert_eq!(text.value(), Some("Count: 0"));

        let enabled_button = node(4);
        assert_eq!(enabled_button.role(), Role::Button);
        assert_eq!(enabled_button.label(), Some("Increment"));
        assert!(enabled_button.supports_action(Action::Focus));
        assert!(enabled_button.supports_action(Action::Click));

        let disabled_button = node(5);
        assert!(
            !disabled_button.supports_action(Action::Focus),
            "a disabled button must not be reachable via accessibility focus"
        );
        assert!(!disabled_button.supports_action(Action::Click));
    }

    #[test]
    fn an_editor_with_no_synced_presentation_falls_back_to_a_focusable_leaf_node() {
        let tree = fixture();
        let update = build(&tree, &InteractionState::default(), 1.0);
        let editor = update
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == AccessNodeId(6))
            .map(|(_, node)| node)
            .unwrap();

        assert_eq!(editor.role(), Role::MultilineTextInput);
        assert!(editor.supports_action(Action::Focus));
        assert!(
            editor.children().is_empty(),
            "with no live session synced yet, there are no TextRun children to attach"
        );
    }

    #[test]
    fn focus_follows_interaction_state_and_defaults_to_root() {
        let tree = fixture();
        let unfocused = build(&tree, &InteractionState::default(), 1.0);
        assert_eq!(
            unfocused.focus,
            access_node_id(id(1)),
            "with nothing focused, the accessibility focus falls back to the root"
        );

        let mut interaction = InteractionState::default();
        interaction.focus_pointer_target(&tree, id(4));
        let focused = build(&tree, &interaction, 1.0);
        assert_eq!(focused.focus, AccessNodeId(4));
    }

    #[test]
    fn bounds_scale_with_the_window_scale_factor() {
        let tree = fixture();
        let update = build(&tree, &InteractionState::default(), 2.0);
        let root = update
            .nodes
            .iter()
            .find(|(node_id, _)| *node_id == AccessNodeId(1))
            .map(|(_, node)| node)
            .unwrap();
        let bounds = root.bounds().expect("the root has layout bounds");
        assert_eq!(bounds.x1, 320.0 * 2.0);
        assert_eq!(bounds.y1, 180.0 * 2.0);
    }

    #[test]
    fn translate_bounds_shifts_by_the_given_origin() {
        let bounds = AccessRect {
            x0: 1.0,
            y0: 2.0,
            x1: 5.0,
            y1: 9.0,
        };
        let translated = translate_bounds(bounds, 10.0, 20.0);
        assert_eq!(
            translated,
            AccessRect {
                x0: 11.0,
                y0: 22.0,
                x1: 15.0,
                y1: 29.0,
            }
        );
    }
}
