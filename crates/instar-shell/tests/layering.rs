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

/// The Instar crates a guest may link.
///
/// Two, since H1. `instar-sdk` is a thin snapshot builder and event router
/// that depends on the protocol and nothing else — the property this rule
/// actually protects is not "one crate" but "no guest links a layout engine
/// or a renderer", and that survives the addition. The subset below is
/// therefore paired with `the_sdk_is_not_a_back_door_into_the_host`: without
/// that second test, widening this list would let anything reach a guest by
/// hiding behind the SDK.
const GUEST_ALLOWED: [&str; 2] = ["instar-ui-protocol", "instar-sdk"];

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
            .filter(|crate_| !GUEST_ALLOWED.contains(&crate_.as_str()))
            .collect();
        assert!(
            offenders.is_empty(),
            "the {name} guest links {offenders:?}.\n\
             A guest speaks the wire format, optionally through the SDK, and \
             links nothing else of Instar's — that is what lets instar-ui take \
             on a layout engine and instar-host a renderer. Whatever this edge \
             was for belongs above the guest boundary.\n\nFull tree:\n{tree}"
        );

        saw_protocol |= linked.iter().any(|crate_| crate_ == "instar-ui-protocol");
    }

    assert!(
        saw_protocol,
        "no guest links instar-ui-protocol, so the matcher above proved nothing"
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
/// see paint *types* — and must not see a rasterizer or a window system.
/// Choosing what rasterizes a scene lives one layer up, in this crate.
///
/// # Why `skrifa` is not on this list
///
/// It was, and Stage 2 took it off deliberately rather than because it had
/// become inconvenient. The original rule read "no backend, no window system,
/// no font", and `skrifa` stood in for the last clause. Stage 1 then put a
/// real text stack in `instar-ui` — Parley shapes in logical space, and the
/// host converts shaped positions and font ppem to physical space during paint
/// lowering. `skrifa` is font-*data parsing* that arrives with that stack:
/// `instar-host -> instar-ui -> parley -> skrifa`.
///
/// So the host cannot avoid it without either the UI layer losing its text
/// stack or the host losing the UI layer, and neither is a layering
/// improvement. What the rule was actually protecting — that the host does not
/// pick what draws pixels — is unchanged and still asserted below.
#[test]
fn the_host_takes_paint_types_but_no_renderer() {
    let root = workspace_root();
    let tree = tree(&root, &["-p", "instar-host"]);

    assert!(
        depends_on(&tree, "instar-paint"),
        "instar-host lowers trees to PaintScenes, so it must see the scene \
         types.\n\nFull tree:\n{tree}"
    );
    for forbidden in ["vello_cpu", "softbuffer"] {
        assert!(
            !depends_on(&tree, forbidden),
            "instar-host has picked up {forbidden}. Lowering a tree to paint \
             intent belongs here; choosing what rasterizes it does not.\n\n\
             Full tree:\n{tree}"
        );
    }
}

/// The SDK may not become a way for anything else to reach a guest.
///
/// `no_guest_links_any_instar_crate_but_the_protocol` was widened when
/// `instar-sdk` arrived, and a widened allowlist is only as good as what sits
/// behind the thing it admitted. If the SDK ever grew an edge to `instar-ui`,
/// every guest would transitively link a layout engine while that test stayed
/// green — the rule would still read correctly and mean nothing.
#[test]
fn the_sdk_is_not_a_back_door_into_the_host() {
    let tree = tree(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../instar-sdk"),
        &[],
    );
    let linked = instar_crates(&tree);
    let offenders: Vec<&String> = linked
        .iter()
        .filter(|crate_| crate_.as_str() != "instar-sdk" && crate_.as_str() != "instar-ui-protocol")
        .collect();
    assert!(
        offenders.is_empty(),
        "instar-sdk links {offenders:?}, and every guest links instar-sdk.\n\
         The SDK is guest-visible, so its dependencies are guest dependencies: \
         an edge here puts that crate inside every application that uses the \
         builder.\n\nFull tree:\n{tree}"
    );
}

/// The runtime's capability surface is what this project declares, not what
/// Wasmtime enables for a general embedding.
///
/// Wasmtime 47 turns on 28 features by default — three GC implementations,
/// coredump support, WAT parsing, a compilation cache, profiling hooks — for
/// an embedder that does not know what it is embedding. Instar knows, and
/// package I cut the set to what Gate 0 and the suites actually prove
/// necessary.
///
/// This test exists because a comment could not hold that line. Cargo builds a
/// dependency with the **union** of the features every declaration enables, so
/// `default-features = false` in one crate is undone by a plain
/// `wasmtime = "47"` in another. That is not hypothetical: the first
/// minimization touched only `instar-kernel`, `instar-host` still declared the
/// default set, and the binary came out byte-identical. The diff read
/// correctly and did nothing.
///
/// So a future crate adding an innocent dependency line would silently restore
/// GC, coredumps and a WAT parser to the shipped runtime while every other
/// test stayed green. Changing the capability surface must mean changing this
/// list on purpose.
#[test]
fn the_runtime_links_exactly_the_wasmtime_features_instar_declares() {
    /// The six Instar declares, plus three Wasmtime turns on for itself from
    /// `cranelift`, `runtime` and `std`. Those three are not Instar's to
    /// choose; they are listed so the check is exact rather than a
    /// superset test, which would let a real addition hide among them.
    ///
    /// `once_cell` is here because this test found it. The ad-hoc `cargo tree`
    /// grep used while landing package I matched `[a-z0-9-]+` and silently
    /// skipped every feature with an underscore — a measurement with the same
    /// shape of bug as the thing it was measuring.
    const APPROVED: [&str; 9] = [
        "async",
        "component-model",
        "component-model-async",
        "cranelift",
        "once_cell",
        "runtime",
        "std",
        "wasmtime-jit-icache-coherence",
        "wasmtime-unwinder",
    ];
    const APPROVED_WASI: [&str; 1] = ["p2"];

    for (package, approved) in [
        ("wasmtime", APPROVED.as_slice()),
        ("wasmtime-wasi", APPROVED_WASI.as_slice()),
    ] {
        let output = std::process::Command::new(env!("CARGO"))
            .args(["tree", "-p", package, "-e", "features", "--invert"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("cargo tree runs");
        assert!(
            output.status.success(),
            "cargo tree failed for {package}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let tree = String::from_utf8_lossy(&output.stdout);

        let needle = format!("{package} feature \"");
        let mut enabled: Vec<&str> = tree
            .lines()
            .filter_map(|line| line.split_once(&needle))
            .filter_map(|(_, rest)| rest.split_once('"'))
            .map(|(feature, _)| feature)
            .collect();
        enabled.sort_unstable();
        enabled.dedup();

        assert!(
            !enabled.is_empty(),
            "no {package} features were parsed out of cargo tree, so this test \
             is checking nothing. The output format has moved:\n{tree}"
        );

        let added: Vec<&&str> = enabled
            .iter()
            .filter(|feature| !approved.contains(feature))
            .collect();
        let removed: Vec<&&str> = approved
            .iter()
            .filter(|feature| !enabled.contains(feature))
            .collect();

        assert!(
            added.is_empty() && removed.is_empty(),
            "{package}'s resolved feature set is not the declared one.\n\
             added:   {added:?}\n\
             removed: {removed:?}\n\n\
             Cargo builds a dependency with the union of every declaration's \
             features, so a plain `{package} = \"47\"` anywhere in the \
             workspace restores the defaults regardless of what \
             instar-kernel and instar-host ask for. If this addition is \
             intended, widen the list above deliberately and say what needs \
             it.\n\nFull tree:\n{tree}"
        );
    }
}
