---
title: Tests and gates
description: The workspace suite, Gate 0, the lint and release-profile checks, and the gates a machine cannot close.
sidebar:
  order: 2
---

## Workspace suite

```sh
cargo test --workspace
```

The suite covers the runtime, byte protocol, retained UI, layout, interaction,
bridge behavior, rendering structure, pixel output, shell behavior, and crate
layering.

## Gate 0

```sh
cargo test -p instar-kernel --test gate0
```

This builds a real `wasm32-wasip2` component and verifies idle suspension,
wake-up, concurrent async progress, generation teardown, and clean shutdown.

Gate 0 is the test that makes "idle means idle" a claim rather than a slogan:
it observes polls, so an idle guest incurring any is a failure, not a
regression to be argued about.

## Lints and formatting

```sh
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
```

Guests are separate workspaces, so the workspace-wide command never sees them.
They are linted in their own CI job — they are the code most likely to rot
unnoticed, since nothing imports them and they only run inside a build script.

## Release profile compile

```sh
cargo build --locked --profile dist --package instar-shell --bin instar
```

Release CI repeats this on every supported OS, packages the binary, verifies
the archive, installs it in isolation, and executes both help forms. Failures
are labeled by configuration, build, package, install, execute, or attestation
stage.

## What CI runs on three platforms, and why

The point of running on three OSes is not thoroughness for its own sake.
Instar's claims are about a *runtime*: "a guest suspends at zero cost" has to
hold on Windows' and Linux' scheduler and timer behaviour too, and "a click
round-trip stays prompt" has to hold wherever winit and Wasmtime actually run.
A single-platform green is evidence about one machine.

## Manual gates

CI deliberately does **not** cover the compositor. `softbuffer`'s `present()`
is the real platform presentation boundary, and asserting what happens past it
needs a display server and a human.

Native assistive-technology behavior has a platform checklist in
`docs/F4-SMOKE.md`. Automated AccessKit projection tests do not substitute for
that manual screen-reader acceptance pass.
