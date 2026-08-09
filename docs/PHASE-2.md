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

### Generational `NodeKey` is the first change, before anything else [done]

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

### What landing it actually cost

`PROTOCOL_VERSION` is 2 and a key is eight wire bytes. Two consequences were
not obvious from the design:

**Duplicate detection had to move from the key to the id.** `(7, 0)` and
`(7, 1)` are distinct `NodeKey`s, so the existing check would have admitted
both into one snapshot — one id as two live nodes, which is exactly the
ambiguity the check exists to prevent. Identity for *uniqueness within a
snapshot* is the id; identity for *sameness across snapshots* is the pair.

**The ledger and the retained tree are one invariant, not two.** The rule is
that the ledger dies with the `RuntimeGeneration`, and clearing it at guest
death is the obvious reading. But `window.tree` outlives the guest — a cleanly
exited guest's interface stays on screen — and a cleared ledger beside a
retained tree is a desync: an identical re-commit takes the no-op path and
never reaches `ledger.apply`, so those ids stay unknown while remaining live,
and the first removal-then-reuse of one is accepted at generation 0. The hole
the ledger exists to close, reopened by the ledger's own reset.

> What dies with the generation is the *history* — retired ids, and the
> observed-id count. What is reseeded is the tree still on screen.

Stated as an invariant to sit beside the retirement one:

> **Retained UI surviving a guest generation change keeps its exact
> `NodeKey`s and repopulates the new generation's ledger before that tree can
> become interactive.**

Not reachable today, because nothing restarts a guest generation. Recorded and
tested anyway, on the same reasoning as the retirement invariant: a rule that
holds only because the code that would break it has not been written yet is
worth an assertion, not a comment. The assertion is
`a_dead_generation_leaves_the_ledger_agreeing_with_the_tree_on_screen`, and it
exists so that reset cannot be quietly "simplified" back to a bare
`ledger.clear()`.

### Open

The `MAX_NODE_IDS` scan is O(observed ids) per commit — negligible for a guest
holding tens of ids, ~65k entries for one that has burned the whole budget.
Not measured, and not worth measuring until a guest exists that churns ids at
all.

### Rest of Stage 2, in order

Layout first, paint second, and each package lands on a green baseline so a
failure can be attributed to the package that caused it.

```text
A1  Row, Stack
A2  sizing: Fill / Content / Fixed, min/max, grow/shrink, alignment
A3  Display, Visibility, Overflow
B1  Scroll viewport semantics
B2  host-owned scroll offset and clamping
B3  clipping and transformed hit-test
C   style: foreground, background, border, corner radius,
    font role/size/weight, padding, gap, cursor
```

Styling is last and separate. Mixing paint vocabulary into the first serious
layout expansion makes failures harder to classify — a wrong rectangle and a
wrong colour on the same commit are two searches, not one.

**No opacity**, whenever C lands — a property by that name will be assumed to
composite a subtree, and per-node paint alpha is not that. C also needs
`StrokeRoundedRect` in `instar-paint` and the Vello backend: `StrokeRect`
currently rejects any width but 1.0 and there is no rounded stroke.

Grid is not exposed as a node kind, even though Taffy supports it. `Stack` is
implemented over a single grid cell internally, which is the ordinary way to
overlap children and stays an implementation detail of `instar-ui::layout` —
the module already owns the rule that no Taffy type reaches its public API.

### Numerics on the wire: bounded integers for lengths, validated floats for ratios

This first said "no floats on the wire, ever". That was the wrong rule — it
answered a question about *trust* with a decision about *representation*, and
would have made flex factors integers to avoid a hazard that validation closes
properly.

```text
dimensional quantities   bounded integers, while the protocol only needs
                         integral logical pixels
intrinsic ratios         validated floats
```

Lengths — `Fixed`, `min`/`max`, padding, gap — stay `u16` under `MAX_LENGTH`.
That buys no NaN, infinity, or negative-zero class at all, cheap decode
validation, straightforward overflow reasoning, a hard ceiling on pathological
layout arithmetic, and a deterministic wire representation. None of that is
worth giving up while every length Instar can express is an integral logical
pixel.

Flex factors are dimensionless ratios where `0.5` and `0.25` are legitimate,
and making them integers would distort the API for very little. So they are
`f32`, and the trust-boundary rule lives in one place at decode:

```text
flex factor must be finite, >= 0, <= MAX_FLEX_FACTOR, with -0.0 canonicalized
```

`MAX_FLEX_FACTOR` is deliberately boring at 1024.0 — far past anything sensible.
The ceiling existing matters; its exact value does not.

> The mistake would not be accepting a float across a trust boundary. It would
> be letting arbitrary IEEE-754 reach Taffy. Decode converts hostile bytes into
> Instar's validated domain, and layout only ever sees the far side of that.

Fractional *lengths* — `12.5px`, percentages, transforms, animation — are a
deliberate later change if a feature ever needs them, not complexity paid for
in advance.

### `Fill` leaves the wire in A2

A1 made `Fill` axis-dependent by adding a second axis: cross-axis stretch under
a column, height under a row, content-sized on a row's main axis. That is one
name for three behaviours, and the rule it implies —

> "Fill means stretch in one axis but content-size in the other, unless grow is
> also set"

— is too clever to teach, test, or keep. The concepts are orthogonal and the
wire says so explicitly:

```text
preferred size         Content | Fixed
main-axis expansion    grow
main-axis contraction  shrink
cross-axis filling     align_self: Stretch
```

Taffy already separates these — `flex_grow` is main-axis expansion,
`align_items`/`align_self` are cross-axis — so this is the wire describing
layout intent rather than inheriting a conflation Instar invented.

Breaking v2/v3 to do it is cheap now and expensive later, which is the whole
argument for doing it in A2 rather than living with the name. `TreeError::
FillHeight` goes with it: it existed because a column of fill-height children
had no defined distribution, and `grow` defines it.

An SDK may still offer `ui.width(Fill)` as sugar and lower it per context. The
sugar is allowed to be clever; the wire is not.

### A3: Display, Visibility, Overflow, frozen before the code

Three properties that all mean some version of "less than fully present", and
whose whole value is being *different* from each other.

```text
Display::Normal     participates in layout

Display::None       retained in the Instar tree
                    absent from layout
                    no paint
                    no hit-test
                    no accessibility
                    descendants likewise absent
```

```text
Visibility::Visible normal presentation

Visibility::Hidden  still participates in layout
                    no paint
                    no hit-test
                    no accessibility
                    suppresses the whole subtree
```

The single line separating them: `Display::None` leaves layout, `Hidden` stays
in it and reserves its space. Everything else the two do is the same, which is
exactly why they need to be two names rather than one property with a flag.

**`Hidden` is subtree-wide, and CSS's version is not.** In CSS a descendant of
a `visibility: hidden` node can set `visibility: visible` and reappear inside
an invisible ancestor. That is a genuinely strange rule to have to hold, it
makes "is this node visible?" a walk to the root instead of a lookup, and no
interface Instar is trying to support needs it. Suppression here is absolute.

```rust
enum Overflow { Visible, Clip }
```

`Clip` means, and means only:

```text
layout            unaffected
descendant paint  intersected with this node's rectangle
descendant hits   intersected with the same rectangle
nested clips      intersect
scrolling         no
scroll offset     neither created nor modified
```

**There is deliberately no `Overflow::Scroll`.** CSS makes scrolling a value of
the overflow property, and copying that would make CSS's overflow model
Instar's architecture by accident. The two are separate things here:
`Overflow::Clip` is a rectangle intersection and holds no state, while `Scroll`
(B1–B3) is a *node kind* — a retained viewport with a host-owned offset,
transient state, and a retirement obligation. A property value cannot carry
that, and pretending it can is how the distinction gets lost.

### Becoming invisible retires interaction, exactly as deletion does

> When a subtree becomes non-interactive through `Display::None` or
> `Visibility::Hidden`, any host transient state referencing that subtree is
> retired before the new state becomes interactive.

The same class of invariant as `Interaction::retire` on deletion, and it has to
land in A3 rather than Stage 3, because otherwise it is a bug waiting for focus
to exist. Press is what there is to retire today; focus, hover, pointer
capture, and scroll offsets join it as they arrive.

Deletion and hiding are not the same event, and the ledger says so — a hidden
node is still live at its generation, and it is still in the tree. What they
share is the only thing that matters here: a press whose target can no longer
be hit must not be completable. Without this, pressing a button and hiding it
mid-press leaves a press outstanding against something the user can neither
see nor reach.

### Scroll: the invariants, frozen before the node exists

Stage 3's scroll semantics were written down before there was a `Scroll` node
to argue about. They are the contract for B1–B3 and are restated here as
obligations rather than description:

```text
offset is host-owned            a guest cannot set, read, or veto it
wheel response needs no guest round-trip
content shrinks                 offset clamps to the new extent
Display::None                   offset is retained, interaction is not
deleted Scroll                  offset is destroyed with the node
hit test                        clip to the viewport, then translate into
                                content space -- in that order
```

Two of those are the ones a plausible implementation gets wrong.

**`Display::None` retains, deletion destroys.** They look alike and are
opposites. A node the guest hid is still a node the guest has; scrolling back
to where you were when it reappears is the behaviour a user expects, so the
offset survives. A node the guest removed is gone, and its offset joins focus,
hover, and pointer capture in `Interaction::retire` — the single site that
answers "the node is gone, forget everything about it". A generational
`NodeKey` is what makes that unambiguous: an id that comes back comes back at a
new generation and therefore starts at offset zero, with no rule needed to say
so.

**Clip before transform.** Hit-testing a scrolled subtree that translates first
and clips second will report hits on content scrolled out of view — the pointer
is inside the child's translated rect but outside the viewport that owns it.
The order is not an optimization.

The stage acceptance is unchanged: a test proving **zero `SendToGuest`** for
hover, focus movement, and wheel events, while each still produces a `Render`.

### B1 is the retained viewport, and nothing else

No wheel events, no scrollbar chrome. Those are B2 and Stage 3. B1 exists so
that when input arrives there is already a correct, tested thing for it to move.

```text
Scroll owns          host-local offset
guest owns           content, not the current scroll position

viewport rect        the Scroll's laid-out rect
content extent       the laid-out bounds of its content

paint                apply ancestor clip
                     intersect with the viewport
                     translate descendants by -offset
                     paint descendants

hit test             apply ancestor clip
                     reject outside the viewport
                     translate the point by +offset
                     descend in content coordinates

content shrinks      clamp the offset before the next presentation
                     becomes interactive
Display::None        no interaction; the offset stays retained
Visibility::Hidden   the same
deletion             destroys the retained scroll state
a commit that
leaves Scroll alive  preserves the host-owned offset
```

**Exactly one content child.** An app that wants several things puts a
container there:

```text
Scroll
└── Stack
    ├── …
    └── …
```

Two reasons, and the second is the real one. It gives one unambiguous content
extent — with several children the extent is a union, and a union of
overlapping boxes is a question with more than one defensible answer. And it
stops `Scroll` quietly becoming a layout container as well as a viewport: a
node that both distributes children *and* owns transient offset state is two
things wearing one name, which is the mistake `Fill` already taught this
protocol once.

**Scroll does not invent a second clipping path.** A3 established the order —
ancestor clip, then this node's clip, then descend — and B1 extends that same
path with a translation between the clip and the descent. Nested
`Overflow::Clip → Scroll → child` must work by composition, not by a parallel
mechanism, and there is a test for exactly that shape.

The transform is where the interesting failure lives, so it is tested
concretely rather than by property: a button at content `y = 200` with the
offset at `150` paints at viewport `y = 50`, is activated by a click at
viewport `y = 50`, is *not* activated by a click at its unscrolled position,
and nothing outside the viewport paints or hits.

### B2: the wheel, and the proof the guest is not in the loop

```text
instar-window   emits RawScrollEvent
                knows WindowId, pointer position, wheel delta
                knows no NodeKey and no ScrollState

pixel deltas    physical -> logical at the window boundary
line deltas     stay explicitly "lines"; UI policy turns them into a
                logical step

instar-ui       finds the deepest eligible Scroll under the pointer
                applies the delta to the host-owned offset
                clamps to the content extent

nested Scroll   the deepest viewport consumes what it can
                the unconsumed remainder bubbles to ancestor Scrolls

offset changed      -> Render
nothing consumed    -> no effect at all
SendToGuest         -> never, for ordinary scrolling
```

**The residual bubbles.** "The nearest scroll owns the whole event" is simpler
and is the classic nested-scroll trap: an inner viewport already at its limit
swallows input that should have kept scrolling the outer one, and the page
feels stuck for no reason the user can see. Consuming what is available and
passing the remainder up costs one subtraction and removes the whole class.

**One sign convention, fixed at the window boundary.** By the time a delta
reaches `instar-ui`, `+y` means *increase the scroll offset*, which reveals
content further down. Platform wheel direction, natural-scrolling settings, and
winit's own conventions are resolved in `instar-window` and never travel
inward. Retained UI that has to ask which way `+y` points on this OS is
retained UI with a platform leak in it.

**Acceptance is stronger than "the offset changed".** A button starts below the
viewport; wheel input arrives; its host-owned offset changes, and its painted
*and* hit-tested position both move to match — the same button, reachable where
it now appears. Across the whole operation, zero `SendToGuest`. Then the
converse: a viewport already at its limit produces neither a guest event nor a
pointless `Render`, because a redraw that changes no pixel is still a frame
somebody paid for.

Scrollbar chrome stays later. Wheel and touchpad scrolling is the
architectural claim worth proving — continuous interaction resolved entirely
host-locally, with Wasm absent from the response loop.

### C: style, sorted by what it can invalidate

The vocabulary is grouped by consequence rather than by what it looks like,
because the grouping *is* the design:

```text
shaping / layout-affecting   font role and family
                             font size
                             font weight

paint-only                   foreground
                             background
                             border
                             corner radius

interaction-only             cursor
```

> **A paint-only change must not rebuild or re-line-break any text.**

That gets an explicit regression test, because Stage 1 is the reason the line
exists. `ShapingStyle` is hashed as a cache key, and the crate already carries
a warning that adding colour to it would silently destroy every reuse — a
colour change would re-shape the tree and nothing would fail, it would just get
slow. A `TextStats` assertion is the only thing that catches that class.

### Borders are defined here, not inherited from the renderer

```text
border                 does not affect layout
                       painted inside the node's laid-out rect
border width           finite and bounded
corner radius          clamped to valid geometry
```

Painted *inside* rather than centred on the edge, which is what a stroke
ordinarily means. A centred stroke puts half its width outside the rect the
layout computed, so a bordered node overlaps its neighbours by a hair, clipping
cuts the outer half off, and the bounds hit-testing uses stop matching the
bounds the user can see. Inside-stroking keeps a node's painted extent exactly
the rect layout gave it.

That makes `StrokeRoundedRect` an internal primitive with stated geometry
rather than "whatever a centred stroke happens to do underneath", and it is
what makes clipping composable with it. `StrokeRect` today rejects any width
but 1.0, so both need widening.

Radii are clamped so that opposite radii cannot exceed the side they share —
otherwise the corners overlap and the shape is not well defined. Clamped at
the boundary, like every other untrusted number, so no backend has to.

**Still no `opacity`.** Node-local alpha is already expressible in every colour
the vocabulary has. What a property called `opacity` is assumed to mean is
*subtree* opacity, and that is a compositing feature — an offscreen layer, a
blend, and a whole category of interaction with clipping and text rendering. A
deceptively simple name with layer-level consequences is worse than no property
at all.

### Then scrollbar chrome, as the last Scroll package

```text
thumb and track state    host-owned
hover and drag           host-local
dragging                 zero guest events
thumb size               derived from viewport and content extent
content or viewport
changes                  recompute and clamp
nested scrollbars        the same clip and translation path B1 and B2 proved
```

Chrome comes after C so it can use the real paint vocabulary. Building it first
would mean inventing scrollbar-only drawing rules that C then replaces, and
temporary rules have a way of outliving their reason.

At that point `Scroll` is a complete proof: layout, clipping, wheel response,
hit-testing, painting, and direct manipulation, none of which puts Wasm in the
continuous-interaction loop.

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

Note what it *cannot* reach, and why Stage 2's generational keys were required:
anything already encoded and queued for the guest. That is now closed from the
other end — the queued bytes carry the generation, so the guest rejects an
activation naming a node it has since replaced. The host still cannot recall
the event; it no longer has to.


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

**A profile split must be `cfg!`, not `#[cfg]`.** The release half of that
table was not being asserted at all. `assert_prompt`'s release body called
*itself* — six live call sites, no assertion, and a stack overflow waiting for
the first release run. A `#[cfg]`-gated pair of bodies is only type-checked in
the build it belongs to, and CI builds debug, so nothing could see it. The
release assertions now live in one body behind `if cfg!(debug_assertions)`,
which costs nothing at runtime and buys compilation in both profiles.

> A gate that hides a body from the compiler hides its bugs too. Split on
> `cfg!` and let both halves compile; reserve `#[cfg]` for code that genuinely
> cannot exist in the other profile.

**Nothing runs `--release`.** CI's whole matrix is debug — `cargo test
--workspace`, and the bridge suite again with `--nocapture`. So the right-hand
column of that table has never executed, which is the other half of why the
recursion survived: not compiled, and not run.

Turning it on for the first time failed, at p50 6.66 ms against a 5 ms target —
and the first three readings looked like a Stage 2 regression. They were not.
They were taken while two other builds had the machine at a load average of 23.

The measurement that settles it interleaves the two commits on an idle machine,
alternating so that drift hits both equally:

```text
             p50                    p95                    p99
parent    4.983 / 4.954 / 4.953  5.34 / 5.18 / 5.30  5.67 / 5.31 / 5.55
stage 2   4.959 / 4.857 / 4.981  5.27 / 5.15 /  --   5.48 / 5.32 /  --
```

Indistinguishable, and the Stage 1 ledger's 4.94 ms reproduces exactly. The
generational key costs nothing measurable: the ledger is one hash lookup per
node on a twelve-node fixture, against a round trip dominated by Wasmtime,
layout, and raster.

> The first number said "regression". The controlled number said "your laptop
> was busy". Same rule as the 386 ms that turned out to be 5 ms — instrument
> the thing, in the profile that matters, on a machine doing nothing else.

What the exercise did find is that `p50 ≤ 5 ms` had no headroom. It was the
recorded 4.94 ms rounded up to the next integer, while p95, p99 and max all
sit near 1.4x their measurements. Six idle runs put p50 between 4.86 and
4.98 ms — 0.4% to 2.9% under the bound, and 6.66 ms the moment anything else
runs. It is now 7 ms, which is the 1.4x its neighbours already used. That is a
calibration, not a concession: the interleaved runs show the margin was always
this thin and that Stage 2 did not spend it.

### So the bounds are asserted on request, not by default

```text
INSTAR_LATENCY_GATE=1 cargo test --release -p instar-host --test bridge -- --nocapture
```

The distribution prints on every run in every profile. What is opt-in is the
*judgement*, because a judgement needs a host that is doing nothing else, and
the machine this was written on reports p95 5.2 ms idle and 17.9 ms with a
build running. A suite that goes red because someone opened a browser teaches
people to ignore red suites, which costs more than the gate is worth.

This is deliberately not the `#[cfg(any())]` it replaces. That assertion was
switched off in a way nothing could see: it did not compile, it left `PROMPT`
dead, and it let the release body rot into infinite recursion. This one
compiles in every profile, runs on request, and *says on every run* that it is
only reporting:

```text
click-to-committed-tree: n=1000 p50=5.15ms p95=9.87ms p99=19.9ms max=158ms
  (reported only; set INSTAR_LATENCY_GATE=1 on an idle host to assert)
```

> An assertion you have chosen not to run must announce itself. The difference
> between a deferred judgement and a forgotten one is whether anybody is told.

Still open, and not answerable from a chair: whether CI should arm the gate,
given that a shared runner is not a quiet host either. Today CI runs only
debug, which is the other half of why the dead assertion survived.

### Ordering is not throughput

`a_hundred_rapid_activations_arrive_in_order` demanded the exact sequence
`1..=100`, sampled one label per 50 ms `wait`. `wait` pumps the whole queue, so
two commits landing in one window are observed as one — a correct run reports a
gap and looks like a dropped click — and 100 debug round-trips do not reliably
fit the deadline, so it also failed for being slow, which is the property this
file deliberately does not assert in debug.

Sampling cannot hide the defect being hunted: a reordering makes an observed
count *decrease*, and dropping observations does not reorder them. So the
assertion is monotonicity plus completion, and the deadline is generous,
because it is a completion test.

## The green baseline rule

> Each Stage 2 architectural package starts from a green
> `cargo test --workspace` and a green
> `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets`.

Not tidiness. When the layout vocabulary lands and thirty tests go red, the
only way to tell new breakage from old debt is that there was no old debt.
Three of the failures cleared to establish this had been red since Stage 1 and
were being read as background noise — one of them was hiding the release
latency gate never running at all.

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
