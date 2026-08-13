---
title: Error taxonomy
description: Every refusal a guest can receive — protocol decode errors, commit errors, attachment errors, runtime errors, and operation errors.
sidebar:
  order: 4
---

Instar refuses things explicitly. A guest is untrusted input, so nearly every
boundary has a named failure rather than a silent fallback, and the names are
part of the contract.

:::caution
The Rust and WIT sources in your checkout are normative. Variants move as Phase
3 proceeds, particularly around text attachments.
:::

## Protocol errors

Produced by `instar-ui-protocol` when the bytes themselves are wrong. Both
sides can raise these: the host decoding a batch, the guest decoding an event.

| Variant | Condition |
|---|---|
| `TooLarge` | The batch exceeds `MAX_BATCH_BYTES`. Checked before allocation. |
| `BadMagic` | The message does not start with `IUI1` or `IUE1`. |
| `UnsupportedVersion` | The version byte is not this build's `PROTOCOL_VERSION`. |
| `Truncated` | Input ended mid-field. Names the field it was reading. |
| `UnknownOpcode` | An opcode not defined in this revision, with its context. |
| `TooManyNodes` | More than `MAX_NODES` nodes. |
| `TooDeep` | Nesting deeper than `MAX_DEPTH`. |
| `TextTooLong` | A node's text exceeds `MAX_TEXT_BYTES`. |
| `InvalidUtf8` | Text is not valid UTF-8. |
| `LengthTooLarge` | A fixed dimension, padding, or gap exceeds `MAX_LENGTH`. |
| `InvalidFlexFactor` | A flex factor is not finite and within `0.0..=MAX_FLEX_FACTOR`. |
| `InvalidBounds` | A minimum exceeds its corresponding maximum. |
| `InvalidFontWeight` | A font weight outside CSS's 1–1000. |
| `MalformedTree` | The pre-order child counts do not describe one well-formed tree. |
| `TrailingBytes` | Bytes remain after the message ends. |

These are structural. Whether a structurally valid tree is *sensible* —
duplicate keys, unreachable nodes, an impossible root — is decided later, by
`instar-ui`, and surfaces as `invalid-batch`.

## Commit errors

Returned from `kernel-ui.commit`.

| Variant | Meaning | What a guest should do |
|---|---|---|
| `invalid-batch` | The batch failed to decode or validate. Carries a message. | Fix the snapshot. This is a bug in the guest. |
| `invalid-attachment` | A text-view attachment was refused, before any tree work happened. Carries an `attachment-error`. | See the nested table below. |
| `commit-in-progress` | Another commit from this generation is still outstanding, so this one was refused rather than queued. | Retry after the earlier commit resolves. |
| `stale-generation` | The committing generation is no longer current, so the host discarded the batch. | Nothing. A guest should not normally see this. |
| `host-unavailable` | The host side that owns the retained tree is gone or shutting down, so no verdict will ever arrive. | Stop committing; the application is ending. |

`stale-generation` exists so that a stale commit is *rejected explicitly*
rather than silently applied to a successor generation's state. By the time a
generation is superseded, its instance is usually already being dropped.

`host-unavailable` is only reachable because `commit` is async: the batch has
been handed to a thread-affine owner and the guest is suspended waiting. If
that owner disappears mid-flight, the guest is told rather than left parked on
a reply that is never coming.

## Attachment errors

Nested inside `invalid-attachment` rather than flattened into the commit error
list, because they are one subsystem's validation taxonomy. No variant carries
a slot number, key, or count: the refusal is about the side table as a whole,
and the host's job is to name the family of rule that was tripped, not to hand
the guest enough to re-derive the failure.

| Variant | Condition |
|---|---|
| `too-many-attachments` | The side table is larger than the maximum useful cardinality. No valid tree can reference more than `MAX_NODES` distinct slots, so entries beyond that cannot contribute. |
| `unavailable-text-view` | The entry exists, but its key does not resolve to a lease this generation owns — a stale incarnation, or cross-generation use. |
| `attachment-out-of-range` | The entry named a slot outside the side table it indexes into. |
| `text-view-already-attached` | Two different live `NodeKey`s reached one `TextViewId`. Duplicate entries naming the same view are legal; two distinct nodes claiming it are not. |

## Runtime errors

Returned from `kernel-runtime.next-event`.

| Variant | Meaning |
|---|---|
| `shutdown` | No more events. Leave the event loop and return from `run`. |
| `internal` | The host could not produce an event. Carries a message. |

`shutdown` is the normal end of an application, not a fault: a guest that
receives it should return `ok` from `run`.

## Operation errors

Returned from `ops.await-op`.

| Variant | Meaning |
|---|---|
| `cancelled` | The operation was cancelled — either because the guest asked through `ops.cancel`, or because the host cancelled it while tearing down the operation's generation. |
| `failed` | The operation ran and failed. Carries a message. |
| `unknown` | No such operation in this generation, including one belonging to a previous generation. |

## What has no error

Whole-generation teardown is not in the guest's world and has no variant. A
guest cannot request it and cannot observe it; from the guest's point of view
its generation simply ceases to exist. That asymmetry is deliberate — see
[Runtime model](/docs/concepts/runtime-model).
