//! Joined proof for the real guest-owned Scratchpad component.
//!
//! The assertions deliberately cross the production boundary: the guest's
//! input mutates its Crop document, asks the host for TextLayouts, submits an
//! independent Surface scene, and the host lowers that scene to pixels. No
//! host document or editor state is consulted.

use std::time::Duration;

use instar_shell::{Presenter, test_harness::launch_component};
use instar_ui::NodeKey;
use instar_window::{LogicalSize, WindowId, WindowMetricsChanged, WindowOutput};

const WINDOW: WindowId = WindowId::from_raw(1);
const SURFACE: NodeKey = NodeKey::first(7);

fn metrics() -> WindowMetricsChanged {
    WindowMetricsChanged {
        window_id: WINDOW,
        logical_size: LogicalSize {
            width: 640.0,
            height: 480.0,
        },
        physical_size: instar_window::PhysicalSize {
            width: 640,
            height: 480,
        },
        scale_factor: 1.0,
    }
}

fn wait_for_scene(harness: &mut instar_shell::test_harness::RuntimeHarness) -> u64 {
    let mut effects = Vec::new();
    for _ in 0..100 {
        if let Some(revision) = harness.surface_revision(SURFACE) {
            return revision;
        }
        effects.extend(harness.wait(Duration::from_millis(20)));
    }
    panic!("Scratchpad never submitted its opening Surface scene: {effects:?}");
}

#[test]
fn real_guest_input_reaches_text_layout_surface_and_pixels() {
    let component = std::fs::read(env!("SCRATCHPAD_WASM"))
        .expect("the Scratchpad component is built by the shell build script");
    let mut harness = launch_component(component, metrics());
    let opening_revision = wait_for_scene(&mut harness);
    assert_eq!(opening_revision, 1);
    assert!(matches!(
        harness
            .read_retained_tree()
            .find(SURFACE)
            .map(|node| &node.kind),
        Some(instar_ui::NodeKind::Surface { .. })
    ));
    let tree_revision = harness.tree_revision();

    let mut presenter = Presenter::new(instar_paint::PhysicalSize {
        width: 640,
        height: 480,
    })
    .expect("headless presenter");
    let before = presenter
        .render(harness.scene().expect("opening scene"))
        .expect("opening scene renders")
        .to_vec();
    let before_ink = before
        .chunks_exact(4)
        .filter(|pixel| pixel[0..3] != [20, 20, 24])
        .count();
    assert!(
        before_ink > 0,
        "opening Surface should rasterize visible scene commands"
    );
    assert!(
        harness
            .scene()
            .is_some_and(|scene| scene.commands.len() >= 3)
    );

    let (x, y) = harness.screen_point_of(SURFACE);
    harness.click_at(x, y);
    assert_eq!(
        harness.focused(),
        Some(SURFACE),
        "pointer press focuses Surface"
    );
    // Let the pointer event reach the guest first. Its resulting present
    // configures the native text-input session after host focus is applied.
    for _ in 0..100 {
        harness.wait(Duration::from_millis(10));
        if harness.surface_revision(SURFACE).unwrap_or(1) > opening_revision {
            break;
        }
    }
    let commit_revision = harness
        .surface_revision(SURFACE)
        .unwrap_or(opening_revision);
    let mut effects = harness.send_output(WindowOutput::ImeCommit {
        window_id: WINDOW,
        text: "hello\nworld".to_owned(),
    });

    for _ in 0..100 {
        effects.extend(harness.wait(Duration::from_millis(20)));
    }
    let changed = harness.surface_revision(SURFACE).unwrap_or(commit_revision) > commit_revision;
    assert!(
        changed,
        "guest commit must replace the retained Surface scene: {effects:?}"
    );
    assert_eq!(
        harness.tree_revision(),
        tree_revision,
        "scene updates do not rebuild semantic UI"
    );

    let after = presenter
        .render(harness.scene().expect("updated scene"))
        .expect("updated scene renders")
        .to_vec();
    assert!(
        harness
            .scene()
            .is_some_and(|scene| scene.commands.len() >= 3)
    );
    assert_ne!(
        before, after,
        "guest-owned document input must change pixels"
    );
}

#[test]
fn empty_preedit_is_delivered_before_commit_without_losing_target() {
    let component = std::fs::read(env!("SCRATCHPAD_WASM")).expect("Scratchpad component");
    let mut harness = launch_component(component, metrics());
    wait_for_scene(&mut harness);
    let (x, y) = harness.screen_point_of(SURFACE);
    harness.click_at(x, y);
    harness.send_output(WindowOutput::ImePreedit {
        window_id: WINDOW,
        text: "a\nb".to_owned(),
        cursor_range: Some((1, 1)),
    });
    harness.send_output(WindowOutput::ImePreedit {
        window_id: WINDOW,
        text: String::new(),
        cursor_range: None,
    });
    harness.send_output(WindowOutput::ImeCommit {
        window_id: WINDOW,
        text: "a\nb".to_owned(),
    });
    let mut effects = Vec::new();
    for _ in 0..100 {
        effects.extend(harness.wait(Duration::from_millis(20)));
    }
    assert!(
        harness.surface_revision(SURFACE).unwrap_or(0) >= 2,
        "preedit/commit should replace the scene: {effects:?}"
    );
}
