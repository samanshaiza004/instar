---
title: Glossary
description: The terms this documentation uses precisely, and what each one excludes.
sidebar:
  order: 8
---

Instar's documentation uses a small number of words in a deliberately narrow
way. Where a term has a looser everyday meaning, the entry says what it
excludes.

#### Guest

An application compiled as a WebAssembly component that implements Instar's
`kernel` world. A guest owns application state and semantic structure. It does
not own a window, a rectangle, a renderer, or the failure surface shown when it
dies.

#### Host

The native side: the event loop, layout, hit-testing, focus and scroll state,
accessibility projection, rendering, and presentation. "The host" means the
authority, not one crate — see [Architecture](/docs/concepts/architecture) for
which crate does what.

#### Generation

A Wasmtime `Store` plus one component instance, treated as a unit. A generation
is the cancellation boundary: tearing it down is how a guest is stopped,
because a suspended Wasm task cannot be safely force-unwound operation by
operation. Not to be confused with the generation field inside a `NodeKey`.

#### NodeKey

A node's full identity: a `u32` id chosen by the guest plus a `u32` generation.
Host-owned focus, scroll offset, pressed state, accessibility identity, and
stale-event rejection all key off the whole thing. Reusing an id for a
different meaning without advancing the generation hands the new node the old
one's host state.

#### Snapshot

A complete semantic tree committed by the guest. Instar has no partial update
message: the guest sends the whole tree and the host diffs it against the
retained one. "Snapshot" therefore never means "diff" or "patch".

#### Commit

The act of sending a snapshot, and the barrier around it. A commit resolves
only after the host has decoded, validated, applied, laid out, and lowered the
snapshot — so a successful commit means usable presentation state, not merely
accepted bytes.

#### Retained tree

The host's copy of the last accepted snapshot, plus the host-local state
attached to its nodes. A refused commit leaves it standing, which is why a
guest bug does not blank the window.

#### Semantic event

An event that names meaning rather than mechanism: an activation of a
particular node, not a click at a coordinate. Pointer positions, hover, press,
and scroll offsets never cross into the guest.

#### Text view / text buffer

Host-owned editable text resources with explicit revisions, from the
experimental `instar:text` package. The guest keeps canonical application data;
the host resource holds native editing and presentation state.

#### Attachment slot

An index into one commit's borrowed handle table, named by a `TextView` node.
It is resolved during admission and never retained. It is not a resource
identity, and it means nothing outside the commit that carried it.

#### Gate

A test that protects a claim the project makes out loud. Gate 0 tests idle
suspension by observing polls; the manual screen-reader pass in `F4-SMOKE.md`
is a gate a machine cannot close.

#### Developer preview

The current status. The CLI, WIT contract, wire protocol, and crate APIs can
change without compatibility. It is not a beta with a stability promise
attached, and it is not a release candidate.
