# Distribution

`dist` (formerly `cargo-dist`) packages and publishes the `youth` CLI binary
(`crates/youth-cli`, `[[bin]] name = "youth"`). This document covers what
gets shipped, how to install and remove it, and the checklist for cutting a
real release.

## What ships

Only `crates/youth-cli`'s `youth` binary is distributed. `crates/youth-desktop`
also declares its own `[[bin]] name = "youth-desktop"`, but that target is a
dev/test-only smoke harness -- it is never invoked as a standalone executable
in production. `youth run` and `youth dev` launch the native host by
re-exec'ing `std::env::current_exe()` (i.e. the `youth` binary itself, with
`run` or the hidden `__dev-child` subcommand); `youth-desktop` is linked into
`youth` as a library. Accordingly `crates/youth-desktop/Cargo.toml` sets
`[package.metadata.dist] dist = false` so `dist` never builds or ships it.

Everything `youth` needs at runtime is embedded in the binary at compile
time (project templates, the vendored WIT contract, the toolchain-version
contract) via `include_str!`. There is no repository-relative path
dependency, and no external `wasm-tools` binary dependency: component
validation goes through `youth_runtime::validate_component`, which uses the
in-process `wasmtime`/`wat` crates, not a shelled-out `wasm-tools` process.
(CI's own `taiki-e/install-action@wasm-tools` step is only for validating
CI-built guest test fixtures -- an internal repo/test concern, unrelated to
what a shipped `youth` binary needs.)

`youth check` / `youth build` / `youth test` *do* shell out to `cargo` on
whatever project you point them at (a generated project is an ordinary Rust
crate compiled to `wasm32-wasip2`), so end users need a working Rust
toolchain themselves -- see Prerequisites below. `youth doctor` checks for
exactly this (`cargo`, `rustc`, `rustup`, the required toolchain channel, and
the `wasm32-wasip2` target).

## Supported platforms

| Platform | Target triple |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |
| macOS x86_64 (Intel) | `x86_64-apple-darwin` |
| macOS arm64 (Apple Silicon) | `aarch64-apple-darwin` |

Configured in `dist-workspace.toml` (`[dist] targets`). Installers: a POSIX
shell script (`youth-cli-installer.sh`) and a PowerShell script
(`youth-cli-installer.ps1`), both hosted alongside the GitHub Release.

## Prerequisites (for people building/testing *this repo*)

- Rust `1.97.1` via `rustup` (pinned in the root `rust-toolchain.toml`),
  with the `wasm32-wasip2` target and `rustfmt`/`clippy` components.
- No `wasm-tools` binary is required for `youth` itself. CI installs it
  separately (`taiki-e/install-action@wasm-tools`) only to validate the
  guest/test-fixture `.wasm` components CI builds for its own test matrix.

## Prerequisites (for end users of the *distributed* `youth` binary)

- A working Rust toolchain with the `wasm32-wasip2` target, since `youth new`
  scaffolds a Rust project that `youth check`/`build`/`test` compile via
  `cargo`. Run `youth doctor` after installing to confirm this and get exact
  remediation (`rustup toolchain install <channel>`,
  `rustup target add wasm32-wasip2 --toolchain <channel>`).
- No other runtime dependency; the `youth` binary is otherwise self-contained.

## Install paths

The installer scripts default `install-path` to `CARGO_HOME` (i.e.
`$CARGO_HOME/bin`, falling back to `$HOME/.cargo/bin` on Unix or the
equivalent on Windows) -- the same place `rustup`-installed tools like
`cargo`/`rustc` live, and already on most Rust developers' `PATH`.

An install receipt (used for future upgrade/uninstall tooling) is written to:

- Unix: `${XDG_CONFIG_HOME:-$HOME/.config}/youth-cli/youth-cli-receipt.json`
- Windows: `%LOCALAPPDATA%\youth-cli\`

Application state (created the first time a generated project is run, not
by installing `youth` itself) lives in the OS-standard data directory
(via the `directories` crate):

- Linux: `~/.local/share/youth`
- macOS: `~/Library/Application Support/youth`
- Windows: `%LOCALAPPDATA%\youth`

## Uninstall

1. Remove the binary: delete `youth` (or `youth.exe`) from the install
   directory reported above (typically `$CARGO_HOME/bin/youth`).
2. Remove the install receipt directory listed above
   (`~/.config/youth-cli/` on Unix, `%LOCALAPPDATA%\youth-cli\` on Windows).
3. Optional -- to also remove all local application state created by
   projects you ran with `youth`, delete the OS data directory listed above
   (`~/Library/Application Support/youth`, `~/.local/share/youth`, or
   `%LOCALAPPDATA%\youth`). This is per-machine, per-user data, not tied to
   any one project checkout.

`install-updater` is currently `false`, so there is no separate self-update/
self-uninstall executable generated; the steps above are manual.

## Verification performed

Before this configuration was adopted, the packaged binary was smoke-tested
from directories with no relationship to this repository checkout:

```
youth --version
youth doctor            # cargo/rustc/rustup/toolchain/target checks
youth new <dest>                         # derives dev.youth.<package>
youth new <dest> --id <app-id>           # optional explicit identity
cd <dest> && youth check
youth test
youth build --release
youth dev --headless-supervisor   # re-execs itself as __dev-child, mounts, clean shutdown on SIGINT
youth run <component> --app-id <id> --ephemeral   # native window, where a display is available
```

All of the above passed standalone: no step touched anything under the
repository checkout, confirming no repo-relative path dependency.

A cross-platform CI verification pass (`pr-run-mode = "upload"`) built and
uploaded artifacts for all four targets from a real PR; see the PR history
for the specific run. `pr-run-mode` is reverted to the default `"plan"`
afterward -- PRs only run `dist plan` going forward; full cross-platform
builds+uploads happen on an actual release tag push.

## Release procedure (for the first real release)

This has **not** been run yet -- no tag has been created or pushed as part
of this work. When ready to cut the first real release:

1. Make sure `master` is green (`ci.yml`) and `dist-workspace.toml` has
   `pr-run-mode = "plan"` (the default -- confirm it wasn't left on
   `"upload"` from a verification pass).
2. Decide the version. `dist` announces from the workspace/package version
   in `Cargo.toml` (`[workspace.package] version`), currently `0.0.2`. Bump
   it if needed and commit.
3. Run `dist plan` locally (or let the `plan` CI job do it on the tag push)
   to sanity-check what will be built and announced.
4. Create and push a git tag matching the version, e.g.:
   ```
   git tag v0.0.2
   git push origin v0.0.2
   ```
   (`dist`'s tag-matching pattern also accepts `youth-cli-v0.0.2` /
   `youth-cli/0.0.2` for a package-scoped release; a plain `vX.Y.Z` tag
   works for this single-dist-package workspace.)
5. Pushing the tag triggers `.github/workflows/release.yml`: `plan` ->
   `build-local-artifacts` (all 4 targets, with GitHub attestations) ->
   `build-global-artifacts` (installers, checksums, source tarball) ->
   `host` (creates the GitHub Release and uploads everything) -> `announce`.
6. Watch the Actions run to completion (`gh run watch` or the Actions tab).
   On success, a GitHub Release is published automatically with the
   generated title/body, all platform archives, both installer scripts, and
   attestations.
7. Smoke-test the real release: run the published shell installer (or
   PowerShell installer) on a clean machine/container, then repeat the
   verification sequence above (`youth --version`, `youth doctor`,
   `youth new`, `youth dev`, ...).
8. If something is wrong post-publish, `dist` releases are just GitHub
   Releases + tags -- delete the release and tag, fix, and re-tag. There is
   no separate un-publish step.

No tag was created and no release was published as part of this
distribution setup -- that step is deliberately left for whoever decides
it's time to cut the first real release.
