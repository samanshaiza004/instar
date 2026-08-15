# Phase 3 — Userland authority pivot

Phase 3 treats the guest as the only authority for application text. The host
transports native input, validates bounded presentation requests, owns retained
geometry and rendering, and never keeps a recoverable document, selection,
undo stack, or composition projection.

## Status

| Area | Status |
|---|---|
| Architecture decision | **Frozen.** Application text, selection, undo, and composition policy remain guest-owned. |
| Component seam | **Implemented.** Scratchpad exercises the real WIT, Surface input, TextLayout, scene, retained-tree, and pixel path. |
| Latency closure | **Pending a valid rerun.** The initial 1 MiB reference benchmark is a provisional failure at 13.1 ms p95; the 5 ms target remains unchanged. |
| Native IME smoke | **Pending.** Logical candidate geometry is covered; native platform candidate-window behavior still needs a manual smoke check. |
| Containment findings | **Open.** The latency/containment investigation and follow-up byte-size × line-count × caret-position matrix remain active. See [`DOS-STARVATION-AUDIT.md`](DOS-STARVATION-AUDIT.md). |

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

`guests/scratchpad` is the guest-owned policy proof. It keeps its document and
carets in `instar-editor-core`, projects arbitrary multi-line preedit
transiently, preserves the empty-preedit-before-commit target, and applies a
two-caret commit (`abc` with carets at 1 and 3 plus `X` becomes `aXbcX`). The
component adapter now commits a real Surface tree, requests bounded
immutable TextLayouts for visible hard rows plus two-row overscan, submits an
independent scene, routes pointer focus and native IME commits, and derives
candidate geometry from the same layout used for caret paint. The joined
`instar-shell` Scratchpad seam test observes the retained scene revision and
rasterized pixels while the semantic tree revision remains unchanged.

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

The remaining native-platform candidate-window smoke check is an environment-
dependent manual check; the automated joined seam covers logical candidate
geometry, multi-row preedit projection, empty-preedit-before-commit ordering,
and guest-owned pixel changes without host document state.

## Latency gate: UNVALIDATED (pre-fix reference run: FAIL, 13.1 ms p95)

The userland-authority pivot adopts a new, stricter Phase 3 typing target:
**p95 native-input → rasterized-pixels ≤ 5 ms**, measured end-to-end through
the real `guests/scratchpad` component (`benchmarks/text-latency`).

Current status, precisely:

```text
pre-fix run:      provisional FAIL, 13.1 ms p95 (historical evidence, kept)
root cause:       repeated visible-row TextLayout creation in present()
                   (NOT line_of_byte/line_range scanning -- see below)
fix:               landed on master, correctness tests green
current gate status: UNVALIDATED -- the benchmark's own completion harness
                   had a bug and must be repaired before any new number
                   from it means anything
```

Not PASS. Not "current FAIL caused by production latency" — the
`GateRun::measure_one` invariant the benchmark used to decide "did the
guest finish responding to this input" assumed one Surface revision per
queued guest message. `guests/scratchpad`'s dirty-presentation optimization
(skip `present()` for events that change nothing visible -- a key release,
a passive pointer move, focus-gained, `ImeEnabled`, metrics) correctly broke
that assumption in both directions: the harness could wait indefinitely for
a revision that correctly never arrives, and unsettled setup/trailing events
could satisfy part of an unrelated later sample. See
`benchmarks/text-latency/README.md` for the repair and why it explains the
stalls observed while re-confirming the fix below, without needing to
invoke system load as the explanation.

The initial pre-fix reference run is preserved at
`benchmarks/text-latency/results/initial-fail-macos-arm64-2026-08-14` and
kept, not overwritten, once a validated number exists — a before/after
record of the pivot's actual latency behavior is more useful than a
benchmark that happened to pass on its first (or third) run. That run still
narrows the problem usefully: ordinary interactive editing — typing, IME
commit, pointer, drag, scroll, a bounded-max text commit, all against a
small/empty document — was comfortably inside budget (p95 well under 1.2 ms
across every such workload); the failure was specific to editing *inside a
large preloaded document*, and its shape ruled out the leading suspect from
`docs/DOS-STARVATION-AUDIT.md`'s F1 (expensive shaping of one long unbroken
line): a 128 KiB pathological single-line document passed at 2.8 ms p95,
while much larger but ordinarily paragraphed 1 MiB / 10 MiB documents cost
8-13 ms for a single keystroke.

**Root cause, confirmed by direct code inspection, not by guesswork**:
`present()` in `guests/scratchpad` re-shaped every visible row (up to
`VIEWPORT_ROWS + OVERSCAN_ROWS` = 28) via `text_layouts::create_layout` on
*every* keystroke, with no reuse of a row whose line and text were
unchanged since the previous call. A small/empty document only ever has a
few visible rows, so it was fast; once a document has 28+ lines, every
keystroke paid for 28 full TextLayout shapes regardless of which row
actually changed. `Document::line_of_byte`/`line_range`
(`crates/instar-editor-core`) are Crop-native seeks, not scans, and were
never the bottleneck — an earlier draft of this section named them as the
leading suspect; that was wrong and is corrected here.

**Fix, landed**: `present()` now retains the previous call's row layouts and
reuses a row's existing `TextLayout` when its line number and bounded text
are unchanged, only calling `create_layout` for rows that actually differ.
All 5 tests in `crates/instar-shell/tests/scratchpad.rs` pass against it.

The gate stays UNVALIDATED, not PASS and not FAIL, until the repaired
benchmark harness produces a clean run under normal (uncontended) system
load. The 5 ms target is not being relaxed, and neither the row-cache fix
nor the harness repair reopens the userland-authority pivot: no host
document replica, no host-local edit shortcut.

### A second, distinct scan bug found by the coverage gap itself

Every workload above measures *insertion* against a large preloaded
document. Nothing measured *deletion* — and that gap was hiding a second,
independent `O(document)` bug: `Document::previous_grapheme_boundary`
(`crates/instar-editor-core/src/lib.rs`) scanned every grapheme from byte 0
up to the caret on every call, making Backspace `O(caret position)`
regardless of how well the rest of the editor performed. A document could
pass every graded insertion workload above while Backspace near its end
stayed pathological — exactly the failure mode "12 of 15 required workloads
measured" can hide: a correctly-implemented benchmark suite still has
whatever gap its own workload list has.

Fixed to a reverse, near-caret traversal (`crop::Rope`'s `Graphemes` is a
`DoubleEndedIterator`; walking backward from the caret with `next_back()` is
`O(log n)`, the same complexity class as the rope's own insert/delete, not
`O(caret position)`). `Document::next_grapheme_boundary` (forward-delete) was
already near-caret and needed no equivalent fix. Three workloads —
`backspace_at_end_1mib`, `backspace_at_end_10mib`, `delete_forward_large_doc`
— now cover deletion the way the original list covers insertion; see
`benchmarks/text-latency/README.md`'s "Workload coverage" section for the
full account, including why they're driven by hand-written loops rather than
the shared `workload!` macro (that macro's per-iteration re-click would
silently relocate the caret away from the document's end before every
measured Backspace).

**This is a separate bug from the keystroke-scaling FAIL above, not a fix for
it.** The leading suspect for that one is a guest-side line/caret *lookup*
(`primary_line`, `line_of_byte`-style calls) — different code, different
call path from the grapheme-boundary walk this fix addresses. Re-run the
gate after both are resolved before expecting the P95_TARGET to pass; fixing
Backspace alone does not flip the verdict recorded above.
