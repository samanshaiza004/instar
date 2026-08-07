//! Builds the counter guest (`guests/counter`) into a `wasm32-wasip2`
//! component and exports its path as `COUNTER_WASM`.
//!
//! Same reasoning as the fixture build scripts under `instar-kernel`,
//! `instar-ui`, and `instar-host`: the component is always built from the
//! source next to it, with the pinned toolchain, on whatever OS is running.
//! A checked-in `.wasm` could pass on a toolchain that the current one would
//! fail — and for the guest the shell actually runs, that is the difference
//! between shipping a binary and shipping a build.
//!
//! The guest depends on `instar-ui-protocol`, which has no build script, so
//! nothing here re-enters itself.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let guest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("guests")
        .join("counter");

    println!("cargo:rerun-if-changed={}", guest.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        guest.join("Cargo.toml").display()
    );
    // The world it generates against, and the encoder it links.
    println!("cargo:rerun-if-changed=../instar-kernel/wit");
    println!("cargo:rerun-if-changed=../instar-ui-protocol/src");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR always set by cargo"));
    // A target dir of our own: sharing the outer workspace's would deadlock on
    // cargo's build lock, since this runs *during* that outer build.
    let target_dir = out_dir.join("counter-target");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .current_dir(&guest)
        .arg("build")
        .arg("--target")
        .arg("wasm32-wasip2")
        .env("CARGO_TARGET_DIR", &target_dir)
        // Inherited flags from the outer build refer to the host target and
        // break the wasm build.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .unwrap_or_else(|error| panic!("failed to spawn cargo for the counter guest: {error}"));

    if !output.status.success() {
        panic!(
            "building the counter guest failed ({}).\n\
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
        .join("counter.wasm");

    assert!(
        wasm.is_file(),
        "the counter guest built successfully but no component at {}",
        wasm.display()
    );

    println!("cargo:rustc-env=COUNTER_WASM={}", wasm.display());
}
