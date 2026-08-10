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

> **Status.** A1–A3 and B1–B2 are implemented and green: generational identity,
> `Row`/`Stack`, the orthogonal sizing vocabulary, `Display`/`Visibility`/
> `Overflow`, the retained `Scroll` viewport, and host-local wheel scrolling.
> Packages C (style) and the scrollbar chrome are **frozen contracts only** —
> see the marked sections below.
>
> This file is the record of *how the decisions were reached*, including the
> ones that turned out wrong. For how Instar works now, read
> `docs/ARCHITECTURE.md`; for what it costs, `docs/baselines/PERFORMANCE.md`.

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

### C: style, sorted by what it can invalidate — **planned, not implemented**

> Nothing in this section or the next exists in the code. They are frozen
> contracts for the next two packages, written before implementation on
> purpose. Everything above this line is built and green.

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

### Then scrollbar chrome, as the last Scroll package — **planned**

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

## What is left, and the order

Package C ends the vocabulary work. There is now enough visual language to
find out whether the architecture works as a desktop toolkit, and that
question is worth more than another control.

```text
D  scrollbar chrome          completes a subsystem rather than opening one
E  focus and keyboard        more important to a real app than more widgets
F  AccessKit
G  UI Gallery                stresses every primitive
H  Calculator + thin SDK     stresses the application API
I  performance and overhead audit
```

> **The freeze criterion: no new ordinary UI feature enters Phase 2 unless the
> UI Gallery or the Calculator demonstrates that it is required.**

Written down because the failure mode is specific and attractive. A toolkit
with twelve controls and no application is a project that has stopped asking
its own question, and every control added on speculation is one the host must
lay out, hit-test, paint, and expose to accessibility forever.

### D — scrollbar chrome

All of it host-owned, which is what makes it the completion of B1 and B2
rather than a new surface:

```text
ScrollState
├── offset
├── hovered_part
├── dragging_thumb
├── drag_origin
└── drag_origin_offset
```

A guest contributes styling and policy, if anything. Wheel, thumb hover, track
click, thumb drag, clamping, and repaint stay host-local.

```text
wheel               -> 0 SendToGuest
thumb drag          -> 0 SendToGuest
hover               -> 0 SendToGuest
track interaction   -> 0 SendToGuest

content shrinks     -> thumb and offset clamp coherently
nested scroll       -> the viewport and clip behaviour B1 established
Display::None       -> state retained, interaction disabled
deletion            -> ScrollState destroyed
```

**The acceptance test stalls the guest for 100 ms mid-drag** and requires the
scrollbar to stay perfectly responsive. Every other assertion here can be
satisfied by an implementation that merely happens not to call the guest; this
one fails unless the guest is genuinely absent from the loop. It is the
clearest possible statement of the claim, so it is the one to make — and it is
asserted on what a user would see, not on `ScrollState`: the offset moves, the
thumb moves, the content paints in its new place, and hit-testing follows it,
all while the guest is blocked.

#### Chrome is presentation, not semantics

```text
guest tree            host presentation

Scroll                Scroll viewport
└── content           ├── content
                      ├── track
                      └── thumb
```

The track and thumb are generated from the `Scroll` node, exactly as a focus
ring will later be generated from focus state. Putting them in the semantic
tree would give internal chrome a `NodeKey`, and `NodeKey` identity belongs to
application semantics — a guest would find nodes it never created, the ledger
would account for them, and accessibility would have to explain them.

#### Drag does not bubble, and wheel does

```text
wheel        the deepest viewport takes what it can; the remainder
             bubbles outward
thumb drag   direct manipulation of one specific container; reaching
             the end does nothing to its ancestors
```

Not an inconsistency. A wheel delta is a quantity with a meaningful leftover,
so passing the leftover on is what stops an inner viewport swallowing the
gesture. A thumb is a physical handle on one scroll container; a handle that
started scrolling its parent when it hit the bottom would be a handle that lies
about what it controls.

#### Pointer capture, and cancelling it

Once a thumb is grabbed, the drag continues while the pointer moves off the
thumb, off the scrollbar, and outside the window, until release. Without
capture, a fast drag intermittently drops control — the pointer outruns the
thumb between events and lands on the track, which would otherwise be a
page-step.

Cancellation follows the doctrine already in place for geometry:

> If the geometry a drag began against becomes invalid, the drag is cancelled
> before the replacement geometry becomes interactive.

A resize, a scale change, deleting the `Scroll`, or hiding it all destroy the
frame of reference the drag's arithmetic depends on. Completing a drag against
geometry that no longer exists is the same defect as completing a press against
a node that no longer exists, and it gets the same answer.

### E — focus and keyboard

Three packages, each from a green checkpoint: **E1** focus lifecycle and
traversal, **E2** keyboard activation, **E3** focus presentation and reveal.

> Focus is host-owned transient interaction state keyed by generational
> `NodeKey`. A guest may request focus semantically; ordinary focus movement
> and keyboard interaction need no Wasm round trip.

#### E1 — the lifecycle

```text
FocusState
├── focused: Option<NodeKey>
└── focus_visible: bool

Tab        -> next focusable in retained tree order
Shift+Tab  -> previous

focusable  = interactive and enabled and presented,
             with every ancestor presented

Display::None / Visibility::Hidden / disabled  -> cannot receive focus
the focused node becomes any of those, or is removed
           -> focus is retired before the new tree becomes interactive
```

**Retirement clears focus outright.** The next `Tab` restarts from the top
rather than resuming where the retired node was. Remembering the position is
better for a form that disables a field mid-edit, and it costs a second piece
of transient state keyed by `NodeKey` — which needs its own retirement rules,
and is exactly the kind of thing that later turns out to reference a node that
no longer exists. Clearing is the rule that cannot be subtly wrong.

The generational regression is mandatory, and is where `NodeKey`'s generation
earns its place a second time:

```text
focus Button (7, 0)  ->  remove it  ->  create Button (7, 1)
                     ->  the new button is NOT focused
```

Focus is precisely the kind of long-lived reference that outlives the node it
names. Without the generation this is a rule someone has to remember; with it,
the stale key simply does not match.

`focus_visible` is deterministic host policy, not a `:focus-visible`
heuristic:

```text
keyboard traversal, accessibility  ->  focus_visible = true
pointer click                      ->  focus may move, focus_visible = false
```

That keeps a keyboard-style ring off the screen after every mouse click without
the guest tracking input modality.

#### E2 — keyboard activation

> Keyboard interaction has the same semantic outcome as pointer interaction,
> while transient pressed presentation stays entirely host-local.

```text
Enter down    activate the focused button; ignore autorepeat
Space down    capture the focused button as keyboard-pressed
              show pressed chrome immediately; no guest event yet
Space up      activate only if the *captured* key is still focused,
              enabled and interactive; always clear the capture
Space repeat  ignored
```

**Space captures, exactly as a pointer press does.** Release must not activate
"whatever is focused now":

```text
Space down on (7,0)  ->  focus moves to (8,0)  ->  Space up  ->  nothing
```

The capture is cancelled by everything that makes the captured key ineligible —
removal, hiding, disabling, focus moving away, the window losing OS focus, a
commit that replaces the generation. Which is the general form of a rule this
stage keeps rediscovering:

> Any transient interaction naming a `NodeKey` is retired when that key stops
> being eligible.

**A press needs to know its source.** One `pressed: Option<NodeKey>` would let
a pointer release complete a Space press, and a Space release complete a
pointer press — two input paths quietly sharing one capture slot. The press
records where it came from and only the matching release completes it.

**No new guest events.** Ordinary buttons produce the same semantic outcome
whatever activated them:

```text
pointer ──┐
Enter ────┼──>  host activation policy  ──>  ButtonActivated { key }
Space ────┘
```

That convergence is what makes F small: AccessKit joins the same arrow rather
than forking a second activation path.

The hostile test splits the two properties that matter, because they have
different answers:

```text
guest blocked 100 ms
  Space down  ->  pressed chrome changes before the guest could run
  Space up    ->  chrome clears immediately, activation is queued
after the stall
              ->  exactly one ButtonActivated, carrying the captured
                  generational key
```

Application consequence may wait for Wasm. Interaction feedback may not.

#### E3 — focus presentation, and semantic reveal

The ring is host-generated chrome with no `NodeKey`, exactly as the scrollbar
is:

```text
guest tree      host presentation

Button          Button
                └── focus ring
```

```text
keyboard focus        focused = key, focus_visible = true
pointer focus         focused = key, focus_visible = false
focus retired         focus_visible = false
OS window blur        the focused key is *retained*; this is not retirement
```

Losing OS focus is not a node becoming ineligible. The window will come back,
and the user's place with it — treating it as retirement would clear focus
every time someone alt-tabbed to check something.

**The ring obeys the same clip stack as the node it surrounds.** Host-generated
chrome escaping a `Scroll` or an `Overflow::Clip` would be the "two answers to
where the node is" problem again, this time between a node's presentation and
its focus presentation — a ring floating over content the node itself is
clipped out of.

##### One reveal primitive

```rust
enum RevealAlignment { Nearest, Start, Center, End }

reveal_node(node, alignment)
ensure_visible(node) == reveal_node(node, Nearest)
```

`Nearest` is the default for Tab because it moves the least. A partially
visible node moves just enough to expose it, never centred gratuitously.

##### Nested viewports: recompute between every step

```text
target's laid-out rect
  ↓
find the Scroll ancestors containing it
  ↓
innermost -> outermost, and for each:
    transform the target into that viewport's content space
    adjust the offset minimally for the alignment
    clamp
    recompute where the target now presents
  ↓
continue outward
```

The recompute is the part that is easy to omit and wrong to omit. Moving an
inner viewport changes where the target sits relative to the outer one, so
computing every offset from the original geometry gives the outer viewport a
stale answer. Nested reveal fails in exactly that case and nowhere else, which
is why it gets its own test.

##### Authority, and refusals

```text
guest    reveal_node(button_7, Center)
host     chooses the actual offsets, and never reports them back
```

```text
unknown or stale NodeKey     no-op
Display::None                not revealable
Visibility::Hidden           not revealable
no layout geometry           not revealable
already visible + Nearest    no offset change, no redraw
reveal moves something       host-local offsets, a redraw, zero SendToGuest
```

`reveal_range` and `select_range` are deliberately absent. They belong to
Phase 3's `TextView`, which needs a range vocabulary this package has no reason
to invent. What E3 freezes is the *principle* — navigation is semantic intent,
never an offset a guest computed from geometry it does not own.

#### The structural invariant, modelled on C5

> Moving focus without changing the guest tree must not enter layout or
> shaping.

A focus ring is paint. If focus movement ever starts running Taffy or Parley,
that is an architectural regression to catch now rather than when it becomes
slow — and, per the rule this file already records, the test observes *entry*
into that work rather than its cost.

#### What E is not

Character input, caret navigation, editing shortcuts and IME are Phase 3's
`TextView`. `Button` activation and focus traversal are ordinary retained-UI
behaviour; letting E acquire the rest would make it half an editor with none of
the contract that makes an editor correct.

The same ownership split that already governs pressed-state and scrolling:
**transient interaction belongs to the host; the guest receives semantic
outcomes.**

```text
host owns    the focused NodeKey, tab traversal, focus-visible state,
             keyboard activation, focus-ring presentation
guest gets   ButtonActivated, and FocusChanged only if it subscribes
```

```text
Tab / Shift+Tab traverse
Enter activates the focused button
Space follows button semantics
a hidden, removed or disabled node loses focus safely
a reused id cannot inherit stale focus
focus movement needs no guest round trip
```

The reused-id case is where generational `NodeKey` earns its place a second
time: focus is exactly the kind of long-lived reference that outlives the node
it names, and the generation makes "is this still the same node?" answerable
rather than a rule someone has to remember.

**Navigation is semantic, never an offset.**

```text
reveal(node, alignment)
ensure_visible(node)
```

Tab traversal immediately needs "bring the focused thing into view", so the
abstraction gets established here rather than being retrofitted when the editor
needs `reveal_range`. A guest that could set a scroll offset directly would be
a guest that can scroll a view out from under someone reading it — the same
reason the offset is host-owned at all.

### F — AccessKit, and not later

Verified against the crates that actually resolve here, not against
recollection: `accesskit` 0.24.1, `accesskit_winit` 0.33.2, and — the detail
that decides whether F is cheap — **`accesskit_winit` 0.33.2 resolves against
winit 0.30.13, which is already in the tree.** No winit upgrade hiding inside
this package. `NodeIdContent` is `u64`, so the existing packing fits exactly;
`Role::{Window, GenericContainer, Label, Button, ScrollView}` and
`Action::{Click, Focus, Blur, ScrollIntoView}` all exist today.

```text
retained tree + LayoutSnapshot + FocusState + ScrollState
        ->  AccessKit projection  ->  platform APIs

ActionRequest  ->  NodeId back to a generational NodeKey
               ->  the interaction policy that already exists
```

#### The projection is a read, not a second model

```text
Root              -> Role::Window
Row/Stack/Column  -> Role::GenericContainer
Text              -> Role::Label
Button            -> Role::Button
Scroll            -> Role::ScrollView
```

`GenericContainer` is documented as presentational and normally filtered out by
assistive technology, which is exactly right for layout-only containers —
better than inventing a semantic role for a node whose whole meaning is "these
things are stacked".

Only what Instar already knows: children, bounds, label, disabled, focus,
scroll position and range, clipping. **No accessibility concept enters the
guest wire.** A guest that has never heard of AccessKit is already fully
described, because everything AccessKit wants is host state.

Focus has exactly one source of truth. `TreeUpdate` wants the focused `NodeId`
on every update, and the root when nothing is focused — which is `FocusState`:

```text
FocusState = Some(key)  ->  TreeUpdate.focus = accesskit_id(key)
FocusState = None       ->  TreeUpdate.focus = root
```

#### Actions converge, or F has failed

```text
mouse click ────┐
Enter / Space ──┼──>  activate(key)  ->  ButtonActivated
AccessKit Click ┘
```

```text
Action::Click          -> the existing activation, generational checks and all
Action::Focus          -> the existing FocusState, reveal and ring
Action::Blur           -> the existing focus retirement
Action::ScrollIntoView -> the existing reveal_node
```

AccessKit defines `ScrollIntoView` as scrolling whatever containers are needed
to expose a node, which is E3's primitive under another name. That it already
exists is evidence the convergence is real rather than arranged.

**This gets an aggressive fault injection.** Bypass `activate()` and
manufacture a `ButtonActivated` directly: the application-visible result is
identical, so only a test watching the *path* can fail. If it passes, F has
quietly forked a second interaction system and the architectural claim is gone.

The route counters are **instrumentation, not behaviour**. The seam exists
because it centralizes the operation; the counter only observes entry into it.
The shape to preserve is

```text
adapter -> canonical operation -> eligibility, transition, effect, observer
```

and not

```text
adapter -> increment the counter -> some other implementation
```

Otherwise a later change could keep the counter satisfied while bypassing part
of the policy, and the test would go on passing.

> **Advertise an action only if Instar can honour it correctly.** AccessKit's
> vocabulary is far larger than Instar's — `SetValue`, `SetTextSelection`,
> direct scroll-offset actions. Advertising one because the enum has it is
> promising behaviour that does not exist. Those become real when Phase 3's
> `TextView` arrives.

For `Scroll`, semantic actions only. Exposing raw offset actions would widen
Instar's public conceptual model to match a platform schema, which is the
reverse of the direction everything else here has taken.

#### Four defects the first non-counter guest found

Dogfooding found in one session what the automated suite had not, which is the
argument for G in miniature. All three were in shipped, tested packages.

**Labels wrapped according to their own fractional widths.** Taffy rounds
computed layout to integers, so a node sized from its own text landed a
fraction of a pixel narrower than the text it was measured from — and the
finalize pass then re-broke the label to that rounded width. "Ordinary button"
needed 89.36pt and got 89.00; "Nothing pressed yet" needed 114.52 and got 115,
so it was fine. The same string wrapped or did not according to the font, the
size, and the letters in it. Fixed by reporting measured dimensions as
ceilings: a box sized from a measurement is never smaller than what was
measured. A genuinely constrained node still wraps, because that width comes
from its parent.

**No wheel event ever reached the scroll subsystem.** B2 built residual
bubbling, D built the scrollbar chrome, `WindowState::on_wheel` settled the
sign convention, and `Host::on_scroll` routed it — and `winit_adapter::translate`
had no `MouseWheel` arm, so none of it was reachable from the application. The
scrollbar rendered and did nothing. Every layer was tested in isolation and the
seam between two of them did not exist. Now wired, with the sign pinned by a
test: winit's positive Y reveals content *above*, which is a negative Instar
offset delta.

**A viewport could not be bounded by its parent.** A `Scroll` is a flex item,
and its automatic minimum size decides whether `grow` and `shrink` can reach
it. Taffy's default overflow is visible, so that minimum was the content's own
size — a viewport with tall content sized to the content and broke the layout
around it. The only way to bound one was a fixed height, which does not follow
a resized window. Declaring `Overflow::Scroll` is what CSS does for the same
reason: a scroll container's automatic minimum size is zero, because clipping
is the whole point of it.

**The scrollbar thumb took a press and never moved.** `winit_adapter::translate`
returned `None` for `CursorMoved`, with a comment explaining that a move is not
a pointer event in Instar's model and that "hover and drag arrive with the
interaction state that needs them" — which never happened. `Host::on_pointer_moved`
was implemented and had six tests, every one of which called it directly.
`WindowOutput` now has a `PointerMoved` term, and the drag has a test driven
only through `handle`, so losing the term again fails.

The pattern worth naming: three of these four are **seams, not units**. Every
part on either side was correct and tested, and each seam was a single missing
arm in one `match`. Package-level verification cannot see them, because at
package level nothing is missing.

Two of the three were the same `match`. Worth checking the rest of that
function's arms against what each layer downstream already implements.

#### Status

```text
F1  semantic projection       DONE
F2  incremental updates       DONE
F3  action convergence        DONE
F0  shell adapter plumbing    DONE / structurally verified
F4  native AT behavior        PENDING -- see docs/F4-SMOKE.md

Accessibility semantics       COMPLETE
Platform accessibility        NOT YET VERIFIED
```

Both halves of that distinction matter. F1–F3 are a subsystem in their own
right — AccessKit itself separates the toolkit-side tree and actions from the
platform adapters — so they merge on their own evidence. And compiling the
cross-platform code is not evidence that macOS, UIA and AT-SPI behave, because
those are three separate native adapters underneath.

So: neither "F is done, the hard part is proven" nor "F1–F3 cannot land until
VoiceOver has been run".

#### Order, and what cannot be verified here

```text
F0  adapter lifecycle: invisible window -> adapter -> visible window,
    with action events reaching the main thread
F1  projection: retained state -> TreeUpdate, stable generational ids
F2  incremental updates, entering neither Taffy nor Parley needlessly
F3  action convergence, fault-injected
F4  platform smoke on Windows, macOS and Linux
```

F0 is its own package because `accesskit_winit::Adapter` must be constructed
**before the window is first shown** — create the window invisible, build the
adapter, then show it. That changes `instar-shell`'s startup order, which is
the kind of thing that works on one machine and fails on another platform.

The plan named `with_mixed_handlers`, and implementation rejected it.
Its attraction is a synchronous initial tree, avoiding a placeholder on some
platform adapters. Its price is that the activation handler must be `Send` and
"may be called on any thread, depending on the underlying platform adapter" —
and answering it means reading the retained tree. That is exactly the rule this
package exists to keep, so the cheaper-looking constructor is the one that
breaks it.

`with_event_loop_proxy` forwards *every* request — activation, actions,
deactivation — through the proxy to the main thread. **Nothing mutates
`FocusState`, `ScrollState` or the tree from an AccessKit callback thread**,
because no AccessKit callback does anything but post an event. The cost is one
frame of placeholder tree on activation, paid once per attach.

F4 is not verifiable on one developer machine, and neither is F0's real
behaviour — both need a display server and a live assistive technology. Same
honesty as the compositor: the automated suite proves the projection and the
action routing, and platform behaviour is a manual smoke test.

#### F0, as built

**Everything F0 claims is verified; what is unverified is specifically the
native AT ↔ AccessKit boundary.** Build and lint coverage, proxy event
conversion, the update seam, lifecycle ordering on a real macOS window, and the
failure behaviour around the visibility requirement — all of these are checked.
Nothing below was observed working with an assistive technology, and that is
F4, not F0.

One adapter, one window — as one field, not two `Option`s. `NativeWindow`
holds the `Arc<Window>` and its adapter together, constructed in a single step
with nothing fallible between the adapter and `set_visible(true)`. Two options
could drift; there is no state in which a visible window lacks its adapter, and
dropping the pair drops both. `Adapter::process_event` is the first statement
in `window_event`, ahead of every early return, and `window_event` is the sole
entry point for winit window events — so the "before the application handles
it, for every event" requirement holds structurally rather than by discipline. `ShellEvent` is the loop's user-event type, with
two variants — the runtime thread's payload-free wake, and
`accesskit_winit::Event` — because a single queue is what puts platform
requests on the main thread.

The seam is `instar_shell::accessibility`. `Adapter::update_if_active` sits
behind a one-method `UpdateSink`, and it is the *only* thing there that a test
cannot reach; every decision about whether and how often to call it is on this
side of the trait. Request conversion is a pure function into a three-case
`Request`, so transport is checked without a window, an adapter or a guest.

Two rules emerged from the AccessKit API that the plan did not anticipate.

`Host::accessibility_update` **drains** — what it returns is not offered twice.
So the shell must not ask while nothing is attached, or the change is discarded
and never reaches the assistive technology that attaches next. `Accessibility`
tracks attachment and does not call the producer at all when detached. This is
a correctness rule, not an optimization, and it has its own test.

An adapter that has just attached **holds nothing to diff against**, so
activation takes a separate path: `reset_accessibility` then a full projection,
via `HostBridge::full_accessibility_tree`. An incremental update on that path
would describe changes to a tree the platform does not have.

What was actually observed, beyond compiling: the shell was launched on macOS
with the counter guest and ran — window created invisible, adapter constructed,
window shown, guest committed, frames presented, no panic. That establishes the
lifecycle ordering `accesskit_winit` requires, since `with_direct_handlers`
panics outright if the window is already visible. It establishes nothing about
VoiceOver, which is F4.

Eleven faults injected across the two crates, all caught: the metrics barrier
dropped, the projection always reset, `ScrollIntoView` misrouted to `Focus`,
unsupported actions falling through to `Activate`, the generation half of a
`NodeId` discarded in transport, the host asked while detached, deactivation
failing to detach, an empty update sent anyway, deactivation mistaken for
activation, transport filtering actions the host should decide about, and
activation sending a diff instead of a full tree.

One methodological note, recorded because it cost the work twice over: using
`git checkout --` to undo a fault injection destroys uncommitted work, and the
baseline here was uncommitted. Fault injection must restore from a file copy,
or run against a committed tree.

Deferring accessibility until an application needs it is how it becomes
scaffolding. By E there is retained semantics, stable generational identity,
layout, visibility, clipping, scroll, focus, keyboard activation, and style —
enough substrate for the mapping to be meaningful.

```text
Window       -> Window
Text         -> Label
Button       -> Button
containers   -> GenericContainer
Scroll       -> ScrollView
```

Boring on purpose. `NodeKey::to_accesskit_id` already packs identity, and
remove-then-reuse already becomes a new accessibility object rather than
recycling one a screen reader may still hold.

> **The architectural test: an accessibility action routes through the same
> interaction machinery as a mouse or keyboard action.** No AccessKit-only
> activation path. Two paths to the same outcome is two places for the rules to
> diverge, and the one used least is the one that rots.

### G and H — dogfooding, which is the actual experiment

The Gallery proves every primitive works: nested rows and stacks, long scroll
areas, hidden and display-none controls, clipping, borders and radii, font
variants, disabled controls, keyboard traversal, scrollbars, accessibility,
resize, DPI changes.

The Calculator proves an application is *pleasant to write*, which is a
different question and the one a gallery cannot answer. A gallery can be green
on every primitive while the API underneath it is miserable.

A thin `instar-sdk` grows from whatever the Calculator makes painful, and from
nothing else. Its job is to **make constructing an authoritative semantic
snapshot pleasant** —

```rust
column((
    text(display),
    row((button("7"), button("8"), button("9"))),
))
```

— and explicitly not to become a component framework, a signals system, a hooks
analogue, or a reconciliation layer. The host already reconciles; a second
reconciler in the guest would be the delta protocol arriving through the back
door.

## Closing Phase 2

The claim, deliberately narrow:

> A Wasm guest can describe a normal desktop interface declaratively while
> Instar provides retained layout, rendering, scrolling, transient interaction,
> keyboard and focus behaviour, and accessibility — without polling and without
> continuous guest participation.

That is a substantial proof and it is not "Instar is ready for Scratchpad".
Stretching it would be claiming the text editor works before anything has tried
to write one.

### Two research items, kept apart from feature work

**Runtime memory.** There is enough evidence to stop hand-waving and not enough
to optimize. A dedicated gate after Phase 2 measures 1, 2, 5 and 10 empty
Instar apps for RSS, PSS or private dirty where the platform offers it, virtual
memory, thread count, linear memories, compiled code, `Engine` sharing, and
`Store` cost. The question is whether the ~41 MB kernel and Wasm addition is
genuinely additive *physical* memory per app, and no architectural decision
about it should be taken before that is known.

**The 105 ms Stage 1 outlier** stays recorded and unexplained. Current runs sit
near 5 ms with small tails, which is evidence about the tree as it is now and
not an explanation of the historical reading. Do not chase it unless it
reproduces; do not quietly delete it either.

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
