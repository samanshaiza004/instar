//! The layering rule from docs/PHASE-1.md, enforced rather than documented.
//!
//! > `instar-window` normalizes OS coordinates; `instar-ui` speaks logical
//! > coordinates; `instar-host` is the only layer that bridges logical
//! > presentation to physical rendering.
//!
//! Architecture rules that live only in prose decay, usually via a plausible
//! one-line convenience ("the window layer already knows where the click was,
//! it may as well ask which node it hit"). This test is what makes that
//! particular shortcut fail loudly instead.

use std::process::Command;

/// `instar-window` must not depend on `instar-ui`, at any depth.
///
/// A dependency edge here would mean windowing knows about node identity,
/// tree revisions, and hit-testing — at which point the windowing layer has
/// started becoming the GUI framework, and the seam that future focus,
/// pointer capture, scrolling, text, and accessibility work depends on is
/// gone.
#[test]
fn instar_window_does_not_depend_on_instar_ui() {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "instar-window",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree runs");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8_lossy(&output.stdout);
    let offenders: Vec<&str> = tree
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("instar-ui "))
        .collect();

    assert!(
        offenders.is_empty(),
        "instar-window has picked up a dependency on the UI layer: {offenders:?}\n\
         Windowing translates OS input and nothing else. If this crate needs to \
         know what was clicked, the resolution belongs in instar-host, which is \
         allowed to see both sides.\n\nFull tree:\n{tree}"
    );
}

/// The window layer's public surface must not leak winit types at all.
///
/// If `RawPointerEvent` carried `winit::event::MouseButton`, every layer above
/// would depend on winit's release cadence for no benefit -- and a future
/// alternate window backend would have to keep speaking winit's vocabulary.
/// `instar-window` is the only crate whose public types may mention winit.
#[test]
fn pointer_events_use_instar_types_not_winit_types() {
    use instar_window::{LogicalPoint, PointerButton, PointerState, RawPointerEvent, WindowId};

    let event = RawPointerEvent {
        window_id: WindowId::from_raw(1),
        logical_pos: LogicalPoint::new(1.0, 2.0),
        button: PointerButton::Primary,
        state: PointerState::Pressed,
    };

    // Constructing this from Instar types alone is the assertion; it would not
    // compile if the fields were winit's.
    assert_eq!(event.button, PointerButton::Primary);
    assert_eq!(event.state, PointerState::Pressed);
}
