# Instar architecture

The shape of the system as it stands, and why each boundary is where it is.

`PHASE-1.md` and `PHASE-2.md` record decisions in the order they were made,
including the ones that turned out wrong and what replaced them. **This
describes how Instar works now.** If the two ever disagree, this file is the
one to trust and the phase log is the one to correct.

## The whole thing

```text
                    ┌─────────────────────────────────────────┐
                    │            instar-shell                 │
                    │  winit · softbuffer · Vello CPU · font   │
                    └────────────────┬────────────────────────┘
                                     │
        ┌────────────────────────────┼────────────────────────────┐
        │                            │                            │
┌───────▼────────┐          ┌────────▼────────┐          ┌────────▼────────┐
│ instar-window  │          │  instar-host    │          │  instar-paint   │
│ OS input only  ├─────────►│  orchestration  │◄─────────┤  scene types    │
└────────────────┘          └────┬───────┬────┘          └─────────────────┘
                                 │       │
                    ┌────────────▼──┐ ┌──▼──────────────┐
                    │  instar-ui    │ │ instar-kernel   │
                    │ tree · layout │ │ Wasmtime · WIT  │
                    │ hit-testing   │ │ generations     │
                    └───────┬───────┘ └────────┬────────┘
                            │                  │
                    ┌───────▼──────────┐       │  WIT
                    │instar-ui-protocol│       │
                    │  wire format     │◄──────┴───► guest component
                    └──────────────────┘
```

Nine crates. The two at the bottom are the only ones a guest can reach, and
that is the load-bearing fact about the whole diagram.

| Crate | Owns | Never knows about |
|---|---|---|
| `instar-ui-protocol` | the wire format: opcodes, `NodeKey`, hard bounds | anything at all — zero dependencies |
| `instar-kernel` | Wasmtime, generations, operations, event delivery | windows, layout, pixels, UI |
| `instar-ui` | the retained tree, Taffy layout, hit-testing | DPI, windows, the guest |
| `instar-window` | winit translation, DPI conversion | node identity, hit-testing, trees |
| `instar-paint` | paint intent: scene and command types | how any of it is rasterized |
| `instar-render-vello-cpu` | a `PaintScene` → premultiplied RGBA8 | windows, the guest, layout |
| `instar-host` | routing, the metrics barrier, the two-thread bridge, scene lowering | a renderer, a window system, a font |
| `instar-shell` | the event loop, presentation, the font, the binary | — it is the top |
| `instar-guest-build` | compiling guests from build scripts | runtime anything; it is a build-dependency |

## Five boundaries, and what each is protecting

### 1. A guest links the wire format and nothing else

`instar-ui-protocol` is tiny, has zero dependencies, and is the only Instar
crate a guest ever links. That is what lets `instar-ui` take on a layout engine
and `instar-host` take on a renderer: none of it can reach a guest, so none of
it becomes a compatibility obligation.

Enforced by `crates/instar-shell/tests/layering.rs` as a **subset rule** — the
set of Instar crates a guest links must be a subset of
`{instar-ui-protocol}` — rather than as a blocklist. A blocklist stops
covering the crate that does not exist yet.

### 2. The host owns geometry, entirely

A guest sends layout *intent*. It cannot express a rectangle, because the
protocol has no way to encode one — the layout section was removed outright
rather than deprecated.

```text
guest: "a column of these, each stretching across it"
host:  every number on the screen
```

`LayoutSnapshot` is an internal `instar-ui` product, not protocol state. Taffy
is an implementation detail of one file.

The vocabulary is four orthogonal questions, and keeping them orthogonal is the
design:

```text
preferred size         Content | Fixed(u16)   plus min/max bounds
main-axis expansion    grow
main-axis contraction  shrink
cross-axis filling     align_self: Stretch
```

**There is no `Fill`.** There was, and it meant three different things once a
second axis existed: cross-axis stretch under a column, height under a row, and
content-size on a row's main axis. One name for three behaviours implies a rule
nobody can hold in their head, so it was deleted while the protocol was still
cheap to break. An SDK may offer `ui.width(Fill)` as sugar and lower it per
context — the sugar is allowed to be clever; the wire is not.

### 3. DPI is converted low and known high

```text
instar-window   owns physical↔logical conversion; emits logical input
instar-ui       operates entirely in logical coordinates; never sees DPI
instar-host     the only layer bridging logical presentation to physical
                rendering
```

Scale is not hidden from the whole host — a renderer needs it — but UI
semantics and hit-testing stay scale-free.

### 4. Two threads, because two libraries each need to own one

winit requires its event loop on the main thread and its `EventLoop` is
deliberately neither `Send` nor `Sync`. Wasmtime ships no executor and expects
the embedder to poll. Those do not fit on one thread.

```text
MAIN THREAD                       RUNTIME THREAD
winit EventLoop                   instar-kernel
instar-window        bounded      RuntimeGeneration
instar-ui           messages      Wasmtime Store
layout/hit-test/render  <----->   guest run_concurrent task
```

`EventLoopProxy` is `Send + Sync` and exists precisely so another thread can
wake the loop. The queues are bounded at 256 in each direction and the winit
side never blocks on them.

**UI commit is async at the guest boundary.** The retained tree belongs to the
presentation side; the guest's `commit(batch).await` suspends while the main
thread applies it and replies over a one-shot. The alternative —
`Arc<Mutex<Tree>>` mutated by the runtime thread — was rejected: it can block
the window thread behind a guest and leaves nobody owning the interface.

### 5. The host may lower to paint intent; it may not rasterize

`instar-host` takes `instar-paint` for scene *types* only. It is the layer that
bridges logical presentation to physical rendering, so turning a laid-out tree
into a `PaintScene` belongs to it. Choosing what rasterizes that scene does
not, and lives in `instar-shell`.

Enforced by a test: `instar-host` must depend on `instar-paint` and must not
depend on `vello_cpu` or `softbuffer`.

`skrifa` was on that list and is not any more. Text shaping lives in
`instar-ui` by design, so `skrifa` arrives as
`instar-host → instar-ui → parley → skrifa`, and the host cannot avoid it
without the UI layer losing its text stack. What the rule protects — that the
host does not choose what draws pixels — is unchanged.

## The rules that are not about dependencies

### The metrics barrier

A scale change opens a barrier that a coherent `WindowMetricsChanged` closes.

```text
Blocked:                          Ready(new):
- no layout                       - recompute layout first
- no UI hit testing               - replace LayoutSnapshot
- no UI activation                - re-lower the scene
- no app-content render           - then input and rendering resume
- redraw request becomes pending
- pointer position may update
- close/lifecycle still works
```

It is a synchronization barrier, not a render guard: input needs it as much as
rendering, because a cursor position converted with the new scale is meaningless
against a layout computed for the old viewport — a click during the barrier
resolves to the *wrong* node rather than to none.

Modelled as a type, not a flag: the only way to obtain usable metrics is
`MetricsState::usable()`, which returns `None` unless `Ready`. Stale values are
reachable only through a name that says so.

### The commit ordering is normative

```text
receive UiCommit
-> check RuntimeGeneration      (before anything else)
-> only then decode bytes
-> validate semantics
-> apply atomically
-> layout
-> lower to PaintScene
-> request redraw
-> reply
```

Screening before decoding means a dead guest cannot make the host spend parser
and allocation work on its behalf. **The type carries that rule, not a
comment**: `CommitRequest` has no accessor for its bytes, and the only route to
them is `screen(current_generation)`.

The reply comes last, and after layout rather than after the tree swap: a guest
resuming from `commit().await` should mean "the host accepted this as a usable
presentation state".

### Every commit is answered exactly once

`Accepted`, `Invalid`, `StaleGeneration`, or `HostUnavailable` — and the last is
the *default*, not a case an exit path must remember. The reply one-shot is
owned by a guard whose `Drop` answers `HostUnavailable`. A guest parked forever
inside `commit().await` is the worst failure this design can produce: silent,
timeout-proof, invisible to every counter. It is made structurally unreachable
rather than merely avoided.

### Back-pressure extends to the final consumer

A bounded queue feeding an unbounded one is not bounded. The runtime thread
reserves capacity in the guest's inbox *before* dequeuing a command, so pressure
propagates back to the winit thread instead of pooling in a hidden reservoir.
Cancellation deliberately does not back-pressure this way — a saturated inbox is
exactly when cancelling is most wanted.

### Lifecycle control does not compete for bounded capacity

Shutdown travels out of band and jumps the queue, because the state most in need
of shutting down is exactly the state where the queue is full.

### A guest commits snapshots; the host diffs them

There is no mutation protocol and no patch opcode. A guest sends a whole
interface every time, and the host diffs it against the retained tree by
`NodeKey`.

```text
full snapshot  ->  host diff  ->  incremental host work
```

**Host nodes are not destroyed and recreated because another snapshot
arrived.** The snapshot is authoritative as a *description*; the retained tree
is the interaction, layout, and render object, and it persists across commits.

Chosen over guest-sent deltas because a guest that mis-tracks its own dirty
state cannot desync the host — structurally impossible rather than merely
tested for. Recovery from any confusion is "send another snapshot".

Two counters, deliberately not one:

```text
commit_sequence   every accepted submission; what the guest sees, so its
                  synchronization does not depend on whether the host found
                  the snapshot interesting
tree_revision     the version of the retained state; advances only when the
                  diff found something; what caches key off
```

An identical re-commit therefore costs the decode and nothing else: no layout,
no scene, no frame.

### Node identity is generational

```rust
struct NodeKey { id: u32, generation: u32 }
```

An id that is removed and reused comes back at a higher generation. Without
that, this sequence mis-delivers and nothing catches it:

```text
ButtonActivated(7) queued  ->  guest removes node 7  ->  guest re-adds node 7
                           ->  old event delivered   ->  lands on the NEW node 7
```

By the time an action is queued it is opaque bytes the host cannot recall. It
no longer has to: the bytes carry the generation, so a guest comparing against
its own live keys rejects an activation for a node it has since replaced.

The guest chooses ids, so the host enforces monotonicity, and the ledger is
itself bounded because `MAX_NODES` bounds live nodes and not a guest burning
new ids forever:

```text
id never seen before   ->  generation must be 0
id currently live      ->  generation must match exactly
id retired             ->  generation must be > previous
per runtime generation ->  at most 65,536 distinct ids ever observed
```

Uniqueness *within* a snapshot is keyed on the id alone: `(7,0)` and `(7,1)`
are distinct keys but still one id claiming to be two live nodes.

The pair also packs losslessly into an AccessKit id
(`generation << 32 | id`), so remove-then-reuse becomes a new accessibility
object rather than recycling one a screen reader may still hold.

### Absent, invisible, and clipped are three different things

```text
Display::None        retained in the tree, absent from layout, paint,
                     hit-testing and accessibility; descendants likewise
Visibility::Hidden   keeps its space; no paint, no hit-test, no
                     accessibility; suppresses the whole subtree
Overflow::Clip       layout unaffected; descendant paint and hit-testing
                     intersected with this node's rect; nested clips
                     intersect
```

One line separates the first two: `None` leaves layout, `Hidden` stays in it.
Everything else they suppress is identical, which is why they are two names
rather than one property with a flag.

`Hidden` is subtree-wide, deliberately unlike CSS, where a descendant can set
`visibility: visible` and reappear inside an invisible ancestor. That makes "is
this node visible?" a walk to the root rather than a lookup.

**`Overflow` has no `Scroll` variant.** CSS makes scrolling a value of the
overflow property; copying that would make CSS's overflow model Instar's
architecture by accident. `Clip` is a rectangle intersection holding no state.
Scrolling is a node kind with a host-owned offset and a retirement obligation,
and a property value cannot carry that.

### Scroll is a retained viewport the guest cannot aim

```text
guest owns    the content
host owns     where that content is scrolled to
```

The offset appears nowhere on the wire in either direction. A guest cannot set
one, read one, or veto a change to one — which is what lets a wheel event move
the view with no Wasm round trip, and means a guest cannot scroll a view out
from under someone reading it.

A `Scroll` takes exactly one content child. That gives one unambiguous content
extent, and it stops `Scroll` becoming a layout container as well as a
viewport, which is two things wearing one name.

Both traversals run the same ordering:

```text
ancestor clip  ->  this node's clip  ->  translate  ->  descend
```

The clip comes first because the other order reports hits on content scrolled
out of view — inside a child's translated rect, outside the viewport that owns
it. The clip travels with the pointer, which only matters once something
*above* the scroll also clips.

Retention follows the node, not the pixels:

```text
commit that keeps the Scroll alive   offset survives unchanged
content shrinks                      clamped before the next presentation
                                     becomes interactive
Display::None / Visibility::Hidden   no interaction; offset retained
the node is deleted                  offset destroyed with it
```

A wheel delta goes to the deepest viewport under the pointer, which takes what
it has room for; **the remainder bubbles outward**. "The nearest scroll owns
the whole event" is the classic nested-scroll trap, where an inner viewport at
its limit swallows input that should have kept scrolling the outer one.

### Continuous interaction is host-local, structurally

A pressed button is drawn pressed, a wheel moves a viewport, and neither
consults the guest. The guest hears about *completed* interactions; everything
between the finger going down and an outcome existing belongs to the host.

This is not a policy the code follows — there is no branch in the scroll path
that can reach the guest at all. The zero-`SendToGuest` property holds because
the path does not exist, rather than because a test watches one that does.
Hover, focus, caret blink, selection, sliders, and drag previews inherit the
same arrangement.

### Transient interaction state is host-owned

A pressed button is drawn pressed, and that frame is requested without consulting
the guest. The guest hears about *completed* interactions; anything between the
finger going down and the outcome existing belongs to the host. This
generalizes to hover, scrolling, caret blink, selection, sliders, and drag
previews.

### The crash surface is host-owned and is not a UI tree

When a guest traps there is no guest left to describe anything, so the crash
screen is emitted as paint commands directly. Synthesizing a tree that says
"the app crashed" would mean the host can author interfaces in the guest's
name, and every downstream consumer would be told a lie by a layer whose job is
transcription. The retained tree keeps saying whatever the guest last said.

The surface is itself bounded: 32 KiB / 512 lines, capped where the state is
built rather than where it is drawn, with the complete diagnostic still going
to the log.

### Retirement: the node is gone, forget everything about it

> Any host transient state referencing a removed `NodeKey` is retired before
> the new snapshot becomes interactive.

And its counterpart, because hiding is not deletion but has the same
consequence for input:

> When a subtree becomes non-interactive through `Display::None` or
> `Visibility::Hidden`, any host transient state referencing it is retired
> before the new state becomes interactive.

Press and scroll offsets obey both today; focus, hover, and pointer capture
join them as they arrive. Deletion destroys a scroll offset, hiding retains
it — a node the guest hid is still a node the guest has, and returning to where
you were when it reappears is what a user expects.

One thing retirement cannot reach is an action already encoded and queued for
the guest. That is what generational keys close, from the other end.

### `measure()` is observational

Taffy probes a text node many times per layout pass under different
constraints, and its last call is not necessarily at the node's final width.

> `measure()` may perform temporary work needed to answer the current sizing
> query, including line-breaking for `Definite(width)`, but it must not mutate
> finalized presentation state or invalidate reusable artifacts based on
> speculative constraints.

```text
MinContent / MaxContent   intrinsic query only, from a cached ContentWidths
Definite(width)           may line-break to compute height; must not touch
                          finalized_width or the shaped artifact
finalize(actual_width)    owns finalized_width, persistent line-break state,
                          and ShapedText extraction
```

The invariant is not "measure never mutates" — a real height needs a real
break — but that speculative probes cannot poison the finalized cache. Getting
this wrong is not a correctness failure, it is a 9× latency failure that no
test would have reported.

## Guest lifetime

```text
Guest lifetime boundary = Store + component instance.

Never: drop a suspended guest future, then reuse that Store.

Teardown:  mark generation dead -> stop accepting its commits
        -> cancel host-owned child operations -> drop instance and Store
        -> create fresh Store + instance -> increment generation
```

Per-operation cancellation is a **separate** mechanism: the host cancels one
operation and the guest task stays alive. Dropping the main guest future is
reserved for destroying a whole generation.

This came out of Gate 0, which found that abandoning a started guest task
retains its runtime bookkeeping. Wasmtime documents the same thing: cancelling a
concurrent task requires dropping the `Store`.

## What the tests assert, and in which profile

Debug and release deliberately assert different things, permanently.

```text
debug     completion, ordering, cancellation, no-hang, boundedness
release   latency: p50, p95, p99, and an outlier guard
```

Debug timings measure rustc's optimization level more than Instar's design: a
single debug reading of 386 ms once sent an investigation hunting an
architectural defect that release measured at 5 ms. The defect was real; the
number that raised the alarm was 75× the number that mattered.

The split is written with `cfg!`, never `#[cfg]`. A `#[cfg]`-gated pair of
bodies is only type-checked in the build it belongs to, and since CI builds
debug, the release half of this once sat calling *itself* — asserting nothing,
and a stack overflow waiting for the first release run.

> A gate that hides a body from the compiler hides its bugs too.

### A performance-invariant test must observe entry, not cost

> When the property is "this path is not taken", assert that the forbidden
> work was **not entered**. Do not assert that it was cheap.

Instar's paint-only invariant — a colour change must not reshape text — was
first tested by asserting the text cache reported no rebuilds, no
re-line-breaks and no extractions. Injecting the exact regression it existed to
catch left it green: those counters stay at zero when layout *does* re-run,
because the cache simply hits. The test proved the expensive work was cheap,
which is a different claim from the one it was written to make.

What distinguishes them is a counter that increments on *consultation*:
`reused` fires if and only if something asked the text system a question. The
full expected tuple for a foreground-only commit is therefore

```text
rebuilt 0   relinebroken 0   reused 0   extracted 0
```

and the fourth is the load-bearing one.

This generalizes past text. If a paint-only update ever runs Taffy and Taffy
grows an effective cache, timings and downstream counters will look fine again
while the architecture has regressed. The question to test is always "did
anything enter this path", never "was entering it expensive".

The corollary is a habit, not a rule:

> A test that stays green under the fault it was written to catch is evidence
> about the injection first, and about the test second.

Both times that has happened here, it was the test. Deliberate fault injection
gets an assertion that the injection actually applied — a substitution that
silently matches nothing produces a green run that means nothing.

The latency bounds are asserted **on request**, not by default:

```text
INSTAR_LATENCY_GATE=1 cargo test --release -p instar-host --test bridge -- --nocapture
```

The distribution prints on every run in every profile; what is opt-in is the
*judgement*, because a judgement needs a host doing nothing else. Every run
says on its own output when it is only reporting, so a deferred judgement
cannot quietly become a forgotten one. See `docs/baselines/PERFORMANCE.md`.

## Known scaffolding

Recorded so it is not mistaken for design:

- **One window, one guest.** `PresentationState` is per-runtime, not per-window.
- **No quota enforcement.** A guest that spins on CPU without yielding is out of
  scope.
- **Runtime memory is unmeasured per app.** The ~41 MB kernel and Wasm addition
  has never been checked for whether it is genuinely additive *physical* memory
  across several running apps. A dedicated gate answers that before any
  architectural decision rests on it.
- **No scrollbar chrome.** Wheel and touchpad scrolling work; there is nothing
  drawn to drag. Package D, and the completion of `Scroll`.
- **No focus or keyboard.** Package E, and the point at which generational
  `NodeKey` earns its place a second time — focus is exactly the kind of
  long-lived reference that outlives the node it names.
- **No accessibility.** Package F. `NodeKey::to_accesskit_id` is ready; the
  mapping is not written.
- **No SDK.** A guest builds a snapshot through `BatchEncoder` directly, which
  is tolerable for a counter and is expected not to be for a calculator. The
  thin SDK grows from what package H finds painful, and from nothing else.
