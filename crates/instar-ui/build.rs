//! Builds the WP5 interaction guest fixture (`tests/fixtures/ui-guest`) into a
//! `wasm32-wasip2` component and exports its path as `UI_GUEST_WASM`.
//!
//! Same reasoning as `instar-kernel`'s build script: the component under test
//! is always built from the source next to it, with the pinned toolchain, on
//! whatever OS the test runs on.
//!
//! This used to need a re-entry guard, because the fixture depended on
//! `instar-ui` and so building it re-entered this script forever. It does not
//! any more: the fixture depends on `instar-ui-protocol`, which has no build
//! script. If a future fixture starts depending on this crate again, the
//! recursion comes back -- and its signature is memorable, hundreds of nested
//! cargo invocations each indented one level further than the last.

use std::path::PathBuf;
use std::process::Command;

fn main() {
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
