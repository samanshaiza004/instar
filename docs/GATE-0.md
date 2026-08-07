# Gate 0 — headless kernel spike

**Verdict: GO**, with one recorded limitation that constrains how cancellation
gets designed (see [Finding 5](#finding-5-abandoned-guest-tasks-retain-state-until-the-store-is-dropped)).

Decided 2026-08-07 on the WP1 toolchain (Wasmtime 47.0.3, wit-bindgen 0.60.0,
Rust 1.97.1 stable, `wasm32-wasip2`; see [TOOLCHAIN.md](TOOLCHAIN.md)).
Re-verify before trusting this if any of those pins move.

**Confirmed on all three target platforms** — Linux, macOS, and Windows — in
CI (`.github/workflows/gate0.yml`, run 31182995718), not on one developer
machine. This matters more than it might look: the idle gate is a claim about
timer and scheduler behaviour, and a runtime that parks cleanly on one
platform's event loop could plausibly spin on another's. It does not.

## The question

Instar's architecture assumes a guest can sit idle at zero cost and be woken by
the host — no polling loop, no ticker thread, no frame-driven wakeups. That
assumption is either true of Wasmtime's Component Model async support or it
isn't, and everything downstream depends on which. Gate 0 exists to find out
empirically, before any of it is built on.

Concretely, five properties:

1. A guest suspends on an async host import and the host can wake it.
2. While suspended, nothing polls, ticks, or wakes it.
3. Two async imports in flight make independent progress (no head-of-line
   blocking).
4. In-flight work can be cancelled, and cancelled work stays cancelled.
5. A guest told to shut down returns cleanly rather than trapping or hanging.

## How it was tested

- `crates/instar-kernel/wit/world.wit` — spike world. One async import
  (`runtime.next-event`), one sync import (`ui.commit`), one synthetic async
  import (`test-support.delay`) so concurrency can be exercised without real
  I/O. Not a draft of the Instar protocol.
- `crates/instar-kernel/tests/fixtures/kernel-spike-guest/` — a real
  `wasm32-wasip2` guest component. Built from source by `build.rs` on every
  test run, so the gates always run against the current toolchain rather than a
  stale checked-in artifact.
- `crates/instar-kernel/src/spike.rs` — host harness: engine, linker (WASI 0.2
  plus the spike's own imports), host import implementations, metrics.
- `crates/instar-kernel/tests/gate0.rs` — the gates themselves.
- `.github/workflows/gate0.yml` — runs all of it on Linux, macOS, and Windows.
  Gate 0's claims are about a runtime, not about one laptop.

Run them with:

```bash
cargo test -p instar-kernel --test gate0
```

## Findings

### Finding 1: suspend/wake works

`gate0_suspend_and_wake`. The guest commits once, parks in `next-event`, and
resumes exactly where it left off on each host event, in order. Host import
call counts confirm it re-enters `next-event` once per delivered event and no
more.

### Finding 2: an idle guest is genuinely idle

`gate0_no_polling_while_idle`. The gate wraps the future driving the guest in a
poll counter and idles for two windows, the second 4x longer than the first.
**Both windows cost the same number of polls** — poll count does not scale with
idle time. A 10ms ticker (which is what the former Youth runtime ran) would
have shown roughly 160 extra polls over the long window; the measured cost is
at most 2, all of it attributable to the test harness's own `select!`.

Two things make this gate trustworthy rather than vacuous: `select!` is
`biased`, so the harness's polling cost is deterministic instead of randomly
ordered; and the gate asserts up front that the counter observed startup polls,
so a counter that silently broke would fail rather than pass.

This is the finding that justifies deleting Youth's `youth-epoch` ticker thread
outright (see `engine.rs`) rather than porting it disabled.

### Finding 3: concurrent async imports do not head-of-line block

`gate0_concurrent_imports_do_not_head_of_line_block`. The guest issues
`delay(400ms)` and `delay(40ms)` together — long one first, deliberately — and
reports them in completion order. Result: `joined:40,400`. If the Component
Model serialized concurrent imports, this would read `joined:400,40`.

### Finding 4: cancelled work stops and stays stopped

`gate0_cancellation_stops_in_flight_work`. With a 10-second delay in flight,
dropping the host future driving the guest stops it: no commit ever lands, no
further host import runs, and driving the event loop afterwards does not let
the cancelled work resurface. This is the safety property the kernel actually
needs, and it holds.

### Finding 5: abandoned guest tasks retain state until the Store is dropped

`gate0_abandoned_tasks_retain_state_until_store_is_dropped`. **This is the one
result that constrains the design rather than enabling it.**

Dropping the driving future stops a started guest task, but does not reclaim
its runtime bookkeeping: Wasmtime's concurrent-state table retains a fixed
number of entries per abandoned task, and abandoning repeatedly on the same
`Store` accumulates them linearly (measured: 5, 10, 15 entries over three
cycles). Driving the event loop afterwards does not reclaim them either.

This appears to be inherent rather than a bug. A suspended guest task owns a
wasm stack that nothing can force-unwind, so dropping the host future that was
scheduling it can stop it but cannot dismantle it. Wasmtime's own
`assert_concurrent_state_empty` helper is documented for catching tasks that
leak *"despite having completed"* — an abandoned task never completes, so it
falls outside what that check treats as a leak. On the normal path the same
assertion passes: after a clean shutdown, everything is reclaimed
(`gate0_clean_shutdown`).

**Consequence for Instar:** cancelling a guest means tearing down its instance
(dropping the `Store`), not abandoning its task and starting another on the
same store. Per-operation cancellation, when the protocol needs it, has to be
modelled *inside* the protocol — the guest cancelling its own subtask — rather
than by dropping host futures out from under a live guest. The operation
registry and cancellation design in later work packages should be written to
that constraint.

The behaviour is pinned by a test rather than only described here, so if a
future Wasmtime version starts reclaiming this state, the test fails and the
constraint can be lifted with evidence instead of by assumption.

### Finding 6: clean shutdown

`gate0_clean_shutdown`. A shutdown event makes the guest leave its event loop
and return `Ok(())` from `run` promptly, with no trap, and Wasmtime's
concurrent state is completely empty afterwards.

## Notes for whoever picks this up next

- `ui.commit` was kept synchronous on purpose. Bounded in-memory work does not
  need the async machinery, and keeping it sync removed a variable from Gate
  0's result. Nothing here argues for making it async later.
- `test-support.delay` is synthetic and spike-only. It exists so concurrency
  could be tested without real I/O, and does not belong in the real protocol.
- The `spike` module is scaffolding for this decision, not the beginning of the
  kernel API. Expect to delete or rewrite it once the real protocol exists.
- Gate 0 says nothing about performance, memory, or startup cost — those are
  discovery metrics measured separately, and the pre-rewrite baseline for them
  is incomplete (see `baselines/managed-youth-final/runtime-metrics.md`).
