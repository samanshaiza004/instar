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

fn wait_for_revision(
    harness: &mut instar_shell::test_harness::RuntimeHarness,
    previous: u64,
) -> u64 {
    for _ in 0..150 {
        if let Some(revision) = harness.surface_revision(SURFACE)
            && revision > previous
        {
            return revision;
        }
        harness.wait(Duration::from_millis(20));
    }
    panic!(
        "Scratchpad did not replace its Surface scene after revision {previous}: {:?}",
        harness.surface_revision(SURFACE)
    );
}

fn marker_color(
    harness: &instar_shell::test_harness::RuntimeHarness,
) -> Option<instar_paint::Color> {
    harness
        .scene()?
        .commands
        .iter()
        .find_map(|command| match command {
            instar_paint::PaintCommand::FillRect { rect, color }
                if rect.x == 612 && rect.y == 448 =>
            {
                Some(*color)
            }
            _ => None,
        })
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

#[test]
fn pointer_selection_and_wheel_stay_guest_local_and_reuse_bounded_layouts() {
    let component = std::fs::read(env!("SCRATCHPAD_WASM")).expect("Scratchpad component");
    let mut harness = launch_component(component, metrics());
    let opening_revision = wait_for_scene(&mut harness);
    let tree_revision = harness.tree_revision();

    harness.click_at(320.0, 240.0);
    for _ in 0..100 {
        harness.wait(Duration::from_millis(10));
    }
    let focused_revision = harness
        .surface_revision(SURFACE)
        .unwrap_or(opening_revision);
    assert_eq!(harness.focused(), Some(SURFACE));
    harness.send_output(WindowOutput::ImeCommit {
        window_id: WINDOW,
        text: (0..80)
            .map(|line| format!("line {line}: abcdefghijklmnopqrstuvwxyz\n"))
            .collect(),
    });
    let document_revision = wait_for_revision(&mut harness, focused_revision);
    let lengths = harness
        .surface_layout_source_lengths(SURFACE)
        .expect("retained Surface scene layouts");
    assert!(!lengths.is_empty());
    assert!(lengths.iter().all(|length| *length <= 4096));
    assert!(
        lengths.len() <= 28,
        "visible rows plus overscan only: {lengths:?}"
    );

    let before_selection_commands = harness.scene().expect("document scene").commands.len();
    let before_pointer_revision = harness.surface_revision(SURFACE).unwrap();
    harness.move_to(8.0, 40.0);
    harness.button(winit::event::ElementState::Pressed);
    for _ in 0..200 {
        harness.wait(Duration::from_millis(10));
    }
    assert!(harness.surface_revision(SURFACE).unwrap_or(0) >= before_pointer_revision);
    harness.move_to(600.0, 40.0);
    for _ in 0..200 {
        harness.wait(Duration::from_millis(10));
    }
    harness.button(winit::event::ElementState::Released);
    let selected_revision = wait_for_revision(&mut harness, document_revision);
    assert!(selected_revision > document_revision);
    assert_eq!(
        marker_color(&harness),
        Some(instar_paint::Color {
            r: 240,
            g: 170,
            b: 70,
            a: 255,
        })
    );
    assert!(
        harness.scene().expect("selected scene").commands.len() > before_selection_commands,
        "selection geometry should be emitted through the retained scene"
    );

    let wheel_revision = harness
        .surface_revision(SURFACE)
        .unwrap_or(selected_revision);
    harness.wheel(320.0, 120.0, 3.0);
    let scrolled_revision = wait_for_revision(&mut harness, wheel_revision);
    assert!(scrolled_revision > wheel_revision);
    assert_eq!(harness.tree_revision(), tree_revision);
    assert_eq!(harness.focused(), Some(SURFACE));
}

#[test]
fn malicious_multiline_preedit_is_viewport_bounded_in_the_real_component() {
    let component = std::fs::read(env!("SCRATCHPAD_WASM")).expect("Scratchpad component");
    let mut harness = launch_component(component, metrics());
    wait_for_scene(&mut harness);
    harness.click_at(320.0, 240.0);
    // The neutral Surface event itself is bounded; this still supplies
    // hundreds of arbitrary hard rows and proves shaping stays viewport-only.
    let preedit = (0..200)
        .map(|line| format!("composition {line}\n"))
        .collect();
    let mut effects = harness.send_output(WindowOutput::ImePreedit {
        window_id: WINDOW,
        text: preedit,
        cursor_range: Some((0, 0)),
    });
    for _ in 0..150 {
        effects.extend(harness.wait(Duration::from_millis(20)));
    }
    let lengths = harness
        .surface_layout_source_lengths(SURFACE)
        .unwrap_or_else(|| {
            panic!(
                "retained preedit scene layouts; scene={:?}; effects={effects:?}",
                harness.scene(),
            )
        });
    assert!(lengths.len() <= 28, "preedit shaping is viewport-bounded");
    assert!(lengths.iter().all(|length| *length <= 4096));
}
