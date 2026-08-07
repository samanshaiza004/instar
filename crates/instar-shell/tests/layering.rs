//! What the shipped guest is allowed to link.
//!
//! From docs/PHASE-1.md's UI layering rule: `instar-ui-protocol` is the *only*
//! Instar crate a guest links for UI. `instar-ui` is free to take on Taffy and
//! anything else it needs precisely because none of it can reach a guest.
//!
//! `instar-ui`'s and `instar-host`'s fixtures state that in a manifest comment.
//! For the guest the shell actually ships, a comment is not enough: the edge
//! that would break this is a one-line convenience ("the guest already knows
//! its node keys, it may as well use the host's `NodeKey`"), and the cost of
//! it is that every guest thereafter links a layout engine.

use std::path::PathBuf;
use std::process::Command;

/// Crates a guest may never link, whatever the reason seems to be at the time.
const FORBIDDEN: [&str; 5] = [
    "instar-ui ",
    "instar-host ",
    "instar-window ",
    "instar-paint ",
    "instar-kernel ",
];

#[test]
fn the_counter_guest_links_the_protocol_and_nothing_else_of_instars() {
    let guest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("guests")
        .join("counter");

    let output = Command::new(env!("CARGO"))
        .args(["tree", "--edges", "normal,build", "--prefix", "none"])
        .current_dir(&guest)
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
        .filter(|line| FORBIDDEN.iter().any(|crate_| line.starts_with(crate_)))
        .collect();

    assert!(
        offenders.is_empty(),
        "the counter guest has picked up host-side crates: {offenders:?}\n\
         A guest speaks the wire format and links nothing else of Instar's. \
         Whatever this edge was for belongs above the guest boundary.\n\n\
         Full tree:\n{tree}"
    );

    assert!(
        tree.lines()
            .map(str::trim)
            .any(|line| line.starts_with("instar-ui-protocol ")),
        "the guest should link the protocol -- if it no longer does, this test \
         is passing for the wrong reason.\n\nFull tree:\n{tree}"
    );
}
