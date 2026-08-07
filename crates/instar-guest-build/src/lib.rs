//! Compiling Instar guests from build scripts.
//!
//! Four crates build a guest during their own build, and before WP8 each
//! carried its own 70-line copy of this. The copies had already begun to
//! drift, which is the usual fate of duplicated build logic: it is rarely
//! read, and a fix applied to one copy looks complete.
//!
//! # Why guests are built rather than committed
//!
//! A checked-in `.wasm` could pass a gate the current toolchain would fail.
//! That matters most for Gate 0, whose entire question is whether *this*
//! toolchain's async support behaves — but it matters everywhere, because a
//! guest is the only thing in the repository compiled by a different target
//! and a different set of assumptions. Building from source next to the test,
//! on whatever OS the test runs on, is what keeps that honest.
//!
//! # Why each guest gets its own target directory
//!
//! This runs *during* an outer cargo build, which holds the workspace's build
//! lock. Sharing that target directory would deadlock against it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Builds `guests/<name>` for `wasm32-wasip2` and exports the component's path
/// to the calling crate as the environment variable `env_var`, readable with
/// `env!`.
///
/// `package` is the guest's cargo package name, which is what determines the
/// output filename — it is passed separately rather than derived from `name`
/// so a directory can be renamed without silently producing a "built fine but
/// no component" failure.
///
/// # Panics
///
/// On any failure, with the guest's real compiler output. A build script that
/// failed quietly would surface as a confusing `env!` error in a completely
/// different crate.
pub fn build_guest(name: &str, package: &str, env_var: &str) {
    // CARGO_MANIFEST_DIR is the *calling* crate's, and every caller lives at
    // crates/<crate>, so the guests tree is two levels up.
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR always set by cargo"),
    );
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .expect("a crate under crates/ always has a workspace root two levels up");
    let guest = root.join("guests").join(name);

    assert!(
        guest.is_dir(),
        "no guest at {} -- guests live in the workspace's guests/ directory",
        guest.display()
    );

    println!("cargo:rerun-if-changed={}", guest.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        guest.join("Cargo.toml").display()
    );
    // Every guest generates against the kernel's WIT, so a change to either
    // world must rebuild all of them.
    println!(
        "cargo:rerun-if-changed={}",
        root.join("crates/instar-kernel/wit").display()
    );
    // And they all link the wire format.
    println!(
        "cargo:rerun-if-changed={}",
        root.join("crates/instar-ui-protocol/src").display()
    );

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR always set by cargo"));
    let target_dir = out_dir.join(format!("{name}-target"));

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .current_dir(&guest)
        .arg("build")
        .arg("--target")
        .arg("wasm32-wasip2")
        .env("CARGO_TARGET_DIR", &target_dir)
        // Inherited flags from the outer build refer to the host target and
        // break the wasm build; `RUSTC_WORKSPACE_WRAPPER` would point at the
        // outer workspace.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn cargo for the {name} guest: {error}"));

    if !output.status.success() {
        // The guest's real compiler output, not a bare exit code: a WIT-level
        // mistake shows up here first, and catching those is what the guests
        // are for.
        panic!(
            "building the {} guest failed ({}).\n\
             Is the `wasm32-wasip2` target installed? \
             (`rustup target add wasm32-wasip2`)\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            name,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let wasm = target_dir
        .join("wasm32-wasip2")
        .join("debug")
        .join(format!("{}.wasm", package.replace('-', "_")));

    assert!(
        wasm.is_file(),
        "the {name} guest built successfully but no component at {} -- \
         has its package name changed?",
        wasm.display()
    );

    println!("cargo:rustc-env={env_var}={}", wasm.display());
}
