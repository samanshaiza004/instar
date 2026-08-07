//! The dependency rules from docs/PHASE-1.md, enforced rather than documented.
//!
//! Architecture rules that live only in prose decay, and always via a
//! plausible one-line convenience. These are the two that Phase 1 rests on:
//!
//! - **A guest links `instar-ui-protocol` and nothing else of Instar's.**
//!   `instar-ui` is free to take on Taffy, and `instar-host` a renderer,
//!   precisely because neither can reach a guest.
//! - **`instar-kernel` knows nothing about windows, layout, or pixels.** It
//!   runs components; what they describe is somebody else's problem.
//!
//! This test lives in the topmost crate because it makes claims about the
//! whole workspace, and the topmost crate is the only one that can see it all
//! without inverting the very layering being checked.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/instar-shell always has a workspace root two levels up")
        .to_path_buf()
}

/// `cargo tree` for a package, one crate per line, no prefix characters.
fn tree(dir: &Path, args: &[&str]) -> String {
    let mut command = Command::new(env!("CARGO"));
    command
        .args(["tree", "--edges", "normal,build", "--prefix", "none"])
        .args(args)
        .current_dir(dir);

    let output = command.output().expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Whether `tree` contains a dependency on the package `name`.
///
/// Matches on the trailing space so `instar-ui` does not match
/// `instar-ui-protocol` — which is the single distinction this whole file
/// exists to police.
fn depends_on(tree: &str, name: &str) -> bool {
    tree.lines()
        .map(str::trim)
        .any(|line| line.starts_with(&format!("{name} ")))
}

/// Every guest in `guests/`, discovered rather than listed.
///
/// A hardcoded list would silently stop covering a guest added later, which is
/// exactly when this rule is most likely to be broken.
fn guests() -> Vec<PathBuf> {
    let dir = workspace_root().join("guests");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("Cargo.toml").is_file())
        .collect();
    found.sort();

    assert!(
        !found.is_empty(),
        "no guests found under {} -- this test would pass vacuously",
        dir.display()
    );
    found
}

/// The Instar crates a dependency tree names.
fn instar_crates(tree: &str) -> Vec<String> {
    let mut found: Vec<String> = tree
        .lines()
        .map(str::trim)
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| name.starts_with("instar-"))
        .map(str::to_string)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The only Instar crate a guest may link.
const GUEST_ALLOWED: &str = "instar-ui-protocol";

/// Stated as a subset rule rather than a list of forbidden crates, because a
/// blocklist stops covering the case that matters — the crate that does not
/// exist yet. A guest linking some future `instar-widgets` would sail past any
/// list written today; it does not sail past this.
///
/// Not every guest links even the protocol: the two kernel-level guests
/// exercise idle suspension and generation lifecycle, and never describe an
/// interface at all. An empty set satisfies the rule.
#[test]
fn no_guest_links_any_instar_crate_but_the_protocol() {
    let mut saw_protocol = false;

    for guest in guests() {
        let name = guest.file_name().unwrap_or_default().to_string_lossy();
        let tree = tree(&guest, &[]);

        let linked = instar_crates(&tree);
        let offenders: Vec<&String> = linked
            .iter()
            .filter(|crate_| crate_.as_str() != GUEST_ALLOWED)
            .collect();
        assert!(
            offenders.is_empty(),
            "the {name} guest links {offenders:?}.\n\
             A guest speaks the wire format and links nothing else of \
             Instar's — that is what lets instar-ui take on a layout engine \
             and instar-host a renderer. Whatever this edge was for belongs \
             above the guest boundary.\n\nFull tree:\n{tree}"
        );

        saw_protocol |= linked.iter().any(|crate_| crate_ == GUEST_ALLOWED);
    }

    assert!(
        saw_protocol,
        "no guest links {GUEST_ALLOWED}, so the matcher above proved nothing"
    );
}

/// From docs/PHASE-1.md, "Forbidden dependencies": `instar-kernel` must never
/// depend on winit, Taffy, Vello, softbuffer, a text renderer, `instar-ui`, or
/// counter-specific types.
///
/// The kernel runs components. What a component describes — a window, a
/// button, a glyph — is a question for layers above it, and an edge here would
/// mean the runtime had opinions about the applications it hosts.
#[test]
fn the_kernel_knows_nothing_about_windows_layout_or_pixels() {
    const FORBIDDEN: [&str; 8] = [
        "winit",
        "taffy",
        "vello_cpu",
        "vello_common",
        "softbuffer",
        "skrifa",
        "instar-ui",
        "instar-paint",
    ];

    let root = workspace_root();
    let tree = tree(&root, &["-p", "instar-kernel"]);

    let offenders: Vec<&str> = FORBIDDEN
        .iter()
        .filter(|crate_| depends_on(&tree, crate_))
        .copied()
        .collect();
    assert!(
        offenders.is_empty(),
        "instar-kernel has picked up presentation dependencies: {offenders:?}\n\
         The kernel runs components; what they describe is somebody else's \
         problem. See docs/PHASE-1.md, \"Forbidden dependencies\".\n\n\
         Full tree:\n{tree}"
    );
}

/// `instar-host` bridges logical presentation to physical rendering, so it may
/// see paint *types* — and must not see a backend, a window system, or a font.
/// Choosing what rasterizes a scene lives one layer up, in this crate.
#[test]
fn the_host_takes_paint_types_but_no_renderer() {
    let root = workspace_root();
    let tree = tree(&root, &["-p", "instar-host"]);

    assert!(
        depends_on(&tree, "instar-paint"),
        "instar-host lowers trees to PaintScenes, so it must see the scene \
         types.\n\nFull tree:\n{tree}"
    );
    for forbidden in ["vello_cpu", "softbuffer", "skrifa"] {
        assert!(
            !depends_on(&tree, forbidden),
            "instar-host has picked up {forbidden}. Lowering a tree to paint \
             intent belongs here; choosing what rasterizes it does not.\n\n\
             Full tree:\n{tree}"
        );
    }
}
