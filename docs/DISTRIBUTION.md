# Instar runtime distribution

Instar distributes one thing: the native `instar` host executable from
`crates/instar-shell`. It loads and runs a WebAssembly Component Model guest
passed to `instar run`; the guest itself is not packaged or published by this
workflow.

There are four release targets, configured in `dist-workspace.toml`:

| Platform | Target |
| --- | --- |
| Apple Silicon macOS | `aarch64-apple-darwin` |
| Intel macOS | `x86_64-apple-darwin` |
| x86_64 Linux | `x86_64-unknown-linux-gnu` |
| x86_64 Windows | `x86_64-pc-windows-msvc` |

Each release also contains the generated POSIX shell and PowerShell
installers. Benchmark examples are kept in the repository but are not release
artifacts.

## Install

The installers place `instar` in `$CARGO_HOME/bin` (or the platform's normal
Cargo home when `CARGO_HOME` is unset). They do not install a guest, create an
application project, or define an application package format.

On macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/samanshaiza004/instar/releases/latest/download/instar-shell-installer.sh | sh
```

On Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/samanshaiza004/instar/releases/latest/download/instar-shell-installer.ps1 | iex"
```

Start a guest component explicitly:

```sh
instar run path/to/component.wasm
```

The component must implement Instar's current WIT contract. Build or obtain
that component separately; application distribution is intentionally outside
this runtime release.

## Uninstall

Remove the installed executable from the Cargo bin directory:

```sh
rm -f "${CARGO_HOME:-$HOME/.cargo}/bin/instar"
```

On Windows, remove `%CARGO_HOME%\bin\instar.exe` (or the equivalent default
Cargo home). The generated installer may leave its receipt under the user's
standard config directory (`$XDG_CONFIG_HOME`/`$HOME/.config` on Unix or
`%LOCALAPPDATA%` on Windows); remove the `instar-shell` receipt directory if
you want to remove that installation record as well. No Instar application
state is removed because this distribution does not create application state.

## Verify a download

Every native archive has a detached SHA-256 file, and the release includes a
unified `sha256.sum` file. From the directory containing a downloaded archive:

```sh
sha256sum --check instar-shell-x86_64-unknown-linux-gnu.tar.xz.sha256
```

To verify the complete release set on Linux:

```sh
sha256sum --check sha256.sum
```

GitHub Artifact Attestations provide build provenance for every native archive.
With the GitHub CLI authenticated to the repository, verify the archive after
checking its checksum:

```sh
gh attestation verify \
  instar-shell-x86_64-unknown-linux-gnu.tar.xz \
  --repo samanshaiza004/instar
```

The release workflow performs the same checksum and attestation checks after
downloading the published GitHub Release. It also runs both generated
installers in isolated Cargo homes and checks that `instar --help` succeeds.

On pull requests, the release matrix is intentionally enabled. Each target
compiles the `dist` profile, packages its native archive, verifies the detached
SHA-256, unpacks and installs that archive, and runs both `instar --help` and
`instar run --help`. The CI summary reports archive and binary byte sizes for
each target. The plan job checks that those targets and checksum artifacts
still match `dist-workspace.toml` before the matrix starts.

## Release procedure

The workflow is generated from `dist-workspace.toml` with `dist 0.32.0` and
reviewed for the runtime-only scope. A version tag such as `v0.0.2` triggers
four native builds, both installers, checksums, attestations, and the GitHub
Release. Before creating a tag, run `dist plan` and confirm that the plan lists
only the `instar` executable and the four configured targets.

This repository does not define `Instar.toml`, capsules, application
manifests, an `instar package` command, or an application bundle format.
Those decisions remain deliberately deferred until an external application
has established a real contract.
