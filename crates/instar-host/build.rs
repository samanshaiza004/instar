//! Builds the WP7B1 bridge guest fixture (`tests/fixtures/host-guest`) into a
//! `wasm32-wasip2` component and exports its path as `HOST_GUEST_WASM`.
//!
//! Same reasoning as the other two fixture build scripts: the component under
//! test is always built from the source next to it, with the pinned toolchain,
//! on whatever OS the test runs on. A checked-in `.wasm` could pass a gate the
//! current toolchain would fail — and WP7B1's gate is specifically about
//! whether this toolchain's async support keeps a suspended guest making
//! prompt progress.
//!
//! The fixture depends on `instar-ui-protocol`, which has no build script, so
//! nothing here re-enters itself. If a fixture ever depends on `instar-host`
//! or `instar-ui` again, the recursion comes back.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("host-guest");

    println!("cargo:rerun-if-changed={}", fixture.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        fixture.join("Cargo.toml").display()
    );
    // The world the fixture generates against, and the encoder it links.
    println!("cargo:rerun-if-changed=../instar-kernel/wit");
    println!("cargo:rerun-if-changed=../instar-ui-protocol/src");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR always set by cargo"));
    // A target dir of our own: sharing the outer workspace's would deadlock on
    // cargo's build lock, since this runs *during* that outer build.
    let target_dir = out_dir.join("host-guest-target");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .current_dir(&fixture)
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
        .unwrap_or_else(|e| panic!("failed to spawn cargo for the host-guest fixture: {e}"));

    if !output.status.success() {
        panic!(
            "building the host-guest fixture failed ({}).\n\
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
        .join("host_guest.wasm");

    assert!(
        wasm.is_file(),
        "host-guest built successfully but no component at {}",
        wasm.display()
    );

    println!("cargo:rustc-env=HOST_GUEST_WASM={}", wasm.display());
}
