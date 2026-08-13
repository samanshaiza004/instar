---
title: Wire protocol
description: The byte format Instar's UI snapshots and events travel in — magic, version, sections, opcodes, and the hard limits applied to every decode.
sidebar:
  order: 3
---

Snapshots and events cross the guest boundary as byte lists inside the WIT
interfaces. `instar-ui-protocol` defines that encoding and has zero
dependencies, so both sides can decode without pulling anything native or
anything guest-specific.

:::caution
`crates/instar-ui-protocol/src/lib.rs` is normative. This page describes the
shape of the format and the bounds it enforces; regenerate anything you depend
on from the source in your checkout, not from here.
:::

## Version and magic

| Constant | Value | Purpose |
|---|---|---|
| `PROTOCOL_VERSION` | `9` | The wire revision this build speaks. |
| `BATCH_MAGIC` | `IUI1` | Leading bytes of a committed UI batch. |
| `EVENT_MAGIC` | `IUE1` | Leading bytes of a host-to-guest event. |

The magic identifies the format; the version byte identifies the revision. The
magic therefore stays put across version bumps.

A version mismatch is a refusal, never a best-effort parse. A host that speaks
9 does not attempt to read an 8, and says so:

```text
wire version 8 is not supported (this build speaks 9)
```

The practical consequence for guest authors: rebuild the guest from the same
checkout as the host. The repository's build scripts track WIT and protocol
sources so ordinary builds do not retain a stale embedded component.

## Limits

Every decode applies these bounds before allocating on the sender's behalf.
They are a security boundary rather than a tuning surface — exceeding any of
them rejects the message.

| Limit | Value | Bounds |
|---|---:|---|
| `MAX_NODES` | 4096 | Nodes in one tree. |
| `MAX_DEPTH` | 64 | Nesting depth. |
| `MAX_TEXT_BYTES` | 4096 | Bytes of text on a single node. |
| `MAX_BATCH_BYTES` | 1 MiB (`1 << 20`) | Size of an entire encoded batch. |
| `MAX_LENGTH` | 16384 (`1 << 14`) | Largest fixed dimension, padding, or gap, in logical pixels. |
| `MAX_FLEX_FACTOR` | 1024.0 | Largest flex grow or shrink factor. |

The batch-byte limit is checked before allocation, which is why a hostile guest
cannot turn a commit into an out-of-memory condition.

## Node opcodes

Seven kinds, deliberately. The layout vocabulary is meant to stay small enough
to reason about completely; every kind added here is one the host must lay out,
hit-test, and paint forever.

| Opcode | Node | Notes |
|---:|---|---|
| 0 | `Root` | The single outermost node. Fills the viewport. |
| 1 | `Column` | Stacks children vertically. |
| 2 | `Text` | Displays text. Measured by the host, not sized by the guest. |
| 3 | `Button` | Interactive, with a text label. |
| 4 | `Row` | Stacks children horizontally. |
| 5 | `Stack` | Overlaps children at the content-box origin; later children paint over earlier ones. |
| 6 | `Scroll` | A retained viewport over exactly one child, with a host-owned scroll offset the guest can neither read nor set. |
| 7 | `TextView` | A surface showing a host-owned text view. A leaf. |

`TextView` carries an **attachment slot**: an index into *this commit's*
borrowed handle table, resolved during admission and never retained. It is not
a resource identity. That distinction is what keeps a text view's lifetime a
host concern rather than something a guest can smuggle across commits.

## Structure, not semantics

The protocol crate reports what the bytes say. It does not decide whether what
they say is sensible: duplicate keys, unreachable nodes, and nonsense
hierarchies are `instar-ui`'s to reject.

Keeping that line sharp is what stops semantic rules from quietly becoming wire
rules. In practice it means a batch can be perfectly well-formed at the byte
level and still be refused a moment later, and the two refusals come from
different places for different reasons.

## Events

Events are encoded the same way, with their own magic. The current event set is
small:

| Opcode | Event | Payload |
|---:|---|---|
| 0 | `Click` | The full `NodeKey` — a `u32` id and a `u32` generation, little-endian. |

Host-to-guest events are bounded and validated on the guest side too, for the
same reason batches are bounded on the host side: a guest should not be
crashable by a malformed host event either.

## Identity on the wire

A `NodeKey` is an id plus a generation. Both travel with every event so a stale
event naming a retired node can be rejected rather than delivered to whatever
now holds that id. See [UI vocabulary](/docs/reference/ui-vocabulary) for the
identity rules a guest is expected to follow.

## Related

- [Error taxonomy](/docs/reference/errors) — every refusal these bounds produce.
- [WIT contract](/docs/reference/wit) — the interfaces these bytes travel in.
- `docs/PROTOCOL-0.md` in the source tree — the design record.
