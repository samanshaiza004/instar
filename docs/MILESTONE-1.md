# Milestone 1: Transactional Visible Counter

Milestone 1 runs one portable `wasm32-wasip2` component in a native window. The
guest declares a semantic tree and handles semantic activation events; the host
owns durable SQLite state, validation, layout, pixels, pointer interaction, and
the platform event loop.

The invariant is:

```text
validate staged semantic tree
→ commit SQLite transaction
→ install authoritative tree
→ publish committed patch to the renderer
```

Any failure before the database commit rolls back state and discards the staged
tree. A commit failure retains the previous tree and faults the component. Pixel
presentation is deliberately outside the database transaction: a renderer that
misses a patch recovers from a host-owned authoritative snapshot.

## State contract

Applications have an explicit validated identity. A state database stores typed
booleans, signed integers, UTF-8 text, and bytes. Guests receive no SQL or file
access. State imports are available only while the host is executing mount,
handle, or read-only resync lifecycle calls.

Default limits are 256 UTF-8 bytes per key, 1 MiB per text or byte value,
16,384 keys, 16 MiB total logical state, 1,024 writes per lifecycle call, and
4,096 state calls per lifecycle call. One entry consumes its key bytes, encoded
value bytes, and 32 bytes of fixed overhead. Boolean and integer values consume
one and eight encoded bytes respectively. All quota arithmetic is checked.

Every attempted import counts as a call. A set counts as a write only after its
phase, key, type, and per-value size are valid, but before write-budget and total
state quota checks. Deleting a present staged key counts as a write; deleting a
missing key does not. Deleting and recreating a key therefore counts twice.
Only committed state and usage metadata become durable.

Runtime database opening fails closed when schema, integrity, or usage metadata
does not match the stored rows. Offline verification reports the discrepancy
without values. Usage repair requires an explicit, non-overwriting backup and
repairs only the derived usage row.

## Failure contract

- A `rejected-event` response with zero write attempts rolls back and leaves the
  instance mounted.
- Any application error after a write attempt rolls back and faults.
- `invalid-state` and `internal` application errors always fault.
- Traps, limits, malformed output, invalid revisions, and commit failures roll
  back and fault.
- Automatic guest reconciliation is deferred; renderer recovery never executes
  guest code.

## Native host

The winit thread owns one window, the softbuffer surface, a renderer-side tree
mirror, geometry, pointer state, and redraw scheduling. A sequential controller
calls the existing Youth worker and returns ordered results through user events.
Pointer motion and resize remain host-local. A primary-button release activates
only the enabled button armed by the matching press. The window redraws only
when invalidated.

The provisional renderer supports root, vertical box, single-line debug text,
and button nodes. Its embedded bitmap font and framebuffer fixtures are
implementation evidence, not protocol compatibility guarantees.

## Non-goals

This milestone does not add a production renderer, styling, themes, keyboard
focus or input, IME, accessibility, menus, images, scrolling, drag and drop,
animation, effects, guest filesystem/network access, packages, migrations,
arbitrary SQL, multiple windows, custom surfaces, hot reload, component
composition, or async guest turns.

## Completion gates

1. The isolated state layer passes identity, quota, persistence, verification,
   backup, repair, locking, and phase tests.
2. Headless tests prove restart persistence and rollback after a guest writes
   state and then returns invalid semantic output.
3. A native counter displays and processes ordered pointer activation while the
   guest and database remain off the event-loop thread.
4. The same guest artifact, semantic snapshot, and provisional framebuffer pass
   the Ubuntu, Windows, and macOS host matrix.
