---
title: Repository map
description: Where things live in the Instar checkout, and which documents are normative.
sidebar:
  order: 3
---

A checkout is a Cargo workspace of native crates, a set of separate guest
workspaces, an engineering record under `docs/`, and this website.

## Top level

| Path | Contents |
|---|---|
| `crates/` | The native workspace: protocol, kernel, UI, window, paint, renderer, host, shell, SDK, text. |
| `guests/` | Guest components, each its own Cargo workspace with its own `target/`. |
| `docs/` | The engineering record: architecture, protocol, phase plans, results, baselines. |
| `website/` | This site — an Astro project whose `src/content/docs/` holds the guide. |
| `scripts/` | Release-checking helpers. |
| `dist-workspace.toml` | cargo-dist configuration: targets, installers, archive shapes. |
| `rust-toolchain.toml` | The pinned toolchain and target, applied automatically by rustup. |
| `deny.toml` | Dependency policy. |

## Which documents are normative

The guide you are reading is the readable source. When it disagrees with the
repository, the repository wins.

| Question | Normative answer |
|---|---|
| What the bytes mean | `crates/instar-ui-protocol/src/lib.rs` |
| What the guest world is | `crates/instar-kernel/wit/` |
| How crates may depend on each other | `docs/ARCHITECTURE.md`, plus the dependency-set tests |
| Why the protocol looks like this | `docs/PROTOCOL-0.md` |
| What the current phase is doing | `docs/PHASE-3.md` |
| What was measured, and when | `docs/baselines/`, `docs/*-RESULTS.md` |
| What the manual accessibility pass covers | `docs/F4-SMOKE.md` |

## On the historical documents

Baseline reports, phase results, and the design ledger are evidence. They
record what was true when they were written, including terminology the project
has since moved on from. They are not rewritten to look current — a measurement
whose context has been edited is no longer a measurement.

## Guests are not in the workspace

`guests/*` are separate Cargo workspaces on purpose. They compile for
`wasm32-wasip2` rather than the host target, and they may link only
`instar-ui-protocol` and optionally `instar-sdk`. Keeping them outside the
workspace is what makes that boundary a build-time fact instead of a
convention.

See [Architecture](/docs/concepts/architecture) for what each native crate is
responsible for, and [Contributing](/docs/development/contributing) for how a
change to any of it is argued for.
