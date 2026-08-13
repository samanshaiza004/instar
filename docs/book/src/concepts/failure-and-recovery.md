# Failure and recovery

An application component is untrusted. Failure handling therefore belongs to
the native host boundary, not to UI supplied by the failing guest.

## Invalid commits

Instar checks byte limits before allocation, decodes the versioned protocol,
validates tree semantics, and only then swaps retained state. A refused commit
leaves the previous valid interface visible.

Common refusal categories include malformed structure, duplicate keys,
impossible root shape, illegal node attachments, and stale guest generation.

## Traps

When a guest traps, its generation ends. The native shell keeps ownership of
the window and presents a crash surface that the guest cannot replace. The
included counter exposes **Crash on purpose** so this path remains observable.

## Resource bounds

The UI protocol caps batch bytes, node count, depth, text per node, and layout
values. Runtime queues are bounded. Error text shown by the host is also
bounded. These are capability limits, not tuning suggestions.

## Host disappearance

Async commit waits for a thread-affine presentation owner. If that owner goes
away, the guest receives `host-unavailable`; it is not left suspended forever.

## Recovery model

Instar currently exposes a generation boundary rather than operation-level
forced cancellation. Restarting means constructing a fresh Store and component
instance. Durable application state and app-level restart policy are not yet a
public Instar product contract.
