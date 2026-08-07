# Instar architecture

The shape of the system as Phase 1 leaves it, and why each boundary is where it
is. `PHASE-1.md` records the decisions in the order they were made;
this describes the result.

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

A guest sends layout *intent*: `Fill | Content | Fixed`, padding, gap. It
cannot express a rectangle, because the protocol has no way to encode one — the
layout section was removed outright rather than deprecated.

```text
guest: "a column of these, filling the width"
host:  every number on the screen
```

`LayoutSnapshot` is an internal `instar-ui` product, not protocol state. Taffy
is an implementation detail of one file.

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
depend on `vello_cpu`, `softbuffer`, or `skrifa`.

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

## Known scaffolding

Recorded so it is not mistaken for design:

- **`TEXT_METRICS`** — fixed-pitch placeholder metrics shared by layout and
  painting, with the shell inverting a real font's advance to match them. Phase
  2 replaces both sides with one shaped result driving Taffy measurement and
  glyph positioning. See `PHASE-1.md`.
- **One window, one guest.** `PresentationState` is per-runtime, not per-window.
- **No quota enforcement.** A guest that spins on CPU without yielding is out of
  scope for Phase 1.
