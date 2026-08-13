---
title: 'Pre-0.3: the host is learning to hold text'
description: 'A Phase 3 release note about host-owned text buffers, retained UI, and why Instar is still deliberately small before 0.3.'
date: 2026-08-13
version: 'PRE-0.3 / PHASE 3'
tags: ['release', 'phase 3', 'text']
---

Instar is a native host for applications compiled as WebAssembly components. A
guest describes meaning. The host owns the window, geometry, input,
accessibility, rendering, and the boundary around every guest turn.

That is the sentence we have been testing since the first real guest opened a
real native window. Phase 3 asks whether the sentence still holds when an
application needs to edit text.

## What Phase 3 adds

Text is not just a longer `Text` node. It has a buffer, a view, a revision, a
selection, editing operations, and a lifetime that can outlive one snapshot.
Those are exactly the places where a guest-owned shortcut could quietly turn
into a second UI system.

The current work therefore keeps the boundary explicit:

- application state remains in the guest;
- editable text resources are created and owned by the host;
- snapshots attach a `TextView` by a checked, commit-local side table;
- edits use explicit revisions instead of “last write wins” guesses;
- a stale generation cannot spend work against a successor.

The wire is now at protocol revision 9. It can carry the `TextView` node and
the attachment capability, while the WIT contract describes the text resource
operations separately. The interface is useful enough to exercise, but still
experimental enough that we are not calling it stable.

## What is already solid

Phase 3 is built on the closed foundations below it:

- **Gate 0:** an idle guest suspends at `next-event`; no timer polls it.
- **Phase 1:** a real component runs in a native window through a bounded,
  host-owned bridge.
- **Phase 2:** retained semantic UI, layout, scrolling, focus, interaction,
  accessibility projection, and crash containment work as one host-owned
  presentation path.

The result is intentionally not a framework. There is still one command:

```sh
instar run path/to/component.wasm
```

There is no project manifest, no package format, no registry, and no
`instar new` waiting around the corner. Those are not missing polish. Each one
would publish a contract, and the project has not seen an application need
that contract yet.

## The honest pre-0.3 status

This is a developer preview, not a stability announcement. The first tagged
binary release is still open. The native screen-reader smoke pass remains a
manual gate. WIT, wire bytes, crate APIs, and the CLI can change.

The good news is that the project is now a much better thing to inspect. You
can build the included counter guest, open a native window, trigger a guest
failure, read the ownership split, and follow the text work in the actual
protocol and host code.

```sh
git clone https://github.com/samanshaiza004/instar.git
cd instar
cargo build --release \
  --manifest-path guests/counter/Cargo.toml \
  --target wasm32-wasip2
cargo run --release --package instar-shell --bin instar -- run \
  guests/counter/target/wasm32-wasip2/release/counter.wasm
```

## What comes next

Before 0.3, the work is less about adding nouns and more about closing the
claims we already make:

1. finish the text editing integration and its platform behavior;
2. run the native accessibility smoke pass on each supported OS;
3. publish the first four-target release with archive, checksum, attestation,
   install, and `--help` proof;
4. make the counter, calculator, and gallery examples easy to reproduce from a
   clean checkout;
5. document the smallest guest-side path without pretending the API is stable.

Instar is pre-0.3 because the boundary is still being earned. That is the
interesting part of the project, and the reason to try it now.
