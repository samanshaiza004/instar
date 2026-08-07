# Instar Phase 1

> **Provenance — read this first.**
>
> This document is a **reconstruction**, not the original Phase 1 plan. The
> original was written in a working session and never committed; by the time
> that gap was noticed, the source text was no longer recoverable. Everything
> below is drawn from one of three places, marked per section:
>
> - **[artifact]** — recoverable from the repo itself (committed code, WIT
>   comments, `TOOLCHAIN.md`, `GATE-0.md`, commit messages, tags).
> - **[directive]** — stated directly by the project owner and transcribed
>   here verbatim or near-verbatim.
> - **[inferred]** — reconstructed from surrounding evidence. Treat as
>   provisional; correct it rather than building on it.
>
> If the original plan text resurfaces, replace this file with it wholesale.
> Nothing here should be treated as authoritative over the original.

## Premise [artifact]

The predecessor codebase (Youth, tag `managed-youth-final`, commit `fac5f8d`)
drove its runtime with a polling loop and a 10ms epoch ticker thread. Instar's
premise is that a guest should sit idle at zero cost and be woken by the host.

That premise is only worth building on if the WebAssembly Component Model's
async support genuinely supports it — so it was tested first, before anything
was built on it. See [GATE-0.md](GATE-0.md).

## Hard idle gates [artifact]

Recorded in `crates/instar-kernel/wit/world.wit` and enforced by
`crates/instar-kernel/tests/gate0.rs`:

> A guest that calls `next-event` in a loop and does nothing else between calls
> must show zero host-import calls, zero polling, and zero CPU use while
> settled.

No permanent polling thread may exist in `instar-kernel`. This is why Youth's
`youth-epoch` ticker was deleted outright rather than ported disabled — see the
rationale in `crates/instar-kernel/src/engine.rs`.

## Forbidden dependencies [artifact]

`instar-kernel` must never depend on winit, Taffy, Vello, softbuffer, a text
renderer, `instar-ui`, or counter-specific types. Stated in the crate's own
docs and `Cargo.toml` description; verified in WP2 via `cargo metadata`.

## Guest lifetime boundary [directive]

Established after Gate 0 exposed that abandoning a started guest task retains
its runtime bookkeeping (see [GATE-0.md](GATE-0.md), Finding 5):

```text
Guest lifetime boundary = Store + component instance.

Never:
drop a suspended guest future
then reuse that Store as if nothing happened.

Guest cancellation/restart:
1. mark generation dead
2. stop accepting its commits
3. cancel host-owned child operations
4. drop the whole instance/Store
5. create fresh Store + instance
6. increment generation
```

Per-operation cancellation is a **separate** mechanism:

```text
guest asks host to cancel operation X
→ host cancels X
→ guest task stays alive
```

Dropping the main guest future is reserved for destroying an entire guest
generation. It is not the per-operation cancellation path.

The correctness boundary that falls out of this:

```rust
if completion.generation != current_generation {
    discard();
}
```

## UI layering [directive]

Established at WP5.5, after WP5 exposed that a guest was linking the host's UI
implementation just to share an encoding.

```text
winit WindowEvent
      |
instar-window        translates OS input only
  RawPointerEvent { logical_pos, button, state, window_id }
      |
instar-host          orchestration
      |
instar-ui            hit test, disabled check, pressed/capture state
  UiAction::ButtonActivated(NodeKey)
      |
instar-host
      |
instar-kernel -> guest
```

Dependencies (`instar-paint` and `instar-shell` added in WP7B2):

```text
instar-window --+
                |
instar-ui ------+--> instar-host --> instar-kernel
                |         ^
instar-paint ---+         |
                    instar-shell --> instar-render-vello-cpu, softbuffer,
                                     winit, skrifa
```

`instar-host` takes `instar-paint` for scene *types* only — no backend, no
font, no window. It is the layer that bridges logical presentation to physical
rendering, so lowering a laid-out tree to a `PaintScene` belongs to it;
choosing what rasterizes that scene does not, and lives one layer up in
`instar-shell`.

**No `instar-window -> instar-ui` edge.** `instar-window` must never know
`NodeKey`, tree revisions, button semantics, or hit-testing: winit is window
and event infrastructure, and widget routing belongs above it. Hit-testing
lives in `instar-ui` because it is tree and presentation behaviour.

`instar-ui-protocol` is a fourth, tiny crate underneath: wire format only —
version, opcodes, primitives, `NodeKey` representation, explicit
encoder/decoder helpers, hard bounds. Zero dependencies, and the *only* Instar
crate a guest links for UI. `instar-ui` is free to take on Taffy and anything
else it needs precisely because none of it can reach a guest.

Encoding stays manual and byte-defined. No Serde, no bincode, no `repr(C)`.

### DPI: converted in `instar-window`, not hidden from the host [directive]

Winit reports cursor positions in physical pixels and expects the application
to convert using the window's current per-window scale factor, which can change
at runtime when a window moves between monitors or display settings change.
`instar-window` owns that conversion — but scale factor is *not* hidden from the
whole host, because the renderer needs it for physical rasterization and text
quality, and future IME candidate geometry needs converting back to OS
coordinates.

```rust
RawPointerEvent { window_id, logical_pos: LogicalPoint, button, state }

WindowMetricsChanged { logical_size, physical_size, scale_factor }
```

```text
instar-window   owns physical<->logical conversion; tracks ScaleFactorChanged;
                emits logical input and WindowMetricsChanged
instar-host     knows physical size + scale; gives a logical viewport to
                instar-ui and a physical target + scale to the renderer
instar-ui       operates entirely in logical coordinates; never sees DPI
```

**Invariant:** `instar-window` normalizes OS coordinates; `instar-ui` speaks
logical coordinates; `instar-host` is the only layer bridging logical
presentation to physical rendering. UI semantics and hit-testing stay
scale-free; presentation does not.

Scale changes are atomic with metrics: the stored factor is updated before any
subsequent pointer event is translated. `ScaleFactorChanged` is winit's
documented way to track runtime DPI changes.

Metrics are never published mixed. Winit reports the new scale alongside an
`InnerSizeWriter` rather than the resulting physical size, and a following
`Resized` is not a documented cross-platform guarantee:

```text
ScaleFactorChanged
  -> update scale immediately
  -> clear stale cursor
  -> mark metrics_pending          (emit nothing)

Resized
  -> update physical size
  -> emit coherent WindowMetricsChanged

AboutToWait, if still pending
  -> query window.inner_size()
  -> emit coherent WindowMetricsChanged
```

Winit applies the OS-suggested size after the scale callback unless the
application overrides it, so flushing at the end of the event cycle yields a
coherent scale + actual size even where no separate resize arrives.

### The metrics barrier [directive]

A scale change opens a barrier that a coherent `WindowMetricsChanged` closes.
`instar-window` signals it; `instar-host` enforces it.

```text
ScaleFactorChanged
  -> window updates conversion scale
  -> clears old cursor
  -> emits MetricsInvalidated(window)
  -> metrics_pending = true

while metrics_pending:
  -> no render
  -> no pointer hit-testing/activation
  -> close/native lifecycle still works
  -> latest pointer position may be retained

Resized / about_to_wait
  -> publish coherent WindowMetricsChanged
  -> metrics_pending = false
  -> host recomputes layout
  -> then interaction and rendering resume
```

This is a synchronization barrier, not a render guard. Input needs it as much
as rendering: a cursor position converted with the new scale is still
meaningless against a layout computed for the old logical viewport, so a click
during the barrier resolves to the *wrong* node rather than to nothing. Winit
runs `about_to_wait` after queued window events and redraw callbacks, so a
`RedrawRequested` can arrive between the scale change and the flush — which is
why the barrier is signalled immediately rather than inferred from the flush.

`MetricsInvalidated` deliberately carries no size and no scale. Its entire
meaning is "previous presentation geometry is temporarily unusable"; values
would invite a host to use them.

Close policy lives in `instar-host`, not `instar-window`: winit leaves it to
the application, and a host may want to ask its guest first, ignore the
request, or close one window of several. The window layer reports
`CloseRequested` and does nothing about it.

`instar-window` is the only crate whose public vocabulary may contain winit
types. `WindowId` is an Instar newtype, so a future alternate window backend
and headless tests both stay clean.

### Geometry is the host's, entirely [directive] — DONE in WP7A

Guest-supplied rectangles are gone. The layout section was **removed from the
protocol outright** rather than deprecated: a guest cannot express a rectangle
even deliberately, so it cannot become authoritative over geometry.

A guest sends layout *intent*; the host computes every number:

```text
width:   Fill | Content | Fixed
height:  Content | Fixed          (no Fill -- see below)
padding
gap
```

Node kinds are `Root`, `Column`, `Text`, `Button`. No grid, no general CSS
surface, no arbitrary positioning. Everything is a flex column.

`Fill` height is rejected: a column of fill-height children has no defined
distribution, and choosing one silently would be inventing layout semantics
rather than implementing them.

**`LayoutSnapshot` is an internal `instar-ui` product, not protocol state.**
Taffy is an implementation detail of `instar-ui/src/layout.rs`; no Taffy type,
`NodeId`, or tree handle appears in any public API, and Taffy's output is
translated immediately into Instar's own `NodeKey -> Rect` snapshot.

Text measurement is currently a placeholder (fixed character metrics) so that
layout is deterministic and testable before a font stack exists. Layout tests
assert *relative* geometry — ordering, containment, non-overlap, monotonicity —
rather than exact pixels, so real shaping can replace it without invalidating
them.

## Toolchain [artifact]

Wasmtime 47.0.3, wit-bindgen 0.60.0, Rust 1.97.1 stable, `wasm32-wasip2`,
wasm-tools 1.255.0. Chosen by head-to-head comparison rather than inherited.
Full reasoning, including the specific upstream async fixes that drove each
pin, is in [TOOLCHAIN.md](TOOLCHAIN.md).

`wasmtime-wasi` brings Tokio in transitively. There is no goal of eliminating
Tokio from the kernel for its own sake — measure first, optimize from evidence.
[directive]

## Measurement policy [artifact]

> Memory and startup are discovery metrics during Phase 1. Do not invent
> targets before measuring the actual baseline.

The pre-rewrite baseline lives in `baselines/managed-youth-final/` and is
partial; what is and isn't captured is documented there.

## Work packages

WP0–WP3 are complete. [artifact — task history and commits]

| WP | Scope | Status |
|---|---|---|
| WP0.1 | Tag `managed-youth-final`, branch `instar-phase-1` | done |
| WP0.2 | Preserve and commit inherited in-progress cleanup | done |
| WP0.3 | Capture the `managed-youth-final` baseline | done (partial — see baseline docs) |
| WP0.4 | Rename `youth-paint`, `youth-render-vello-cpu` → `instar-*` | done |
| WP1 | Choose and pin the toolchain; build an empty component fixture | done |
| WP2 | Scaffold `instar-kernel` with Component Model async enabled | done |
| WP3 | Headless kernel spike — **Gate 0** | done, GO |

Remaining sequence [directive]:

**WP4 — runtime lifecycle** (done)
- `RuntimeGeneration`
- one `Store` per generation
- operation registry
- protocol-level operation cancellation
- whole-generation teardown/restart
- stale completion rejection
- trap fixture
- abandoned-task regression fixture
- bounded-memory soak: 1,000 generation create/suspend/teardown cycles,
  asserting host bookkeeping stays bounded, RSS does not grow linearly, and
  active-task count returns to baseline

**WP5** — `instar-ui` plus minimal button interaction, together (done)

**WP5.5** — extract `instar-ui-protocol`; remove the recursive build-script
dependency; keep every attack and round-trip test; verify the guest graph
contains no `instar-ui` (done)

**WP6** — `instar-window`: winit translation only. No hit-testing, no
`NodeKey` knowledge, `ControlFlow::Wait`. (done)

**WP7A** — host layout: Taffy into `instar-ui`; guest layout section removed;
minimal layout properties; intrinsic text/button measurement; logical viewport
input; `LayoutSnapshot` generated host-side; button round-trip rewritten
against host layout. (done)

Exit gate, met: guest provides zero geometry; host produces a deterministic
`LayoutSnapshot`; hit-testing uses that snapshot; the counter fixture
round-trip still works.

**WP7B** — `instar-host` composition: compose window + ui + kernel. Enforce the
metrics barrier, route `UiAction` to guest events, own close policy. (done:
routing core in WP7B core, the two-thread bridge in WP7B1, rendering and crash
presentation in WP7B2)

Metrics capability is modelled directly rather than as flags:

```rust
enum MetricsState {
    Blocked { last_valid: Option<WindowMetrics> },
    Ready(WindowMetrics),
}
```

`last_valid` is diagnostics and cache context only — never lay out, hit-test,
or render from it. The only way to obtain usable metrics is `usable()`, which
returns `None` unless `Ready`, so "stale but present" is not reachable as a
usable value.

**Invalidating geometry cancels any interaction captured against that
geometry.** Normative, not incidental: a press recorded against a layout that
no longer describes the screen must not be completable, because the node under
the pointer may have moved. Enforced by `Interaction::cancel()` on
invalidation.

```text
Blocked:                          Ready(new):
- no layout                       - recompute layout first
- no UI hit testing               - replace LayoutSnapshot
- no UI activation                - then process actionable input/render
- no app-content render           - service pending redraw
- redraw request becomes pending
- pointer position may update
- close/lifecycle still works
```
### WP7B1 — the runtime/main-thread bridge [directive] — DONE

Two threads, actor-style. Winit and `run_concurrent` must **not** cooperatively
own one thread: winit requires its event loop on the main thread and its
`EventLoop` is deliberately not `Send`/`Sync`, while Wasmtime ships no executor
and expects the embedder to own polling. `EventLoopProxy` is `Send + Sync` and
exists precisely to wake the loop from another thread.

```text
MAIN THREAD                       RUNTIME THREAD
winit EventLoop                   instar-kernel
instar-window        bounded      RuntimeGeneration
instar-ui           messages      Wasmtime Store
layout/hit-test/render  <----->   guest run_concurrent task
```

```rust
enum RuntimeCommand { DeliverEvent(GuestEvent), CancelOperation(OperationId), Shutdown }

enum HostUserEvent {
    UiCommit { generation: RuntimeGeneration, request: CommitRequest },
    GuestTrapped { generation: RuntimeGeneration, error: GuestError },
    GuestExited { generation: RuntimeGeneration },
}
```

Flow: click -> `UiAction` -> `RuntimeCommand::DeliverEvent` -> runtime wakes
guest -> guest commits -> `HostUserEvent::UiCommit` via `EventLoopProxy` ->
main validates and applies -> layout -> `request_redraw`.

**UI commit becomes async at the guest boundary.** The authoritative retained
tree belongs to the main/presentation side. Do *not* share it as
`Arc<Mutex<UiTree>>` for the runtime thread to mutate synchronously — that
risks blocking the window thread and muddles ownership. Instead the guest's
`commit(batch).await` suspends while the main thread applies atomically and
replies over a oneshot. Wasmtime's concurrent calls are designed to suspend
while waiting on host work, so this is a genuine proof that host services can
marshal onto thread-affine platform owners without blocking the Wasm task.

Queues are bounded at 256 in each direction, and interactive sends from the
winit thread use non-blocking `try_send`: never block winit waiting for queue
capacity.

The Store-per-generation rule is unaffected — Wasmtime documents that
cancelling a concurrent task requires dropping the `Store`, and that dropping
the Rust future alone does not cancel the guest task, which matches Gate 0's
Finding 5.

#### WP7B1 acceptance gate [directive]

Main-thread ordering, which is normative:

```text
receive UiCommit
-> check RuntimeGeneration      (before anything else)
-> only then decode bytes
-> validate semantics
-> apply atomically
-> layout
-> request redraw
-> reply
```

Rejecting a stale generation *before decoding* means a dead guest cannot make
the host spend parser and allocation work on its behalf.

Required tests:

1. click -> guest event -> async commit -> accepted result -> guest re-suspends
2. an invalid commit returns rejection without mutating the tree
3. 100 rapid activations preserve order
4. a full main->runtime queue never blocks winit
5. runtime->main wake works while the event loop is in `Wait`
6. a pending bulk async operation does not delay a UI commit
7. a guest trap while a commit awaits produces no later mutation
8. teardown while a commit awaits resolves or cancels cleanly
9. an old-generation `UiCommit` is rejected before decoding
10. 1,000 complete click/commit cycles leave queue and operation counts at
    baseline

**Measure promptness, not just completion.** Wasmtime warns that a future
inside `run_concurrent` can go unpolled for an extended period even after its
waker fires, so "eventually completes" is not the property that matters here —
a round-trip that resolves in three seconds passes that test and is a broken
UI. The gate is that runtime->main->runtime round-trips make *prompt* progress
under concurrent load, with a measured bound rather than an eventual one.

#### What the gate found [artifact]

All ten tests live in `crates/instar-host/tests/bridge.rs`, each against a real
`wasm32-wasip2` guest in a real generation on a real second thread. The
promptness bound is 250ms per click-to-committed-tree round-trip, asserted with
a 3-second host operation in flight and again as the *slowest* of 1,000
consecutive cycles. Measured values sit two orders of magnitude below it.

The bound is asserted; the *distribution* is not. p50/p95/p99 are collected
over the 1,000-cycle run and printed, and stay observational until there are
numbers from a real windowed host to calibrate against — tightening CI against
an unloaded developer machine with no display server attached would be
inventing a target before the baseline exists, which is what the measurement
policy above rules out for memory and startup. The max is what the gate
asserts, deliberately: a p99 bound would permit ten of a thousand clicks to
take arbitrarily long, and the tail is exactly where a broken UI shows up.

**250ms is a deadlock and regression detector, not a performance target.**
[directive] Measured on a developer machine: p50 206µs, p95 215µs, p99 225µs,
max 475µs — roughly 0.2ms for a full guest→main→guest commit round trip,
against an 8–16ms display budget. The headroom is two orders of magnitude, so
the assertion's job is to catch something *stopping*, and the percentiles are
kept as telemetry rather than promoted into the assertion.

Two things the gate changed rather than confirmed. Both are now normative.

**Back-pressure must extend to the final consumer. A bounded queue feeding an
unbounded one is not bounded.** The guest's own event inbox was unbounded, so
the bounded main->runtime queue drained into it regardless of how far behind
the guest was: the "full queue" test could not make anything drop. The inbox is
now bounded too (`EVENT_QUEUE_CAPACITY`), and the runtime thread takes a permit
for it *before* dequeuing a command, so pressure propagates back to the winit
thread instead of pooling in a hidden reservoir:

```text
runtime command waiting
  -> reserve guest inbox capacity
  -> only then dequeue DeliverEvent
  -> deliver
```

Only event delivery back-pressures this way. Cancellation is deliberately not
held behind a full inbox, since a saturated inbox is exactly when cancelling is
most wanted.

**Lifecycle control must not compete with normal work for bounded queue
capacity.** Sending `Shutdown` over the same bounded queue meant it was
discarded in precisely the state that needed it — a full queue and a guest
suspended on an unanswered commit — and the thread never joined. Teardown must
stay deliverable while ordinary queues are saturated, so it travels out of band
and jumps the queue. `RuntimeCommand::Shutdown` remains for the ordered case,
where a caller genuinely wants the guest to see its pending events first.

**Every accepted commit resolves exactly once.** `Accepted`, `Invalid`,
`StaleGeneration`, or `HostUnavailable` — and the last is the *default*, not a
case an exit path has to remember. The reply one-shot is owned by a guard whose
`Drop` answers `HostUnavailable`, so a request dropped by a panicking main
thread, an early return, a disconnected queue, or a shutdown between submission
and application still wakes its guest. A guest parked forever inside
`commit().await` is the worst failure this bridge can produce — silent,
timeout-proof, and invisible to every counter — so it is made structurally
unreachable rather than merely avoided.

**UI commit became async at the guest boundary**, as specified:
`kernel-ui.commit` is an `async func` in `wit/kernel.wit`, and a new
`commit-error.host-unavailable` variant exists so a guest suspended on a reply
that will never arrive is woken with a verdict rather than left parked. The
kernel keeps its synchronous in-memory commit log for embedders that install no
sink, which is why WP4's and WP5's headless tests needed no rewrite.

### WP7B2 — rendering and crash presentation [directive] — DONE

```text
UiCommit accepted -> recompute layout if needed -> lower to PaintScene -> request_redraw
RedrawRequested   -> if MetricsState::Ready render, else defer
GuestTrapped      -> PresentationState::Crashed -> request_redraw
```

```rust
enum PresentationState {
    App,
    Crashed { generation: RuntimeGeneration, message: String },
}
```

The crash screen is host-owned; showing it requires no guest tree mutation.

**WP7B1 gates the first real windowed counter.** WP5 proved Wasm/UI semantics
and WP6 proved OS semantics; the two-thread crossing is the remaining
architectural risk after Gate 0. WP8 fixture work may proceed in parallel.

#### Lowering happens on commit, not on redraw [directive]

A frame callback is the worst place to discover work. Lowering at commit time
means the redraw path is "hand the backend a scene that already exists", and it
means everything that could still have refused a batch has happened before the
host promises a frame. The full normative order, extending WP7B1's:

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

**The reply comes after layout, not after the tree mutates.** A guest resuming
from `commit().await` should mean "the host accepted this as a usable
presentation state", not "the bytes entered a tree". Rendering itself need not
have completed — only the work that could still have refused the batch.

#### The scene is subject to the metrics barrier, more sharply than layout is

A `LayoutSnapshot` is logical; a `PaintScene` is *physical*, built for one
window size and scale. Presenting one across an invalidation would draw the old
window's geometry into the new window's buffer, so invalidation discards the
scene outright and `HostWindow::scene()` is gated on `Ready` exactly as
`layout()` is.

#### The crash screen is host-owned, and is not a UI tree [directive]

When a guest traps there is no guest left to describe anything, so the crash
screen is emitted as paint commands directly. Synthesizing an Instar tree that
says "the app crashed" and pushing it through the normal path is **rejected**:
it would mean the host can author interfaces in the guest's name, and every
downstream consumer — hit-testing, the commit log, anything that later asks
what the guest committed — would be told a lie by a layer whose whole job is
transcription. The retained tree keeps saying whatever the guest last said;
what the window *shows* is a separate question.

A clean exit is not a crash. A guest whose `run` returned did what it meant to,
and replacing its last interface with an error screen would report a failure
that did not happen.

**The crash surface must itself be impossible to overwhelm.** [directive] Trap
text is guest-influenced and effectively unbounded — a wasm backtrace runs to
hundreds of lines, and a guest may panic with a megabyte of its own choosing.
The surface the host puts up *because* something went wrong is the last place
that may become a way to make things worse, so what it retains and draws is
capped at **32 KiB / 512 lines**, whichever binds first, cut on a character
boundary and marked as truncated.

The cap is applied where the state is *built*, not where it is drawn: nothing
downstream — the scene builder, a `Debug` print, whatever reads `presentation()`
next — can then be handed the unbounded version. Truncation costs nothing,
because `HostEffect::GuestGone` still carries the complete diagnostic and the
shell logs it in full.

#### Press feedback is the host's [directive]

A pressed button is drawn pressed, and that frame is requested by
`instar-host` without consulting the guest — the guest is told about a
*completed* click and would be a runtime round-trip too late to provide
feedback for one in progress.

Generalized: **transient interaction state is host-owned and must never
require a Wasm round trip.** The same rule will cover hover, scrolling, caret
blink, selection, sliders, and drag previews. The guest hears about outcomes;
the host owns everything that happens between the finger going down and the
outcome existing.

#### Painting uses the advance layout measured with [directive]

`instar_ui::TEXT_METRICS` is public for exactly one reason: whoever draws text
must place glyphs at the same advance layout measured its boxes with. A painter
using its font's own advances produces text that drifts out of the rectangles
the host computed for it — and the host's geometry is the authority, not the
font's. The shell inverts its face's advance to pick the em size at which one
glyph occupies one layout column. Both sides are placeholders and are replaced
together, when real shaping lands.

That constraint is why the shipped face is monospaced. A proportional face
rendered at a fixed pitch would look wrong in a way that is nobody's bug in
particular.

> **Phase 2 debt — do not build on this.** [directive]
>
> `TEXT_METRICS` is acceptable Phase 1 scaffolding and must not become
> architecture. "Invert a real glyph advance until it matches fake layout
> columns" has to die *before* the UI service expands — every widget added on
> top of it is another thing to unpick, and the trick is only invisible while
> everything is monospaced and left-aligned.
>
> The end state is one shaped result driving both sides, rather than two
> approximations kept in sync by a shared constant:
>
> ```text
> text + style
>    ↓
> Parley shape/layout
>    ├── intrinsic size      → Taffy
>    └── positioned glyphs   → Vello CPU
> ```
>
> Parley already produces shaped width/height and positioned runs, and Vello
> CPU already accepts positioned glyph runs — `instar-paint`'s `GlyphRun` is
> that shape today. The pieces exist; what is missing is the font context, and
> introducing one is Phase 2 work.
>
> **Phase 1 is not to be expanded to fix this.** The signal to do the
> replacement is the first requirement that needs more from text than a
> fixed-pitch rectangle: a proportional face, wrapping inside a widget, mixed
> sizes, bidi, or a caret.

#### `instar-shell` [artifact]

A new topmost crate, and the only one that links a window, a renderer, and a
font at once: winit's event loop, `EventLoopProxy` as the bridge's wake, the
Vello CPU backend (its opt-in `glyph-run` feature enabled here and nowhere
else), softbuffer presentation, and the real counter guest built from source by
its build script. Everything below it stays testable without a display server,
and stays that way because none of it can reach these types.

**A frame that cannot be represented is not presented.** softbuffer's
`0x00RRGGBB` output carries no alpha, so a scene is checked for its opening
opaque `Clear` before anything is rasterized, and any error between rasterizing
and presenting drops the frame — a partially packed buffer is a torn mix of two
frames, and a stale frame is a far smaller problem than a half-drawn one.

Verified in `crates/instar-shell/tests/render.rs`: the real guest, on a real
thread, through the real host, rasterized by the real backend with the real
font, asserted on the resulting **pixels** — every pixel opaque, the host's own
colors on screen, glyph ink inside the boxes layout computed, a click visibly
changing the window, and a guest that really traps ending up as the host's
crash screen with the guest's last tree still intact underneath it.

**WP7 is complete.** [directive] The premise is proven end to end, not in
pieces: a guest that costs nothing while idle, woken by the host, describing an
interface it owns no geometry in, presented by a host that owns every pixel of
it — and replaced by a host-owned surface when it dies.

### Phase 1 closure [directive]

What remains is consolidation, not architecture. Adding architecture from here
is the failure mode to avoid.

```text
WP8   counter + fixture consolidation
WP9   full 3-OS CI + overhead profiles A–D
WP10  docs, dependency audit, dead-code/dependency cleanup
      final manual 3-OS window smoke
      -> Phase 1 closed
```

**WP8** — counter guest and fixture consolidation. Partly landed: the counter
guest the shell runs lives in `crates/instar-shell/guests/counter`, built from
source by the shell's build script and guarded by
`crates/instar-shell/tests/layering.rs`. What remains is consolidating the
three near-duplicate fixtures (`ui-guest`, `host-guest`, `counter`) and adding
breadth — guests that misbehave in more ways than trapping on demand.

**WP9** — CI rewrite; compare against the WP0.3 baseline; overhead profiles
A–D.

**WP10** — docs, dependency audit, dead-code and dependency cleanup. The
`youth-*` crates still in the workspace are the obvious candidates.

#### The final Phase 1 gate is manual, and that is correct [directive]

`softbuffer::Buffer::present()` is the actual platform presentation boundary,
and softbuffer backs AppKit, Win32, and Wayland/X11. The pixel tests in
`crates/instar-shell/tests/render.rs` prove everything immediately *before*
that call and deliberately do not claim to prove compositor presentation —
a claim that needs a compositor.

So the closing gate is a human running the app once per OS:

```text
window appears
-> Click me
-> visible count changes
-> Crash on purpose
-> visible host crash screen
-> window closes normally
```

**No screenshot automation.** Manual evidence is sufficient and appropriate for
this boundary; building a screenshot harness to assert what a person can see in
ten seconds would be the same category error as tightening a latency budget
against an unloaded machine.

## Gate contingency [directive] — CLOSED

Gate 0 was not to be considered globally closed on a single-platform result:
the product claim is Linux + Windows + macOS, so the contingency stayed open
until `.github/workflows/gate0.yml` passed on all three.

**It has.** All three platforms passed on 2026-08-07 (run 31182995718). The
Gate 0 contingency is closed and the GO verdict is global.

## Out of scope for Phase 1 [artifact]

Noted in `engine.rs` while deciding not to port epoch interruption: quota
enforcement and malicious-guest test suites are excluded. Guest hangs on
CPU-bound, non-yielding code are not a Phase 1 concern.
