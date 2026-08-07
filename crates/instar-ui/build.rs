//! Builds the WP5 interaction guest fixture (`tests/fixtures/ui-guest`) into a
//! `wasm32-wasip2` component and exports its path as `UI_GUEST_WASM`.
//!
//! Same reasoning as `instar-kernel`'s build script: the component under test
//! is always built from the source next to it, with the pinned toolchain, on
//! whatever OS the test runs on. Here it does double duty — the fixture
//! depends on `instar-ui` itself, so this is also what proves this crate
//! compiles for wasm32, which it must, since guests are its main consumer.

use std::path::PathBuf;
use std::process::Command;

/// Set while building the fixture, to stop this script recursing.
///
/// The fixture depends on `instar-ui` (deliberately — the guest and host must
/// share one definition of the encoding), so building it builds this crate
/// again, which runs this script again, which builds the fixture again. That
/// recursion is infinite and its failure mode is memorable: hundreds of nested
/// cargo invocations, each indented one level further than the last.
const REENTRY_GUARD: &str = "INSTAR_UI_BUILDING_FIXTURE";

fn main() {
    if std::env::var_os(REENTRY_GUARD).is_some() {
        // This is the nested build of `instar-ui` for wasm32-wasip2, as a
        // dependency of the fixture. It needs no fixture of its own, and
        // nothing in the library reads `UI_GUEST_WASM` — only the tests do.
        return;
    }

    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ui-guest");

    println!("cargo:rerun-if-changed={}", fixture.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        fixture.join("Cargo.toml").display()
    );
    // The fixture depends on this crate's own source and on the kernel's WIT.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=../instar-kernel/wit");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR always set by cargo"));
    let target_dir = out_dir.join("ui-guest-target");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .current_dir(&fixture)
        .arg("build")
        .arg("--target")
        .arg("wasm32-wasip2")
        .env("CARGO_TARGET_DIR", &target_dir)
        .env(REENTRY_GUARD, "1")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .expect("failed to spawn cargo for the ui-guest fixture");

    if !output.status.success() {
        panic!(
            "building the ui-guest fixture failed ({}).\n\
             Is the `wasm32-wasip2` target installed? \
             (`rustup target add wasm32-wasip2`)\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let wasm = target_dir
        .join("wasm32-wasip2")
        .join("debug")
        .join("ui_guest.wasm");

    assert!(
        wasm.is_file(),
        "ui-guest built successfully but no component at {}",
        wasm.display()
    );

    println!("cargo:rustc-env=UI_GUEST_WASM={}", wasm.display());
}
