---
title: Verify a download
description: Check an Instar release archive against its SHA-256 digest and its GitHub artifact attestation.
sidebar:
  order: 5
---

Release archives are accompanied by detached SHA-256 files and a unified
`sha256.sum`. GitHub also records an artifact attestation for each native
archive.

## SHA-256

On Linux:

```sh
sha256sum --check instar-shell-x86_64-unknown-linux-gnu.tar.xz.sha256
```

On macOS:

```sh
expected=$(awk '{print $1}' instar-shell-aarch64-apple-darwin.tar.xz.sha256)
actual=$(shasum -a 256 instar-shell-aarch64-apple-darwin.tar.xz | awk '{print $1}')
test "$expected" = "$actual"
```

On PowerShell:

```powershell
$expected = (Get-Content .\instar-shell-x86_64-pc-windows-msvc.zip.sha256).Split()[0]
$actual = (Get-FileHash .\instar-shell-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash
if ($expected -ne $actual.ToLower()) { throw 'checksum mismatch' }
```

Do not execute an archive whose digest differs.

## Attestations

With the GitHub CLI installed:

```sh
gh attestation verify \
  instar-shell-x86_64-unknown-linux-gnu.tar.xz \
  --repo samanshaiza004/instar
```

This confirms that the archive digest appears in a signed attestation issued
for the Instar repository's GitHub Actions workflow. Checksum and attestation
answer different questions: the checksum detects changed bytes; the
attestation binds those bytes to a build identity.

| Question | Answered by |
|---|---|
| Did these bytes change in transit? | SHA-256 digest |
| Were these bytes produced by Instar's release workflow? | GitHub attestation |
| Is this the version I meant to fetch? | The release tag you downloaded from |

## What CI proves

For every configured target, release CI performs the complete chain:

```text
build → package → verify checksum → unpack → install → execute help
```

Tag builds additionally verify the published release assets and their public
attestations after downloading them back from GitHub Releases.

That chain is the reason the installer is a thin delegator: the artifact it
fetches has already been unpacked, installed, and executed once by CI on the
same platform you are installing on.
