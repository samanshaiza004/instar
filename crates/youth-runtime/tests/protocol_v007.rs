//! Protocol 0.0.7 modifier-aware shortcut plumbing and coexistence regressions.
//!
//! SCRATCHPAD-F001: no app could declare a real `Primary+Character` shortcut
//! before 0.0.7 because `youth_tree::ShortcutKey` carried no modifier field
//! and the host's fallback shortcut matching excluded any Control/Super-held
//! character press outright. This file proves the fix end to end: mount a
//! real 0.0.7 guest that declares a `Primary+S` Save shortcut alongside a
//! focused Editor, and drive it through the same `youth_interaction` routing
//! the desktop host uses.

mod common;

use common::{counter_component, test_component};
use youth_interaction::{InteractionState, LogicalKey, Modifiers, SemanticAction};
use youth_runtime::{AppLifecycle, YouthApp};
use youth_tree::{Limits, NodeData, NodeId, Tree};

fn primary() -> Modifiers {
    Modifiers {
        control: true,
        ..Modifiers::default()
    }
}

#[test]
fn v007_shortcut_component_mounts_and_reports_the_new_world() {
    let mut app = YouthApp::load(test_component("youth-shortcut-primary-v007"))
        .expect("v0.0.7 shortcut component loads");
    assert_eq!(app.inspect().world, "youth:app/application@0.0.7");

    let mounted = app.mount().expect("v0.0.7 shortcut component mounts");
    assert_eq!(mounted.revision, 0);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);

    let editor = NodeId::new(2).unwrap();
    let save = NodeId::new(4).unwrap();
    assert!(matches!(
        mounted
            .nodes
            .iter()
            .find(|node| node.id == editor)
            .map(|node| &node.data),
        Some(NodeData::Editor { .. })
    ));
    assert_eq!(
        mounted
            .nodes
            .iter()
            .find(|node| node.id == save)
            .and_then(|node| node.data.shortcuts().first())
            .cloned(),
        Some(youth_tree::Shortcut::new(
            youth_tree::ShortcutKey::Character("s".into()),
            youth_tree::ShortcutModifiers::primary(),
        ))
    );

    let canonical_snapshot = app.tree().unwrap().to_snapshot();
    let round_trip = Tree::from_snapshot(canonical_snapshot.clone(), &Limits::default()).unwrap();
    assert_eq!(round_trip.to_snapshot(), canonical_snapshot);
}

#[test]
fn focused_editor_declines_primary_s_and_the_fallthrough_activates_save_with_one_guest_call() {
    let mut app = YouthApp::load(test_component("youth-shortcut-primary-v007"))
        .expect("v0.0.7 shortcut component loads");
    let mounted = app.mount().expect("v0.0.7 shortcut component mounts");
    let tree = Tree::from_snapshot(mounted, &Limits::default()).unwrap();

    let editor = NodeId::new(2).unwrap();
    let save = NodeId::new(4).unwrap();

    let mut interaction = InteractionState::default();
    interaction.focus_pointer_target(&tree, editor);
    assert_eq!(interaction.focused(), Some(editor));

    let before_calls = app.inspect().guest_call_count;

    // Plain "s": the editor claims it as ordinary text input. No semantic
    // action fires, so no guest turn happens -- the call count is unchanged.
    let plain = interaction.key(
        &tree,
        LogicalKey::Character('s'),
        Modifiers::default(),
        false,
    );
    assert!(plain.action.is_none());
    assert!(
        plain.editor_input.is_some(),
        "plain \"s\" must still be ordinary text input while the editor is focused"
    );
    assert_eq!(
        app.inspect().guest_call_count,
        before_calls,
        "a locally-claimed editor keystroke must not reach the guest"
    );

    // Primary+S: the editor declines it (no InsertText/Undo/Redo/Copy/Cut/
    // Paste mapping for a bare "s"+primary), so it falls through to shortcut
    // routing and activates Save -- not a literal-text insertion.
    let shortcut = interaction.key(&tree, LogicalKey::Character('s'), primary(), false);
    assert_eq!(
        shortcut.editor_input, None,
        "a focused editor must not insert literal text for Primary+S"
    );
    assert_eq!(shortcut.action, Some(SemanticAction::Activate(save)));

    let receipt = app.activate(save).expect("Save activation commits");
    assert!(receipt.committed);
    assert_eq!(
        app.inspect().guest_call_count,
        before_calls + 1,
        "activating Save must reach the guest exactly once"
    );
    let status = NodeId::new(3).unwrap();
    assert_eq!(
        app.tree()
            .unwrap()
            .node(status)
            .and_then(|node| node.data.text_value()),
        Some("saved"),
        "Save command must actually run"
    );
}

#[test]
fn primary_s_activates_save_while_the_editor_is_unfocused() {
    let mut app = YouthApp::load(test_component("youth-shortcut-primary-v007"))
        .expect("v0.0.7 shortcut component loads");
    let mounted = app.mount().expect("v0.0.7 shortcut component mounts");
    let tree = Tree::from_snapshot(mounted, &Limits::default()).unwrap();
    let save = NodeId::new(4).unwrap();

    let mut interaction = InteractionState::default();
    interaction.focus_pointer_target(&tree, save);
    assert_eq!(interaction.focused(), Some(save));

    let change = interaction.key(&tree, LogicalKey::Character('s'), primary(), false);
    assert_eq!(change.editor_input, None);
    assert_eq!(change.action, Some(SemanticAction::Activate(save)));
}

#[test]
fn a_bare_primary_modifier_with_no_character_produces_no_shortcut_target() {
    // `youth_interaction::LogicalKey` has no bare-modifier variant: a
    // Control/Super press alone can only ever surface to the host as
    // `WindowEvent::ModifiersChanged`, which never calls `InteractionState::
    // key` at all (see `crates/youth-desktop/src/native.rs`). There is
    // therefore no way to construct a "bare modifier" key press through this
    // API -- the closest reachable case is a held primary modifier with a
    // key that maps to no declared shortcut, which must still produce no
    // action and require no guest turn.
    let mut app = YouthApp::load(test_component("youth-shortcut-primary-v007"))
        .expect("v0.0.7 shortcut component loads");
    let mounted = app.mount().expect("v0.0.7 shortcut component mounts");
    let tree = Tree::from_snapshot(mounted, &Limits::default()).unwrap();
    let editor = NodeId::new(2).unwrap();

    let mut interaction = InteractionState::default();
    interaction.focus_pointer_target(&tree, editor);
    let before_calls = app.inspect().guest_call_count;

    let change = interaction.key(&tree, LogicalKey::Character('q'), primary(), false);
    assert_eq!(change.action, None);
    assert_eq!(change.editor_input, None);
    assert_eq!(app.inspect().guest_call_count, before_calls);
}

#[test]
fn v002_through_v007_components_mount_without_cross_version_drift() {
    let fixtures = [
        (
            counter_component(),
            "youth:app/application@0.0.2",
            "v0.0.2 counter",
        ),
        (
            test_component("youth-legacy-v003"),
            "youth:app/application@0.0.3",
            "v0.0.3 legacy",
        ),
        (
            test_component("youth-time-stub"),
            "youth:app/application@0.0.4",
            "v0.0.4 time",
        ),
        // youth-sdk-tally is a local-path youth-sdk fixture, so it always
        // tracks whichever protocol the SDK crate in this workspace
        // currently targets -- 0.0.9, now that the grow field exists.
        (
            test_component("youth-sdk-tally"),
            "youth:app/application@0.0.9",
            "current SDK tally",
        ),
        (
            test_component("youth-editor-v006"),
            "youth:app/application@0.0.6",
            "v0.0.6 editor",
        ),
        (
            test_component("youth-shortcut-primary-v007"),
            "youth:app/application@0.0.7",
            "v0.0.7 shortcut",
        ),
    ];

    for (component, expected_world, label) in fixtures {
        let mut app =
            YouthApp::load(component).unwrap_or_else(|error| panic!("{label} loads: {error}"));
        assert_eq!(app.inspect().world, expected_world, "{label}");
        app.mount()
            .unwrap_or_else(|error| panic!("{label} mounts: {error}"));
        assert_eq!(app.lifecycle(), AppLifecycle::Mounted, "{label}");
        let canonical_snapshot = app.tree().unwrap().to_snapshot();
        let round_trip = Tree::from_snapshot(canonical_snapshot.clone(), &Limits::default())
            .unwrap_or_else(|error| panic!("{label} snapshot revalidates: {error}"));
        assert_eq!(round_trip.to_snapshot(), canonical_snapshot, "{label}");
    }
}
