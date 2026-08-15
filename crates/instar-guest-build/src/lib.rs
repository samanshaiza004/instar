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
//! # Why the guests get a nested target directory, and why it is temporary
//!
//! This runs *during* an outer cargo build, which holds the workspace's build
//! lock. Sharing that target directory would deadlock against it, so the
//! guests are compiled into a nested one under `OUT_DIR`.
//!
//! That nested directory used to be *retained*, and Cargo never cleans
//! `OUT_DIR`: a fresh one accumulates for every build-script hash and nothing
//! prunes them. Fifty-one stale guest target directories were found in one
//! working tree, several over a gigabyte, five of them under `instar-shell`
//! alone — 20 GiB of permanently orphaned nested Cargo installations, which
//! was more than half of `target`.
//!
//! So the build now separates the two things `OUT_DIR` was holding:
//!
//! ```text
//! OUT_DIR
//! ├── guest-target/       one shared nested target, removed before exit
//! └── artifacts/
//!     └── counter.wasm    retained; what the env var points at
//! ```
//!
//! One shared nested target rather than one per guest, so guests built by the
//! same invocation do not each recompile `wit-bindgen` and the protocol.
//!
//! The cost is real and worth stating: the nested Cargo cache no longer
//! survives a build-script re-run, so **every** re-run recompiles all of that
//! crate's guests from scratch rather than only the changed one. A build
//! script re-runs when a watched input moves — the WIT world, the workspace
//! manifest, or any guest source — which is exactly when a guest developer is
//! iterating. If that becomes painful, the answer is not to retain the
//! directory again; it is to move guest compilation out of `build.rs` entirely
//! into an `xtask` with one stable target directory this project owns and can
//! garbage-collect.

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
/// Builds every guest in one invocation, then removes the nested target.
///
/// A slice rather than repeated calls to a single-guest function, because the
/// cleanup must not be a separate step a caller can forget — a forgotten
/// cleanup is the leak this exists to close, restored silently.
pub fn build_guests(guests: &[(&str, &str, &str)]) {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR always set by cargo"));
    let target_dir = out_dir.join("guest-target");
    let artifacts = out_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("OUT_DIR is writable");

    // A guard rather than a call at the end of the function. Every failure in
    // `build_guest_into` is a `panic!` -- a guest that will not compile, a
    // missing component, an unspawnable cargo -- and a panic unwinds straight
    // past a trailing statement. The first version of this had the cleanup as
    // the last line with a comment claiming it was unconditional, and it was
    // not: the success path was clean and every error path leaked, which is
    // precisely the shape of regression this design invites.
    let _cleanup = TempTarget(target_dir.clone());

    for (name, package, env_var) in guests {
        build_guest_into(name, package, env_var, &[], &target_dir, &artifacts);
    }
}

/// Same as [`build_guests`], with an explicit `--features` list per guest.
///
/// Exists for `benchmarks/text-latency`: its GATE build needs the ordinary
/// production guest, and its DIAGNOSTIC build needs the same guest source
/// compiled with an extra Cargo feature (see the guest's own `bench-probe`
/// feature) that switches its `wit_bindgen::generate!` world and adds timing
/// marks. Two builds of one guest, from one crate's `build.rs`, need two
/// artifact names -- `env_var` still identifies each uniquely.
pub fn build_guests_with_features(guests: &[(&str, &str, &str, &[&str])]) {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR always set by cargo"));
    let target_dir = out_dir.join("guest-target");
    let artifacts = out_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("OUT_DIR is writable");
    let _cleanup = TempTarget(target_dir.clone());

    for (name, package, env_var, features) in guests {
        build_guest_into(name, package, env_var, features, &target_dir, &artifacts);
    }
}

/// Removes the nested guest target when it goes out of scope, unwinding
/// included.
///
/// Relies on unwinding, which is what a build script gets by default. Under
/// `panic = "abort"` the directory would survive a failed build — recoverable
/// by the next successful one, which removes it, and not worth defending
/// against a profile nothing here sets.
struct TempTarget(PathBuf);

impl Drop for TempTarget {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn build_guest_into(
    name: &str,
    package: &str,
    env_var: &str,
    features: &[&str],
    target_dir: &Path,
    artifacts: &Path,
) {
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

    // Every guest generates against the kernel's WIT, so a change to either
    // world must rebuild all of them.
    watch(&root.join("crates/instar-kernel/wit"));
    // Every crate a guest links inherits its version and edition from the
    // workspace manifest, so that file is an input to the guest too.
    watch(&root.join("Cargo.toml"));
    // The guest itself, and every workspace crate it links, transitively.
    watch_package(&guest, &mut Vec::new());

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command
        .current_dir(&guest)
        .arg("build")
        .arg("--target")
        .arg("wasm32-wasip2")
        .env("CARGO_TARGET_DIR", target_dir)
        // Inherited flags from the outer build refer to the host target and
        // break the wasm build; `RUSTC_WORKSPACE_WRAPPER` would point at the
        // outer workspace.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    let output = command
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

    let built = target_dir
        .join("wasm32-wasip2")
        .join("debug")
        .join(format!("{}.wasm", package.replace('-', "_")));

    assert!(
        built.is_file(),
        "the {name} guest built successfully but no component at {} -- \
         has its package name changed?",
        built.display()
    );

    // Copied out before the nested target is removed. The env var points at
    // the retained artifact, so a consumer holding it across a rebuild still
    // has a file.
    //
    // Named after `env_var`, not `name`: `build_guests_with_features` can
    // build the same guest directory/package twice with different feature
    // sets (one artifact per build mode), and `env_var` is the one thing
    // guaranteed distinct between those two calls -- `name` is not.
    let artifact = artifacts.join(format!("{}.wasm", env_var.to_lowercase()));
    std::fs::copy(&built, &artifact).unwrap_or_else(|error| {
        panic!(
            "could not retain the {name} component at {}: {error}",
            artifact.display()
        )
    });

    println!("cargo:rustc-env={env_var}={}", artifact.display());
}

/// Declares `path` as an input of the calling crate's build. Cargo scans
/// directories recursively, so a directory covers the files under it.
fn watch(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

/// Declares the crate at `dir` as an input, then does the same for every crate
/// it depends on by path — transitively.
///
/// A guest is its own cargo workspace, so cargo sees this build script as an
/// opaque command and tracks none of what the guest is compiled from. Anything
/// not declared here is invisible: edit it, and the guest keeps its previous
/// `.wasm`, which is how a test suite ends up running a guest that speaks an
/// older wire protocol than the host it is being tested against.
///
/// The dependencies are read out of the manifests rather than listed by hand,
/// because a hand-written list only stays right until a guest gains a
/// dependency, and it goes wrong silently.
fn watch_package(dir: &Path, seen: &mut Vec<PathBuf>) {
    let canonical = dir
        .canonicalize()
        .unwrap_or_else(|error| panic!("cannot resolve {}: {error}", dir.display()));
    if seen.contains(&canonical) {
        return;
    }
    seen.push(canonical);

    watch(&dir.join("src"));
    let manifest = dir.join("Cargo.toml");
    watch(&manifest);
    // Only the guests carry a lock file of their own, and it is what pins the
    // versions their build resolves.
    let lock = dir.join("Cargo.lock");
    if lock.is_file() {
        watch(&lock);
    }

    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", manifest.display()));
    for dep in path_dependencies(&text) {
        let dep = dir.join(dep);
        // Something that does not resolve to a crate is not a dependency. A
        // real one that fails to resolve fails the guest's own build moments
        // later, with cargo's diagnosis rather than a guess from here.
        if dep.join("Cargo.toml").is_file() {
            watch_package(&dep, seen);
        }
    }
}

/// The `path = "..."` values in a manifest.
///
/// A scan rather than a TOML parse: this crate is a build dependency of four
/// others and has no dependencies of its own, which is worth more than
/// exactness on manifests we do not write. It over-reports rather than guesses
/// — anything that is not a crate is dropped by the caller — and skips
/// comments, so a commented-out dependency does not become an input.
fn path_dependencies(manifest: &str) -> Vec<&str> {
    manifest
        .lines()
        .filter_map(|line| {
            let uncommented = line.split('#').next().unwrap_or_default();
            let (_, rest) = uncommented.split_once("path")?;
            let rest = rest.trim_start().strip_prefix('=')?.trim_start();
            rest.strip_prefix('"')?.split('"').next()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{TempTarget, path_dependencies};

    #[test]
    fn reads_a_dependency_in_either_shape() {
        let manifest = "\
[dependencies]
wit-bindgen = \"0.60.0\"
instar-ui-protocol = { path = \"../../crates/instar-ui-protocol\" }

[dependencies.other]
path = \"../other\"
";
        assert_eq!(
            path_dependencies(manifest),
            ["../../crates/instar-ui-protocol", "../other"]
        );
    }

    #[test]
    fn skips_comments() {
        let manifest = "# gone = { path = \"../gone\" }\nkept = { path = \"../kept\" } # note\n";
        assert_eq!(path_dependencies(manifest), ["../kept"]);
    }

    #[test]
    fn ignores_a_manifest_with_no_path_dependencies() {
        assert!(path_dependencies("[dependencies]\nwit-bindgen = \"0.60.0\"\n").is_empty());
    }
    /// The error path, which is the one that regressed.
    ///
    /// Cleanup used to be the last statement of `build_guests`, so it ran only
    /// when every guest compiled. A guest that fails — the ordinary case while
    /// someone is editing one — left the whole nested Cargo installation
    /// behind, which is the leak this package exists to close.
    #[test]
    fn the_nested_target_is_removed_even_when_a_guest_build_panics() {
        let dir = std::env::temp_dir().join(format!(
            "instar-guest-build-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(dir.join("wasm32-wasip2/debug")).expect("temp dir is writable");
        std::fs::write(dir.join("wasm32-wasip2/debug/big.bin"), [0u8; 4096])
            .expect("temp dir is writable");
        assert!(dir.is_dir(), "the fixture exists to begin with");

        let path = dir.clone();
        let result = std::panic::catch_unwind(move || {
            let _cleanup = TempTarget(path);
            panic!("a guest failed to compile");
        });

        assert!(result.is_err(), "the panic really happened");
        assert!(
            !dir.exists(),
            "a failed guest build must still remove {}; leaving it is how a \
             gigabyte per build-script hash accumulates",
            dir.display()
        );
    }
}
