---
title: Runtime model
description: How Instar runs a guest — one generation, bounded queues, zero idle polling, and a cancellation boundary at the Store.
sidebar:
  order: 1
---

Instar's runtime is event-driven. The guest exports one async `run` function
and imports async host operations. The host runs the component on a dedicated
runtime thread while the native event loop remains on the main thread.

## One guest generation

```text
MAIN THREAD                           RUNTIME THREAD
native event loop                    Wasmtime Store + component
     │                                      │
     ├─ semantic event ────────────────────▶ │
     │                                      ├─ guest wakes
     │                                      ├─ updates app state
     │ ◀──────────────────── UI commit ─────┤
     ├─ validate / diff / layout / render   ├─ waits for reply
     └─ reply ─────────────────────────────▶ └─ suspends at next-event
```

The queues are bounded. The main thread never blocks waiting for guest work,
and lifecycle control has a path that does not compete with ordinary traffic.

## Zero idle polling

`next-event` suspends the guest. Instar does not wake every component on a
10 ms tick to ask whether it has work. Gate 0 tests the property directly by
observing polls: an idle guest incurs none.

This is not a claim that the native process consumes literally zero resources.
The window system, process, and runtime remain resident. The claim is narrower
and useful: guest idleness does not become periodic guest execution.

## Generation is the cancellation boundary

A suspended WebAssembly task owns a Wasm stack that the host cannot safely
force-unwind operation by operation. Instar therefore treats the Store and
component instance as a **generation**. Cancellation tears down the generation
and starts another; fine-grained task cancellation is a guest protocol concern.

Every host-bound request carries the generation that made it. The host screens
stale work before decoding or allocating on its behalf.

There are two cancellations at two scales, and only one of them is visible to
the guest:

| Scale | Who asks | Effect |
|---|---|---|
| One operation | The guest, through `ops.cancel` | The operation's waiter resolves as `cancelled`; the guest task keeps running. |
| Whole generation | The host, by dropping the Store | The guest ceases to exist. It cannot request this and cannot observe it. |

## Concurrent host operations

The runtime uses Wasmtime's async Component Model support. A guest may wait on
more than one imported async operation; independent operations can progress
without a polling ticker. See the repository's `GATE-0.md` for the measured
acceptance experiment.

## Shutdown

Shutdown closes lifecycle control, wakes suspended work, and joins the runtime
thread. A guest that returns normally ends the application. A guest that traps
is reported through the host-owned failure path described in
[Failure and recovery](/docs/concepts/failure-and-recovery).
