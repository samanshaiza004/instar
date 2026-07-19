//! Youth Rust Guest Profile 0.0.1 — the guest import budget.
//!
//! Youth's WASI context is closed, so these imports are inert: the host
//! decides what they return. They remain a real linking and compatibility
//! surface, so this test fails when a toolchain update silently widens it.
//! See `docs/GUEST-PROFILE.md`.

mod common;

use common::counter_component;
use youth_runtime::component_imports;

/// Interfaces the Youth application contract itself requires.
const REQUIRED: &[&str] = &["youth:app/ui"];

/// Inert WASIp2 interfaces the Rust standard library pulls in on
/// `wasm32-wasip2`. Permitted, but budgeted: adding to this list is a
/// deliberate decision, never an accident of a toolchain bump.
const PERMITTED_INERT: &[&str] = &[
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stderr",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stderr",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:clocks/monotonic-clock",
    "wasi:io/error",
    "wasi:io/poll",
    "wasi:io/streams",
];

#[test]
fn counter_imports_match_the_guest_profile_exactly() {
    let imports = component_imports(counter_component()).expect("component imports are readable");

    let mut allowed: Vec<&str> = REQUIRED
        .iter()
        .chain(PERMITTED_INERT.iter())
        .copied()
        .collect();
    allowed.sort_unstable();

    let unexpected: Vec<&String> = imports
        .iter()
        .filter(|import| !allowed.contains(&import.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "component imports interfaces outside the Youth Rust Guest Profile: {unexpected:?}\n\
         If this is an intended toolchain change, review whether the new capability is \
         acceptable and add it to PERMITTED_INERT in this file and to docs/GUEST-PROFILE.md."
    );

    let missing: Vec<&&str> = REQUIRED
        .iter()
        .filter(|required| !imports.iter().any(|import| import == *required))
        .collect();
    assert!(
        missing.is_empty(),
        "component no longer imports the Youth contract: {missing:?}"
    );
}

#[test]
fn the_profile_forbids_filesystem_and_network_capabilities() {
    let imports = component_imports(counter_component()).expect("component imports are readable");

    // These are the capabilities Milestone 0 must not grant at all
    // (spec section 14). They are absent from the guest entirely, so the
    // closed context is a second line of defense rather than the only one.
    for forbidden in ["wasi:filesystem", "wasi:sockets", "wasi:http"] {
        assert!(
            !imports.iter().any(|import| import.starts_with(forbidden)),
            "component imports a forbidden capability family: {forbidden}"
        );
    }
}
