# Current protocol reference

This is the short reference for the active Instar protocol surfaces. The
normative implementations are `crates/instar-ui-protocol/src/lib.rs`,
`crates/instar-surface-protocol/src/lib.rs`, and
`crates/instar-kernel/wit/kernel.wit`. The former Phase 1 record is archived at
[`history/PROTOCOL-0.md`](history/PROTOCOL-0.md).

Instar has three related but separate contracts:

```text
semantic UI snapshot       guest → host       IUI1, protocol version 9
semantic/button input      host → guest       IUE1, protocol version 9
Surface-local input        host → guest       IUS1, protocol version 9
Surface presentation scene guest → host       ISF0, scene version 0
WIT capability boundary    host ↔ guest       kernel world, async imports
```

All byte protocols are manually encoded, little-endian, bounded before
allocation, and experimental. There is no compatibility promise.

## Semantic UI protocol

`instar-ui-protocol` carries the guest's retained semantic snapshot and the
host's semantic activation event. A snapshot is a bounded pre-order tree with
guest-assigned generational `NodeKey { id, generation }` values. Current node
kinds are:

```text
ROOT · COLUMN · TEXT · BUTTON · ROW · STACK · SCROLL · SURFACE
```

The guest supplies semantic structure, text, style, and layout intent: content
or fixed preferred sizes, min/max bounds, flex basis, grow/shrink, alignment,
distribution, display/visibility, clipping, padding, gap, text style, paint
style, and cursor intent. The host validates the snapshot, owns the retained
tree, computes layout, hit-tests, and lowers the result for presentation.

The guest cannot dictate semantic UI/window geometry, but a `Surface` may
describe Surface-local presentation geometry inside the rectangle allocated by
the host. Semantic node rectangles and window placement never cross this wire.

The host emits `IUE1` click events for completed activation of enabled semantic
buttons. Press, release, hover, drag, focus traversal, and scrolling remain
host-local unless they are explicitly part of a focused Surface's neutral input
contract.

Hard semantic limits include a 1 MiB batch, 4,096 nodes, depth 64, 4,096 bytes
of text per node, and bounded layout lengths. The protocol crate owns encoding,
decoding, and bounds; `instar-ui` owns semantic validation and interaction
meaning.

## Surface input and scene protocols

`Surface` is a semantic leaf whose scene is independently replaceable. Its
`IUS1` events are neutral and targeted by generational `NodeKey`:

```text
pointer down/up/move · wheel · raw key · focus
IME enabled/preedit/commit/disabled · metrics changed
```

They carry Surface-local coordinates or raw input data. They do not prescribe
editor behavior; guest code decides document, selection, composition, and
command policy.

`instar-surface-protocol` is a separate zero-dependency `ISF0` scene wire. A
scene contains bounded presentation commands:

```text
fill/stroke rect · fill/stroke rounded rect · clip stack
transform stack · draw immutable TextLayout by slot
```

Scene rectangles, clip rectangles, transforms, and text origins are local to
the host-assigned Surface rectangle. They cannot move or resize the semantic
Surface or its window. The scene is capped at 1 MiB, 65,535 commands, 4,096
layout references, and clip/transform depth 64. Scene updates are capability-
checked and single-flight per runtime generation and Surface; an invalid update
leaves the previous scene and revision intact.

## WIT capabilities

The `kernel` world exposes the runtime boundary rather than a widget API:

```wit
kernel-runtime.next-event() -> result<list<u8>, runtime-error>
kernel-ui.commit(batch: list<u8>) -> result<commit-result, commit-error>
text-layouts.create-layout(text, style) -> result<text-layout, layout-error>
surfaces.update-surface(target, scene, layouts) -> result<u64, surface-error>
surfaces.capture-pointer / release-pointer
surfaces.request-focus
surfaces.configure-text-input
ops.start / await-op / cancel
```

`next-event`, `commit`, Surface scene updates, layout creation, and operation
awaits are asynchronous where the guest may need to suspend. Text layouts are
host-owned immutable objects borrowed by a scene; they are not guest resources
or copied glyph buffers. Every commit and scene update has an explicit verdict,
and stale generations or unavailable owners cannot leave a guest parked
indefinitely.

The allowed guest-side Instar dependencies are intentionally separate from
these runtime capabilities: `instar-ui-protocol`,
`instar-surface-protocol`, optional `instar-editor-core`, and optional
`instar-sdk`. Guests cannot link the host, kernel implementation, layout engine,
window layer, renderer, or shell.
