---
title: WIT contract
description: The kernel world a guest implements — its export, its imports, and the text capability under active integration.
sidebar:
  order: 5
---

The current world is defined in `crates/instar-kernel/wit`. The repository
source is normative and experimental.

```wit
package instar:kernel@0.1.0;

world kernel {
    import kernel-runtime;
    import kernel-ui;
    import ops;
    import instar:text/text@0.1.0;
    export run: async func() -> result<_, string>;
}
```

## Guest export

```wit
run: async func() -> result<_, string>;
```

Returning `ok` ends the application normally. Returning an error or trapping
ends the guest generation and enters host failure handling.

## Runtime import

```wit
next-event: async func() -> result<list<u8>, runtime-error>;
```

The guest suspends here until the host delivers a semantic event or begins
shutdown. Suspending here must cost nothing: no timer wakes the component to
ask whether it has work.

## UI import

```wit
commit: async func(
    batch: list<u8>,
    text-views: list<borrow<text-view>>,
) -> result<commit-result, commit-error>;
```

A successful result includes the accepted revision. Errors distinguish an
invalid batch, a refused attachment, a commit already in flight, a stale
generation, and host unavailability — see the
[error taxonomy](/docs/reference/errors).

The second parameter is the attachment side table. A `TextView` node in the
batch names a *slot* in this list; the borrow is resolved during admission and
never retained past the commit.

## Operations import

The `ops` interface starts, awaits, and cancels host-owned long-running
operations by numeric ID. Cancelling an operation resolves its waiter without
ending the guest; tearing down the Store remains the whole-generation boundary.

This is why cancellation appears at two scales in one file, and why only one of
them is visible to the guest.

## Text capability

The optional `instar:text` package defines host-owned buffers and views with
explicit revisions. It supports buffer/view creation, reads, compare-and-edit,
selection, and destruction. The guest retains canonical application data; the
host resource provides native editing/presentation state under explicit IDs.

This interface is currently changing as Phase 3 integration proceeds. Generate
bindings from the checked-out WIT files rather than reproducing declarations.

## Byte protocol

UI snapshots and events are byte-defined inside the WIT lists. The current wire
protocol version is **9**. Version mismatch is a refusal, never a best-effort
parse. See the [wire protocol reference](/docs/reference/protocol),
`docs/PROTOCOL-0.md`, and `instar-ui-protocol/src/lib.rs` for limits and exact
encoding.
