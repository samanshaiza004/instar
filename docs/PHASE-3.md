# Phase 3 — Userland authority pivot

Phase 3 treats the guest as the only authority for application text. The host
transports native input, validates bounded presentation requests, owns retained
geometry and rendering, and never keeps a recoverable document, selection,
undo stack, or composition projection.

## Contract

```text
native input / IME → targeted Surface events → guest document and policy
→ bounded TextLayout queries → independent Surface scene → pixels
```

`Surface` is a semantic leaf identified by its existing generational `NodeKey`.
It has ordinary layout/style, explicit focusability, and declared interests,
but no resource handle or attachment table. Its retained scene survives a
same-key commit, visibility change, and resize; removal, kind change, key
generation change, and runtime teardown retire the scene and input state.

The independent `instar-surface-protocol` is a bounded, zero-dependency V0
display list: finite rectangles, rounded rectangles, clips, affine transforms,
and references to immutable TextLayout objects. Scenes are limited to 1 MiB,
65,535 commands, 4,096 layout references, and stack depth 64. Updates are
single-flight per runtime generation and Surface. Capability authority is
resolved before scene decoding; an invalid update leaves the previous scene
and revision intact.

`instar-text-layout` owns the host-only Parley seam. A guest layout handle is a
generation-owned lease; a retained scene holds an internal `Arc` to the same
immutable shaped object, never a Wasmtime resource or copied glyph data.
Geometry and rendering therefore use one object. V0 currently uses a
provisional 4 KiB text bound, with structural bounds of 4,096 visual lines,
4,096 clusters, 8,192 selection rectangles, and 4,096 live layouts per
generation. The bound is intentionally provisional pending the documented
4/8/16 KiB reference benchmark; it is not a document-size limit.

Native input is neutral. Raw keys preserve stable logical/physical names,
location, modifiers, repeat, and transition; Surface-local pointer, wheel,
focus, and IME events are delivered without host editor policy. Coalesced
preedit, pointer movement, wheel, and metrics cannot overtake ordered events.
If an ordered event cannot enter the bounded queue, the generation terminates
through the out-of-band lifecycle channel rather than trying to enqueue an
overflow notification into the full inbox. Metrics barriers preserve an active
text-input session and suppress only newly derived candidate geometry.

## Guest proof

`instar-editor-core` is a first-party userland convenience library, not part of
Instar’s semantic contract; applications may replace any or all of it. It uses
Crop for rope storage and exposes byte/grapheme/CRLF helpers, positions,
selections, atomic edits, revision, and undo/redo without host identifiers or
synchronization state.

The closure application is a guest-owned Scratchpad. It sends only bounded
visible hard-row slices and overscan to TextLayout, projects arbitrary
multi-line preedit transiently, preserves the empty-preedit-before-commit
target, and commits without preedit. Its novel-widget proof is two carets:
`abc` with carets at 1 and 3 plus commit `X` becomes `aXbcX`; the host sees only
raw input, layout requests, scene commands, and pixels.

## P0 deletion and history

The former host-replica implementation is archived at
`archive/phase3-host-replica` (`445acaa`) and its record is preserved with an
explicit warning at `docs/history/PHASE-3-HOST-REPLICA.md`. The active branch
contains no `instar:text` WIT resources, TextView attachment vocabulary,
host-side document synchronization, or `NODE_TEXT_VIEW` protocol node.

The architectural decision is recorded in
`docs/adr/0001-userland-text-authority.md`. There is no protocol compatibility
layer: old capabilities and old scene formats are rejected rather than
translated.

## Evidence

Current foundation checks:

```text
cargo test -p instar-editor-core
cargo check --workspace
```

The Surface codec includes round-trip, truncation/mutation, stack-balance, and
slot-validation tests. TextLayout tests cover immutable shared objects,
bounded selection output, cursor validation, and navigation wrappers. The
editor-core tests cover revisioned edits, undo/redo, CRLF grapheme movement,
and the two-caret descending batch.

Native IME candidate-window smoke checks and the full guest Scratchpad wiring
remain the next execution step; they do not change the authority contract.
