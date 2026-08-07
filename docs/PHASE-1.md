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

Dependencies:

```text
instar-window --+
                +--> instar-host --> instar-kernel
instar-ui ------+
```

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

Close policy lives in `instar-host`, not `instar-window`: winit leaves it to
the application, and a host may want to ask its guest first, ignore the
request, or close one window of several. The window layer reports
`CloseRequested` and does nothing about it.

`instar-window` is the only crate whose public vocabulary may contain winit
types. `WindowId` is an Instar newtype, so a future alternate window backend
and headless tests both stay clean.

### Guest-supplied geometry is temporary [directive]

The explicit rects a guest sends today are WP5 scaffolding, not protocol
semantics. They travel in a separate, optional layout section rather than on
tree nodes, and feed `LayoutSnapshot::from_wire`. When the host computes layout
(WP7), it produces the snapshot itself and that section is deleted — with no
change to the tree format, which is the reason for the separation.

Leaving them would make the guest authoritative over geometry and undermine the
retained host presentation model.

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

**WP7** — `instar-host`: compose window + ui + kernel; replace the test rects
with host layout; UI owns hit-testing and interaction; host turns a `UiAction`
into a guest event.
**WP8** — counter guest and fixtures
**WP9** — CI rewrite; compare against the WP0.3 baseline

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
