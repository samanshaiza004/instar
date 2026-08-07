//! Builds the Gate 0 spike guest fixture (`tests/fixtures/kernel-spike-guest`)
//! into a `wasm32-wasip2` component and hands its path to the test suite via
//! `KERNEL_SPIKE_GUEST_WASM`.
//!
//! Doing this from a build script (rather than committing a prebuilt `.wasm`
//! or making CI run two cargo invocations) keeps the fixture honest: the
//! component under test is always built from the source next to it, with the
//! pinned toolchain, on whatever OS the test is running on. That matters for
//! Gate 0 specifically, since the whole question is whether this toolchain's
//! async support works -- a stale checked-in artifact could pass a gate the
//! current toolchain would fail.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // The kernel's WIT is shared by every fixture, so a change to either world
    // must rebuild all of them.
    println!("cargo:rerun-if-changed=wit");

    build_fixture(
        "kernel-spike-guest",
        "kernel_spike_guest",
        "KERNEL_SPIKE_GUEST_WASM",
    );
    build_fixture("kernel-guest", "kernel_guest", "KERNEL_GUEST_WASM");
}

/// Builds one guest fixture and exports its path as `env_var`.
fn build_fixture(dir_name: &str, crate_name: &str, env_var: &str) {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(dir_name);

    println!("cargo:rerun-if-changed={}", fixture.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        fixture.join("Cargo.toml").display()
    );
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR always set by cargo"));
    // A target dir of our own: sharing the outer workspace's would deadlock on
    // cargo's build lock, since this runs *during* that outer build.
    let target_dir = out_dir.join(format!("{dir_name}-target"));

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .current_dir(&fixture)
        .arg("build")
        .arg("--target")
        .arg("wasm32-wasip2")
        .env("CARGO_TARGET_DIR", &target_dir)
        // Inherited flags from the outer build refer to the host target and
        // break the wasm build (and `RUSTC_WORKSPACE_WRAPPER` would point at
        // the outer workspace).
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn cargo for the {dir_name} fixture: {e}"));

    if !output.status.success() {
        // Surface the guest's real compiler output rather than a bare exit
        // code -- a WIT-level mistake shows up here first, and the whole point
        // of the fixture is to catch those.
        panic!(
            "building the {} fixture failed ({}).\n\
             Is the `wasm32-wasip2` target installed? \
             (`rustup target add wasm32-wasip2`)\n\
             --- stdout ---\n{}\n--- stderr ---\n{}",
            dir_name,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let wasm = target_dir
        .join("wasm32-wasip2")
        .join("debug")
        .join(format!("{crate_name}.wasm"));

    assert!(
        wasm.is_file(),
        "{dir_name} built successfully but no component at {}",
        wasm.display()
    );

    println!("cargo:rustc-env={env_var}={}", wasm.display());
}
