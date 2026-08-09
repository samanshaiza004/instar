# Instar wire protocol, revision 0

The bytes between a guest and the host. Two messages, both little-endian, both
hand-written.

> **Experimental. No compatibility promise.** This is the Phase 1 protocol and
> it is expected to change — the version byte exists so that a change is a
> refusal rather than a misparse. `PROTOCOL_VERSION` is **1**; revision 0 refers
> to the design generation, not the byte.

Normative source: `crates/instar-ui-protocol/src/lib.rs`. If this document and
that file disagree, the file is right and this is a bug.

## Principles

**Encoding is manual and byte-defined.** No Serde, no bincode, no `repr(C)`. A
format a guest depends on should be something a human can implement from a
specification, in any language, without linking the host's toolchain.

**The protocol layer reports structure; it does not judge meaning.**
`decode_batch` answers "do these bytes describe *a* tree" — well-formed
pre-order, in bounds, valid UTF-8. Whether that tree makes *sense* — exactly one
root, outermost, no duplicate keys — is `instar-ui`'s call, and produces a
different error type. A guest author needs to tell "your message is malformed"
apart from "your message is impossible".

**Every limit is checked before allocation.** The input is untrusted, and a
guest is exactly the party who would send 4 GB of node headers.

**A guest cannot express geometry.** There is no rectangle in this format. The
layout section was removed outright rather than deprecated, so a guest cannot
become authoritative over pixel positions even deliberately.

## Limits

| Limit | Value | Why |
|---|---|---|
| `MAX_BATCH_BYTES` | 1 MiB | checked before anything is parsed |
| `MAX_NODES` | 4096 | bounds the allocation a header can request |
| `MAX_DEPTH` | 64 | bounds recursion during assembly |
| `MAX_TEXT_BYTES` | 4096 | per node |
| `MAX_LENGTH` | 16384 | any fixed dimension, padding, or gap; bounds downstream layout arithmetic |

## Batch: guest → host

An interface description. Magic `IUI1`.

```text
┌────────────────────────────────────────────┐
│ "IUI1"                            4 bytes  │
│ version = 1                       1 byte   │
├────────────────────────────────────────────┤
│ section = SECTION_TREE (1)        1 byte   │
│ node_count                        u16      │
│ node × node_count                          │
├────────────────────────────────────────────┤
│ section = SECTION_END (0)         1 byte   │
└────────────────────────────────────────────┘
```

### Node

Emitted in **pre-order**: each node is immediately followed by exactly
`child_count` children. There are no explicit end markers — the counts are the
structure, and counts that do not describe one well-formed tree are
`MalformedTree`.

```text
┌────────────────────────────────────────────┐
│ kind                              1 byte   │
│ key                               u32      │
│ flags                             1 byte   │
│ text (only for TEXT and BUTTON)            │
│   ├ length                        u16      │
│   └ utf-8 bytes                            │
│ width  tag / value                1 + u16  │
│ height tag / value                1 + u16  │
│ padding                           u16      │
│ gap                               u16      │
│ child_count                       u16      │
└────────────────────────────────────────────┘
```

**Text presence is implied by `kind`, not by a flag.** `TEXT` and `BUTTON`
carry a string; `ROOT` and `COLUMN` do not. A reader that guessed differently
would desynchronize, which is why kind comes first.

| `kind` | Node | Text | Interactive |
|---:|---|---|---|
| 0 | `ROOT` | no | no |
| 1 | `COLUMN` | no | no |
| 2 | `TEXT` | yes | no |
| 3 | `BUTTON` | yes | when enabled |

| `flags` bit | Meaning |
|---:|---|
| `1 << 0` | `ENABLED` — a button the host will hit-test |

A **disabled button is not interactive at all**: the host refuses to hit it,
rather than delivering a click and trusting the guest to re-check.

### Dimensions

```text
tag 0  FILL     value ignored   take the parent's cross-axis extent
tag 1  CONTENT  value ignored   shrink to fit
tag 2  FIXED    value = pixels  exactly this many logical pixels
```

**`FILL` height is rejected** by `instar-ui`, not by the wire format. A column
of fill-height children has no defined distribution, and picking one silently
would be inventing layout semantics rather than implementing them. The wire can
say it; the layer above says no.

## Event: host → guest

Magic `IUE1`. One event kind exists.

```text
┌────────────────────────────────────────────┐
│ "IUE1"                            4 bytes  │
│ version = 1                       1 byte   │
│ kind = EVENT_CLICK (0)            1 byte   │
│ node key                          u32      │
└────────────────────────────────────────────┘
```

Ten bytes. A click is reported **only for a completed interaction** on an
enabled button that the host hit-tested itself; press, release, hover, and drag
never cross this boundary. Transient interaction state is the host's.

Events are bounds-checked guest-side by the same `Reader`, for the same reason
batches are host-side: a guest should not be crashable by a malformed host
event either. This is the direction people forget.

## The WIT contract

The transport for these bytes is `crates/instar-kernel/wit/kernel.wit`:

```wit
next-event: async func() -> result<list<u8>, runtime-error>;
commit:     async func(batch: list<u8>) -> result<commit-result, commit-error>;
```

Both are `async`, and both matter:

- **`next-event` suspends at zero cost.** That is Gate 0's finding and the
  premise of the whole runtime.
- **`commit` suspends until the host has accepted the interface as a usable
  presentation state** — after layout, not merely after the tree is swapped.
  The guest resumes with a revision or one of four verdicts.

```wit
variant commit-error {
    invalid-batch(string),   // malformed, or meaningless
    stale-generation,        // the committing generation was superseded
    host-unavailable,        // nobody is left to apply it
}
```

`host-unavailable` exists because `commit` is async: the batch has been handed
to a thread-affine owner and the guest is suspended waiting. If that owner
disappears mid-flight the guest must be told rather than left parked forever.
The host makes this structurally total — a dropped reply *is* `host-unavailable`.

## What a guest may rely on

- Bytes it commits are either fully applied or fully refused. There is no
  partial application: the tree swap is one assignment after all validation.
- A refusal leaves the previous interface standing. A malformed commit does not
  blank the window.
- Node keys are the guest's own. The host learns them from the wire and gives
  them back in events; it never invents one.
- Every commit is answered exactly once.

## What a guest may not rely on

- **Any of this staying the same.** No compatibility promise; see the top.
- Geometry. It cannot see any, and the host may lay out however it likes.
- Appearance. Colour, spacing, fonts, and press feedback are the host's.
- Being asked before the host does something. Close policy, crash presentation,
  and teardown are host decisions a guest cannot observe or veto.
