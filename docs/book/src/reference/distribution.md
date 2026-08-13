# Distribution

Instar distributes one application: the native `instar` executable from the
`instar-shell` crate.

## Targets

| Platform | Rust target | Archive |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | `.tar.xz` |
| macOS Intel | `x86_64-apple-darwin` | `.tar.xz` |
| Linux x86_64 glibc | `x86_64-unknown-linux-gnu` | `.tar.xz` |
| Windows x86_64 MSVC | `x86_64-pc-windows-msvc` | `.zip` |

Guest `.wasm` components are not bundled with the host. Repository benchmarks
and test executables are not release artifacts.

## Installers

The release generates a POSIX shell installer and a PowerShell installer. Both
select an archive, unpack it, and install the binary under Cargo home. Verify
the release checksum and GitHub attestation separately when provenance matters.

### macOS / Linux

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://instar.samanshaiza.com/install | sh
```

### Windows

```powershell
irm https://github.com/samanshaiza004/instar/releases/latest/download/instar-shell-installer.ps1 | iex
```

## Release evidence

Each target archive has a detached SHA-256, appears in `sha256.sum`, and gets a
GitHub Artifact Attestation. CI installs the archive it just built and executes
`instar --help` and `instar run --help`. Tag CI then downloads the public
release and verifies it again.

## Versioning

The workspace is currently version `0.0.2`, but the absence of a published tag
means there may be no downloadable release yet. Versions below `1.0.0` carry no
compatibility promise. The protocol has an independent byte version and will
reject mismatches.
