# Instar Phase 2 — Retained UI Foundation

> **The question:** can Instar's retained UI service become a genuinely usable
> general desktop GUI *without contaminating the kernel or introducing a frame
> loop*?

Out of scope, deliberately: scratchpad, editor, files, audio, GPU, custom font
loading.

Guardrails carried from Phase 1: `instar-kernel` never learns about windows,
layout, or pixels; a guest links `instar-ui-protocol` and nothing else of
Instar's; no frame loop; and **transient interaction never needs a Wasm round
trip** — now extended from pressed-state to hover, focus, and scrolling.

---

## Stage 0 — host-side snapshot diffing [done]

### The finding that started it

Instar never had a mutation protocol. The wire format has two sections,
`SECTION_TREE` and `SECTION_END`; there is no `SetText`, no patch opcode. The
predecessor *did* have one (`youth-sdk`'s `set_text` and its `AppliedPatch`
pipeline); WP5/WP7A replaced it with full-tree replacement and nobody noticed,
because replacing seven nodes is free.

### The decision

> A guest commits a full UI **snapshot**. The host diffs it against the
> retained tree by `NodeKey` and applies only what changed. **Host nodes are
> not destroyed and recreated because another snapshot arrived.**

Chosen over guest-sent deltas because a guest that mis-tracks its own dirty
state cannot desync the host — structurally impossible rather than merely
tested for. Guest and host fully re-synchronize on every accepted commit, and
recovery is "send another snapshot".

Deltas remain a *measured* escape hatch, and the measurement now says not to
bother: see the ledger below.

### Two counters, not one

```text
commit_sequence   every accepted guest submission; what the guest sees, so its
                  synchronization does not depend on whether the host found the
                  snapshot interesting
tree_revision     the version of the retained UI state; advances only when the
                  diff found something; what caches key off
```

### Measured ledger

At 4,000 nodes, changing one text leaf (Apple Silicon, release):

| layer | cost | O(?) |
|---|---:|---|
| decode | 108 µs | O(tree), inherent |
| validate | 177 µs | O(tree), inherent |
| diff | 523 µs | O(tree), flat in change size |
| layout | 11,356 µs | **O(tree)** |
| lower | 900 µs | **O(tree)** |
| raster | 15,040 µs | **O(tree)** |

**Snapshot transport is ~0.8 ms of a ~27 ms frame — 3%.** The expensive work is
host-side recomputation, so the architecture stays `full snapshot → host diff →
incremental host work` rather than pivoting to deltas. One changed leaf and
four hundred changed leaves cost the same today; closing that is the rest of
Phase 2.

The one target already met: an identical re-commit costs 0.8 ms instead of
27 ms, because an empty `ChangeSet` skips layout, lowering, and raster
entirely.

`MAX_NODES` stays **4096**. At 28 ms per change, raising it would expand
supported input faster than supported performance. Revisit only once a one-leaf
change at 4k is comfortably inside a frame budget.

---

## Stage 1 — Parley [done]

`TEXT_METRICS` is deleted. One shaped result drives both measurement and
painting.

> Text is shaped exactly once, in logical space. `instar-ui` shapes at
> `scale = 1.0` with `quantize = false` and never receives display scale.
> `instar-host` converts shaped positions **and font ppem** to physical space
> during paint lowering. Pixel quantization and hinting are renderer concerns.

Multiplying `font_size` rather than wrapping a logical run in a scale
transform, because Vello uses ppem to select bitmap and colour glyph strikes.

Letting `instar-ui` see scale would make logical layout depend on which monitor
the window occupies — Parley multiplies size and spacing internally, and
recovering logical geometry by dividing back down lets physical quantization
change wrapping between scale factors. **Moving a window between monitors must
not reflow text.**

### The cached object is Parley's `Layout`

`ShapedText` is the extracted render artifact, never the thing re-broken:

```text
same text/style, same width    → reuse everything
same text/style, width changed → break_all_lines + align, re-extract; NO reshape
text or style changed          → rebuild the Layout, break, extract
```

Resize is why: it changes every text node's width while changing none of their
text.

**Extraction is a finalization pass, not a measurement side effect.** Taffy
calls a measure closure many times per node under `MinContent`/`MaxContent` to
resolve flex distribution, and its last call is not necessarily at the node's
final width. So `measure()` answers questions and caches; `finalize()` runs
after Taffy has real geometry. Costs at most one extra line-break; never a
second shape.

`ShapingStyle` is named to make it obvious that only shaping-affecting
properties belong in it — it is hashed as a cache key, so adding colour would
silently destroy the reuse.

**Known limitation:** Parley documents that content-width calculation may be
inaccurate for mixed-direction text. Recorded, with a bidi fixture that asserts
what actually happens rather than a value we cannot justify.

### The ledger

Warm counter click, 1,000 cycles, release:

| | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|
| Phase 1 (fake metrics) | 206 µs | 215 µs | 225 µs | 475 µs |
| Parley, first cut | 46 ms | 82 ms | 134 ms | 301 ms |
| **Parley, fixed** | **4.94 ms** | **5.75 ms** | 11.5 ms | 105 ms |

`TextStats` for one warm click, ten text-bearing nodes, one changed label:

```text
rebuilt       1
relinebroken  1
reused        9
extracted     1
```

Two defects, both in how the cache was *used* rather than in shaping:

1. `finalize` re-extracted unconditionally, so a detected reuse still paid a
   full extraction. Reuse: 2.53 ms → **83 ns**.
2. `measure` called `break_all_lines` for every constraint. Taffy probes
   `MinContent`, `MaxContent`, and `Definite` per node per pass, so each probe
   re-broke the cached layout — 29 re-linebreaks for 10 nodes — and each
   invalidated the extracted artifact, making one changed label re-extract
   every node on screen. Round trip: 43.9 ms → **5.36 ms**.

Intrinsic queries are now answered from a per-entry `ContentWidths` cache.
Caching those ourselves is **required**, not an optimization: Parley
deliberately stopped caching them internally, so removing the line-break
mutation alone would have left Taffy's repeated min/max probes recomputing
them every time.

Shaping itself was never the problem — an explicitly-named monospace face
rebuilds in ~370 µs.

### Open

- **The 105 ms max**, 20× p50 and unexplained by any measured layer. Needs an
  outlier-only trace including thread wake delay before anyone calls it noise.
- **`system-ui` selection** costs ~7× an explicit face per shape, and
  **`TextContext::new()` costs ~342 ms** of system font enumeration. Neither
  enters the warm path, so both are startup/font-cache debt to be re-baselined
  against the current numbers rather than fixed on the strength of the old
  ones.

---

## Stage 2 — protocol v2

### Generational `NodeKey` is the first change, before anything else

```rust
struct NodeKey { id: u32, generation: u32 }
```

**Why it is mandatory rather than cosmetic.** Host-side retirement (landed in
Stage 1) cancels a press whose node disappeared, which closes
press→remove→reuse→release. It cannot close this:

```text
ButtonActivated(7) queued  →  guest removes node 7  →  guest re-adds node 7
                           →  old event delivered   →  lands on the NEW node 7
```

`GuestEvent` is opaque bytes by then — no node key, no revision, nothing the
host can match against or invalidate. Both existing guards miss it: `KindChanged`
does not fire because a button replaced by a button is the same kind, and the
geometry barrier does not fire because the scale never moved. It is reachable
by a fast double-click, not by exotic timing.

Tree revision is *not* the fix. A newer tree revision does not invalidate an
action — an unrelated label may have changed. Node generation answers precisely
"is this still the same logical node?".

### Lifecycle

```text
first lifetime:  (7, 0)
removed
reuse id 7:      (7, 1)
removed
reuse id 7:      (7, 2)
```

Old generations can never become live again within one `RuntimeGeneration`.

### The host enforces monotonicity, because the guest chooses ids

A buggy or hostile guest could otherwise resend `(7, 0)` after removal.

```text
id never seen before   →  generation must be 0
id currently live      →  generation must match exactly
id retired             →  generation must be > previous
```

### The ledger must itself be bounded

`MAX_NODES` bounds *live* nodes; it does not bound a long-running guest burning
new ids forever. So:

```text
per RuntimeGeneration:
  max distinct node ids ever observed = 65,536
  exceeding it rejects the snapshot
  the whole ledger dies with the Wasm runtime generation
```

Same reasoning as the bounded queues and the bounded crash surface: a structure
that grows with guest behaviour and never shrinks is a slow failure with no
counter attached.

### It also fixes accessibility identity

AccessKit requires a stable `NodeId` unique within its tree. Two `u32`s pack
losslessly:

```rust
accesskit_id = ((generation as u64) << 32) | id as u64;
```

So remove-then-reuse automatically becomes a *new* accessibility object rather
than recycling one a screen reader may still hold a reference to. This makes
generational keys a Stage 4 correctness input, not only a Stage 2 tidy-up —
and the reason to land them before focus, hover, scrolling, and AccessKit each
add another stale-reference surface.

### Rest of Stage 2

Node kinds `Row`, `Stack`, `Scroll`. Layout: `min`/`max`, alignment, flex grow,
display, visibility, overflow. Style: foreground, background, border, corner
radius, font role/size/weight, padding, gap, cursor. **No opacity** — a property
by that name will be assumed to composite a subtree, and per-node paint alpha
is not that. `StrokeRoundedRect` is needed in `instar-paint` and the Vello
backend: `StrokeRect` currently rejects any width but 1.0 and there is no
rounded stroke.

Grid is not exposed, even though Taffy supports it.

---

## Stage 3 — focus, keyboard, scrolling

`instar-window` gains `KeyboardInput`, `MouseWheel`, `Focused` translation and
a cursor-icon output — it still never learns what a `NodeKey` is.

### Scroll semantics, frozen before the node exists

```text
Scroll owns transient:  offset_x/y, max offset, scrollbar hover/drag state
layout determines:      viewport rect, content extent
paint:                  children clipped to viewport, translated by -offset
hit test:               clip first, then transform into content coordinates
guest:                  never participates in wheel/touchpad response;
                        no scroll event unless explicitly subscribed
content shrinks:        offset = clamp(offset, new_extent)
removed / Display::None: transient scroll state is dropped with it
```

The stage's acceptance is a test proving **zero `SendToGuest`** for hover, focus
movement, and wheel events while each still produces a `Render`.

---

## The retirement invariant

> Any host transient state referencing a removed `NodeKey` is retired before
> the new snapshot becomes interactive.

Landed for pressed-state in Stage 1 and for the text cache in the same place.
Focus, hover, pointer capture, and scroll offsets join it as they arrive —
`Interaction::retire` is deliberately the single site that answers "the node is
gone, forget everything about it".

Note what it *cannot* reach, and why Stage 2's generational keys are required:
anything already encoded and queued for the guest.


---

## Method

Two rules earned the hard way during Stage 1.

### Performance claims require the same build profile and direct instrumentation

A debug reading of 386 ms was reported against a release baseline of 206 µs and
sent an investigation hunting an architectural defect that release measured at
5 ms. The defect was real; the number that raised the alarm was 75× the number
that mattered.

Separately, a mechanism was inferred from arithmetic that fit — 11 nodes ×
~35 ms ≈ 386 ms, therefore every node must be reshaping. It was wrong.
`rebuilt` was 1 the whole time; the cost was 29 speculative line-breaks.

> **Arithmetic inference may suggest where to look. It may never establish
> cause.** Instrument the thing, in the profile that matters.

This is why the bridge tests now assert different properties per profile:

```text
debug    completion, ordering, cancellation, no-hang, boundedness
release  latency: p50 ≤ 5 ms, p95 ≤ 8 ms, p99 ≤ 16 ms, max ≤ 250 ms
```

`max` is a deadlock and outlier guard, not a performance target, which is why
it sits three orders of magnitude above p50.

### `measure()` is observational

> **`measure()` may perform temporary work needed to answer the current sizing
> query, including line-breaking for `Definite(width)`, but it must not mutate
> finalized presentation state or invalidate reusable artifacts based on
> speculative constraints.**

```text
MinContent / MaxContent
→ intrinsic query only
→ cached ContentWidths
→ no line-break mutation

Definite(width)
→ may line-break temporarily to compute height
→ must not update finalized_width
→ must not invalidate shaped artifact

finalize(actual_width)
→ owns finalized_width
→ owns persistent line-break state
→ owns ShapedText extraction
```

The invariant that matters is not "measure never mutates" — a real height needs
a real break — but that **speculative Taffy probes cannot poison the finalized
cache**.

An earlier wording forbade line-breaking outright. That was stricter than the
code could be and stricter than correctness requires: the first seam forbade
*extraction* as a measurement side effect and was right, but said nothing about
line-breaking, which had the identical hazard and cost 9× the latency. The
answer is not to ban the operation but to name what it may not touch.

When a rule names one operation as forbidden, the next question is which
sibling operations share its hazard.
