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

/// Every term in the input vocabulary must be produced by something.
///
/// This is the rule that failed three times in one session, in one `match`.
/// `WindowOutput::Scroll`, `WindowOutput::PointerMoved` and `WindowOutput::Key`
/// each had a complete, tested implementation waiting on the far side of the
/// host, and `winit_adapter::translate` had no arm that produced them — so a
/// wheel did nothing, a scrollbar thumb took a press and never moved, and Tab
/// did not move focus. Every layer was correct. The seam did not exist.
///
/// Unit tests cannot see this. At the level of any one package nothing is
/// missing: the enum has the variant, the host handles it, the arithmetic is
/// right. What is absent is a line in a different crate, and the only way to
/// notice is to ask whether anything ever constructs the term.
///
/// Source inspection rather than types because a `match` arm returning `None`
/// is perfectly well-typed, which is exactly why this went unnoticed. If a
/// term is ever genuinely produced somewhere else, extend the search — do not
/// delete the rule.
#[test]
fn every_window_output_term_is_produced_by_the_winit_adapter() {
    let lib = include_str!("../src/lib.rs");
    // Production code only. The adapter's own test module names these terms in
    // its assertions, and counting those would let a test satisfy the search
    // for the arm it is meant to be testing -- which is exactly how the first
    // draft of this test passed with the pointer-move arm deleted.
    let adapter = include_str!("../src/winit_adapter.rs")
        .split_once("#[cfg(test)]")
        .map_or_else(
            || panic!("the adapter's test module marks the end of production code"),
            |(production, _)| production,
        );

    // The variants declared on the enum, read from its own definition so a
    // new term is covered the moment it is added.
    let body = lib
        .split_once("pub enum WindowOutput {")
        .expect("WindowOutput is declared in lib.rs")
        .1
        .split_once("\n}")
        .expect("the enum ends")
        .0;

    let terms: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| {
            // Variant lines, not doc comments or attributes.
            line.chars().next().is_some_and(char::is_uppercase)
        })
        .map(|line| {
            line.split(['(', ' ', '{', ','])
                .next()
                .expect("a variant name")
        })
        .collect();

    assert!(
        terms.len() >= 7,
        "only found {terms:?} -- the parser stopped matching the enum's shape"
    );

    // `MetricsInvalidated` is the exception, and a deliberate one: it is not a
    // translation of any single winit event but a barrier raised alongside
    // `ScaleFactorChanged`. It is produced in the adapter all the same, so it
    // needs no special case here -- if that ever changes, this comment is the
    // place to record why.
    let missing: Vec<&&str> = terms
        .iter()
        .filter(|term| !adapter.contains(&format!("WindowOutput::{term}")))
        .collect();

    assert!(
        missing.is_empty(),
        "these input terms are declared, and routed by instar-host, but \
         nothing in winit_adapter::translate ever produces them: {missing:?}\n\n\
         A term with no producer is a subsystem that is complete, tested, and \
         unreachable from the running application. That has happened three \
         times: the wheel, the pointer move, and the keyboard. Add the missing \
         `match` arm rather than removing the term."
    );
}
