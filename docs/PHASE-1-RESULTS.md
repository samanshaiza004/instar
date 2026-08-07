# Instar Phase 1 — results

What Phase 1 actually proved, what it cost, and what is still scaffolding.

Companion to `PHASE-1.md`, which records the decisions as they were made. This
records the outcome. Where the two disagree about what was *decided*, that one
is right; where they disagree about what was *measured*, this one is.

Tagged `instar-phase-1`.

---

## The claim under test

> A guest should sit idle at **zero** cost and be woken by the host.

The predecessor codebase (Youth, tag `managed-youth-final`, commit `78650bc`)
drove its runtime with a polling loop and a 10 ms epoch ticker thread. Instar's
whole premise is that the WebAssembly Component Model's async support makes that
unnecessary — which is only worth building on if it is true, so it was tested
before anything was built on it.

**It is true.** See below.

---

## 1. Gate 0 — the premise holds, and one constraint falls out of it

`docs/GATE-0.md`. A headless kernel spike, before any UI existed.

| # | Finding |
|---|---|
| 1 | Suspend and wake works across the async host-import boundary |
| 2 | An idle guest is genuinely idle: zero host-import calls, zero polls, zero CPU while settled |
| 3 | Concurrent async imports do not head-of-line block |
| 4 | Cancelled work stops and stays stopped |
| 5 | **Abandoned guest tasks retain state until the `Store` is dropped** |
| 6 | Clean shutdown works |

### Finding 5 is the one that shaped the architecture

Dropping the Rust future for a suspended guest task does **not** cancel the
guest task. Its runtime bookkeeping survives. Wasmtime documents the same
thing: cancelling a concurrent task requires dropping the `Store`.

That produced the rule everything else was built around:

```text
Guest lifetime boundary = Store + component instance.

Never: drop a suspended guest future, then reuse that Store.

Teardown:  mark generation dead
        -> stop accepting its commits
        -> cancel host-owned child operations
        -> drop the whole instance and Store
        -> create fresh Store + instance
        -> increment generation
```

Per-operation cancellation is a **separate mechanism** and must never be
confused with it: the host cancels one operation, and the guest task stays
alive. Dropping the main guest future is reserved for destroying an entire
generation.

The correctness boundary that falls out:

```rust
if completion.generation != current_generation {
    discard();
}
```

**Verdict: GO, globally.** The contingency required all three platforms before
Gate 0 could be considered closed — the product claim is Linux + Windows +
macOS, and "suspends without polling" has to hold against three schedulers, not
one. All three passed on 2026-08-07 (run 31182995718).

---

## 2. The async bridge — proven, with two bugs the gate found rather than confirmed

WP7B1. Two threads, because winit requires its event loop on the main thread
and its `EventLoop` is deliberately not `Send`/`Sync`, while Wasmtime ships no
executor and expects the embedder to poll.

**UI commit became async at the guest boundary.** The retained tree belongs to
the presentation side; `commit(batch).await` suspends while the main thread
applies it atomically and replies over a one-shot. The alternative —
`Arc<Mutex<UiTree>>` mutated by the runtime thread — was rejected because it can
block the window thread behind a guest and leaves nobody owning the interface.

This is a genuine demonstration that **host services can marshal onto
thread-affine platform owners without blocking the Wasm task**.

### The ten-test acceptance gate passed

`crates/instar-host/tests/bridge.rs`. Every test drives a real
`wasm32-wasip2` guest, in a real generation, on a real second thread.

### Promptness, measured — not "eventually completes"

Wasmtime warns that a future inside `run_concurrent` can go unpolled for an
extended period even after its waker fires. A round-trip resolving in three
seconds would pass an "eventual" test and is a broken UI.

Click → guest wake → commit → layout → reply, over 1,000 consecutive cycles:

| p50 | p95 | p99 | max |
|---:|---:|---:|---:|
| 206 µs | 215 µs | 225 µs | 475 µs |

Against a **250 ms** asserted ceiling — three orders of magnitude of headroom,
and roughly 1/40th of a single 8 ms display frame. The assertion is on the max,
not a percentile: a p99 bound would license ten of a thousand clicks to take
arbitrarily long, and the tail is exactly where a broken UI shows up.

The bound is a **deadlock and regression detector**, not a performance target.
The distribution is telemetry and is deliberately not asserted.

### Two bugs, now normative rules

**Back-pressure must extend to the final consumer.** The guest's own event
inbox was unbounded, so the bounded main→runtime queue drained straight into it
and the "full queue" test could not make anything drop. A bounded queue feeding
an unbounded one is not bounded. The runtime thread now reserves inbox capacity
*before* dequeuing a command, so pressure propagates back to the winit thread
instead of pooling in a hidden reservoir. Cancellation deliberately does not
back-pressure this way — a saturated inbox is exactly when cancelling is most
wanted.

**Lifecycle control must not compete with normal work for bounded capacity.**
`Shutdown` travelled on the same bounded queue, so it was discarded in
precisely the state that needed it — a full queue and a guest suspended on an
unanswered commit — and the thread never joined. Teardown now jumps the queue
out of band.

### And one hardening

**Every accepted commit resolves exactly once.** `Accepted`, `Invalid`,
`StaleGeneration`, or `HostUnavailable` — and the last is the *default*, not a
case an exit path must remember. The reply one-shot is owned by a guard whose
`Drop` answers `HostUnavailable`. A guest parked forever inside `commit().await`
is the worst failure this design can produce: silent, timeout-proof, invisible
to every counter. It is now structurally unreachable rather than merely avoided.

---

## 3. Presentation — the runtime became an application

WP7B2. The counter renders in a real window: winit, `EventLoopProxy` as the
bridge's wake, Vello CPU, softbuffer, a real font.

Verified in `crates/instar-shell/tests/render.rs` against actual **pixels** —
the real guest, on a real thread, through the real host, rasterized by the real
backend: every pixel opaque, the host's own colours on screen, glyph ink inside
the boxes layout computed, a click visibly changing the window, and a guest that
really traps ending up as the host's crash surface with its last tree intact
underneath.

Two ownership results worth naming:

**The crash surface is host-owned and is not a UI tree.** When a guest traps
there is no guest left to describe anything, so the screen is emitted as paint
commands directly. Synthesizing a tree that says "the app crashed" would mean
the host can author interfaces in the guest's name. The retained tree keeps
saying whatever the guest last said.

**Transient interaction state is host-owned.** A pressed button is drawn
pressed without consulting the guest — the guest is told about *completed*
interactions and would be a runtime round-trip too late otherwise. This
generalizes to hover, scrolling, caret blink, selection, sliders, and drag
previews.

---

## 4. Overhead

Full method and caveats in `OVERHEAD.md`. Apple Silicon, macOS, release build.

| Stage | RSS | Δ | Threads |
|---|---:|---:|---:|
| baseline (bare process) | 9.0 MB | — | 1 |
| **A** kernel + guest, settled | 50.4 MB | **+41.3 MB** | 11 |
| **B** + host: layout, routing, scene | 50.5 MB | **+96 KiB** | 11 |
| **C** + font + renderer, one frame | 52.0 MB | **+1.5 MB** | 11 |
| **D** after 100 full cycles | 52.1 MB | **+48 KiB** | 11 |

| | |
|---|---|
| Guest start to first commit | 51 ms |
| Layout + scene lowering | 177 µs |
| First rasterized frame | 0.55 ms |
| Slowest full cycle (click → raster) | 0.45 ms |
| Idle wakes / commits, every stage, 3 s window | **0 / 0** |
| `instar` release binary | 23.0 MB |
| `counter.wasm` (release) | 116 KB |

**Wasmtime is the cost.** 41 of the 43 MB Instar adds is stage A. Everything
Instar itself contributes fits in the remaining ~1.6 MB, and the orchestration
layer is 96 KiB — reproducible to the kilobyte across runs.

**Nothing accumulates.** After 100 guest wake-ups, commits, layouts, lowerings,
and 101 rasterized frames, the process is within 48 KiB of where it started;
one run finished 96 KiB *below*. A runtime that keeps a little of every
interaction dies after an afternoon, and this one does not.

**A settled Instar does nothing at all** — zero wakes, zero commits, at every
stage. The predecessor's ticker would have shown ~300 per window.

### Against the predecessor

Only the size comparison is like-for-like; Youth's runtime numbers were never
captured.

| | Youth | Instar |
|---|---|---|
| Desktop binary | 25.6 MB | 23.0 MB |
| Release build, full workspace | 42 min 17 s | 3 min 01 s |
| Idle behaviour | 10 ms ticker + polling loop | zero wakes |

---

## 5. Three-OS status

| | Linux | macOS | Windows |
|---|---|---|---|
| Gate 0 | ✅ passed | ✅ passed | ✅ passed |
| Full suite in CI | ⚙️ configured | ⚙️ configured | ⚙️ configured |
| Overhead profiles | measured on macOS; harness is portable to Linux | ✅ | not measured — needs a Win32 call, and therefore a dependency |
| **Manual window smoke** | ⬜ **pending** | ⬜ **pending** | ⬜ **pending** |

Gate 0's three-platform result is recorded and closed. The full-suite CI
(`.github/workflows/ci.yml`) is written and passes locally on macOS; its
Linux and Windows runs are pending a push.

### The manual smoke test is the honest final gate

`softbuffer`'s `present()` is the real platform presentation boundary. The pixel
tests prove everything immediately before that call and deliberately do not
claim to prove compositor presentation — that needs a display server and a
person.

Per OS, once:

```text
window appears
-> Click me
-> visible count changes
-> Crash on purpose
-> visible host crash screen
-> window closes normally
```

No screenshot automation. Building a harness to assert what a person can see in
ten seconds would be the same category error as tightening a latency budget
against an unloaded runner.

**None of the three has been done.** This is the one item between here and a
closed Phase 1 that cannot be done from a terminal.

---

## 6. What is still scaffolding

Recorded so it is not mistaken for design.

### `TEXT_METRICS` — the one to remove first

Fixed-pitch placeholder metrics, shared by layout and painting, with the shell
inverting a real font's advance until one glyph occupies one fake column. It
works, it is tested, and it must not outlive Phase 1: every widget built on top
of it is another thing to unpick, and the trick is only invisible while
everything is monospaced and left-aligned.

The end state is one shaped result driving both sides:

```text
text + style
   ↓
Parley shape/layout
   ├── intrinsic size      → Taffy
   └── positioned glyphs   → Vello CPU
```

The pieces exist — Parley produces shaped extents and positioned runs, Vello CPU
consumes positioned runs, and `instar-paint`'s `GlyphRun` is already that shape.
What is missing is a font context, and introducing one is Phase 2 work.

**Trigger:** the first requirement that needs more from text than a fixed-pitch
rectangle — a proportional face, wrapping inside a widget, mixed sizes, bidi, or
a caret.

### Smaller ones

- **One window, one guest.** `PresentationState` is per-runtime, not per-window.
- **Debug guests are 4 MB, release guests are 116 KB.** Build scripts produce
  debug components; nothing depends on the size, but the 36× gap will surprise
  someone.
- **No quota enforcement.** A guest spinning on CPU without yielding is out of
  scope for Phase 1 — deliberately, and recorded in `engine.rs`.
- **Windows overhead is unmeasured**, because measuring it needs a dependency in
  the tool that measures.

---

## 7. What Phase 1 does *not* claim

- That the protocol is stable. It is explicitly experimental, and the version
  byte exists so a change is a refusal rather than a misparse.
- That the crate layout is final.
- That anything has been run under sustained real-world load. 100 cycles catches
  per-interaction leaks; the 1,000-generation soak catches lifecycle churn.
  Neither is an overnight soak.
- That the compositor does the right thing with a presented buffer — see the
  manual gate above.
- Anything about performance under contention, multiple guests, or a window
  being resized continuously.

---

## The short version

The premise holds. A guest suspends at zero cost, is woken by the host, round
trips through a two-thread bridge in about 0.2 ms, describes an interface it
owns no geometry in, and gets it rendered in a real window — and when it dies,
the host says so in a surface the guest cannot influence and cannot flood.

The architecture that came out of it is mostly a set of refusals: a guest links
one tiny crate, cannot express a rectangle, cannot author the screen that
reports its own death, and cannot be left waiting forever. Each of those was
cheaper to enforce in a type than to remember.
