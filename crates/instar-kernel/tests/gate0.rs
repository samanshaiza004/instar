//! Gate 0 (WP3): does Wasmtime's Component Model async actually give Instar
//! the properties its whole architecture is premised on?
//!
//! Each test here is a *gate*, not a unit test: it is meant to fail loudly and
//! specifically if the underlying runtime cannot do the thing, so that a
//! "no-go" is unambiguous and attributable. Read `docs/GATE-0.md` for the
//! recorded outcome.
//!
//! The gates deliberately assert on observables that would still hold if the
//! implementation were rewritten -- completion order, host-import call counts,
//! poll counts, runtime bookkeeping emptiness -- rather than on internals.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use instar_kernel::spike::{Spike, SpikeHandle, guest_component_bytes};

/// Wraps a future and counts how many times it is polled.
///
/// This is the instrument behind the idle gate. A runtime that busy-polls, or
/// that wakes the guest on a timer, shows up here as a poll count that climbs
/// while the harness is deliberately doing nothing.
struct CountingFuture<F> {
    inner: F,
    polls: Arc<AtomicU64>,
}

impl<F: Future> Future for CountingFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: standard pin projection to a single structural field; `polls`
        // is not structurally pinned and `inner` is never moved.
        let (inner, polls) = unsafe {
            let this = self.get_unchecked_mut();
            (Pin::new_unchecked(&mut this.inner), &this.polls)
        };
        polls.fetch_add(1, Ordering::SeqCst);
        inner.poll(cx)
    }
}

async fn spike() -> (Spike, SpikeHandle) {
    let bytes = guest_component_bytes().expect("guest fixture built by build.rs");
    Spike::new(&bytes)
        .await
        .expect("spike instantiates with WASI and the kernel-spike imports linked")
}

/// Drives `fut` for `window`, expecting it *not* to finish (the guest should
/// still be parked in its event loop). Panics if the guest returns early.
macro_rules! run_for {
    ($fut:expr, $window:expr) => {
        tokio::select! {
            // `biased` matters for the idle gate: without it `select!` polls
            // its branches in a random order, so the number of times the guest
            // future gets polled per window varies run to run and there is no
            // stable baseline to compare against. Biased ordering makes the
            // harness's own polling cost deterministic.
            biased;
            result = &mut $fut => panic!("guest exited before the gate finished: {result:?}"),
            _ = tokio::time::sleep($window) => {}
        }
    };
}

/// Gate: a guest parked in an async host import can be woken by the host, and
/// resumes exactly where it left off.
///
/// This is the baseline capability everything else in Instar's design assumes.
/// If this fails, the event-driven architecture is not viable on this runtime.
#[tokio::test]
async fn gate0_suspend_and_wake() {
    let (mut spike, handle) = spike().await;
    let mut run = std::pin::pin!(spike.run());

    // Let the guest reach its first `next-event` suspension.
    run_for!(run, Duration::from_millis(200));
    assert_eq!(
        handle.commits_utf8(),
        vec!["ready"],
        "guest should have committed once, then parked in next-event"
    );

    handle.send("echo:first").expect("guest accepts events");
    run_for!(run, Duration::from_millis(200));

    handle.send("echo:second").expect("guest accepts events");
    run_for!(run, Duration::from_millis(200));

    assert_eq!(
        handle.commits_utf8(),
        vec!["ready", "first", "second"],
        "each host event should wake the guest exactly once, in order"
    );
    assert_eq!(
        handle.metrics().next_event_calls(),
        3,
        "guest should have re-entered next-event once per delivered event, \
         plus once more where it is currently parked"
    );
}

/// Gate: while the guest is parked, nothing polls, nothing ticks, nothing
/// wakes.
///
/// This is the hard idle gate. The former Youth runtime failed the spirit of
/// this by construction -- it ran a 10ms epoch ticker thread -- which is why
/// `instar-kernel` deleted that thread outright (see `engine.rs`).
#[tokio::test]
async fn gate0_no_polling_while_idle() {
    let (mut spike, handle) = spike().await;
    let polls = Arc::new(AtomicU64::new(0));
    let mut run = std::pin::pin!(CountingFuture {
        inner: spike.run(),
        polls: Arc::clone(&polls),
    });

    // Get the guest parked in `next-event`.
    run_for!(run, Duration::from_millis(200));
    let polls_at_idle = polls.load(Ordering::SeqCst);
    let calls_at_idle = handle.metrics().next_event_calls();

    // Non-vacuity check: startup (instantiate, commit, reach next-event) must
    // have polled the guest a few times. Without this, a counter that silently
    // stopped working would make every assertion below trivially true.
    assert!(
        polls_at_idle >= 2,
        "expected the guest to be polled while starting up, saw {polls_at_idle} -- \
         the poll counter is not measuring anything and the idle assertions below \
         would pass vacuously"
    );

    // Idle for a short window, then for a window 4x longer. Comparing the two
    // is what makes this gate robust: `tokio::select!` itself costs a small,
    // fixed number of polls per use (one on entry, one when the sleep branch
    // wakes it), and that overhead is the same for both windows. Genuine
    // polling, by contrast, scales with elapsed time -- so the *difference*
    // between the two windows is the real signal, not the absolute count.
    let short_window = Duration::from_millis(400);
    run_for!(run, short_window);
    let polls_after_short = polls.load(Ordering::SeqCst);
    let short_cost = polls_after_short - polls_at_idle;

    let long_window = short_window * 4;
    run_for!(run, long_window);
    let polls_after_long = polls.load(Ordering::SeqCst);
    let long_cost = polls_after_long - polls_after_short;

    assert_eq!(
        short_cost,
        long_cost,
        "idling {long_window:?} cost {long_cost} polls but idling {short_window:?} cost \
         {short_cost}: poll count scales with idle time, which means something is waking \
         the guest periodically. A 10ms ticker (what Youth ran) would show ~{} extra polls \
         over the long window.",
        long_window.as_millis() / 10
    );
    assert!(
        long_cost <= 2,
        "idling cost {long_cost} polls; only the harness's own `select!` entry/wake \
         (at most 2) should be polling an idle guest"
    );
    assert_eq!(
        handle.metrics().next_event_calls(),
        calls_at_idle,
        "guest must not re-enter next-event while idle -- that would mean the \
         import returned spuriously rather than suspending"
    );
    assert_eq!(
        handle.commits_utf8(),
        vec!["ready"],
        "an idle guest must not produce work"
    );

    // And it is still live: idleness must not be indistinguishable from death.
    handle.send("echo:awake").expect("guest accepts events");
    run_for!(run, Duration::from_millis(200));
    assert_eq!(
        handle.commits_utf8(),
        vec!["ready", "awake"],
        "guest must still wake normally after a long idle period"
    );
}

/// Gate: two async host imports in flight in the same guest task make
/// independent progress -- a slow one does not head-of-line block a fast one.
///
/// The guest issues `delay(long)` and `delay(short)` together and reports them
/// in *completion* order. If the Component Model serialized concurrent imports,
/// the long one would be reported first.
#[tokio::test]
async fn gate0_concurrent_imports_do_not_head_of_line_block() {
    let (mut spike, handle) = spike().await;
    let mut run = std::pin::pin!(spike.run());

    run_for!(run, Duration::from_millis(200));

    // Long delay issued first, deliberately.
    handle.send("join:400,40").expect("guest accepts events");
    run_for!(run, Duration::from_millis(900));

    assert_eq!(
        handle.commits_utf8(),
        vec!["ready", "joined:40,400"],
        "the 40ms delay must complete before the 400ms one that was issued first; \
         `joined:400,40` would mean concurrent async imports are serialized"
    );
    assert_eq!(
        handle.metrics().delay_calls(),
        2,
        "both delays should have actually entered the host import"
    );
}

/// Gate: a guest that is told to shut down returns from its export cleanly,
/// rather than trapping or hanging.
#[tokio::test]
async fn gate0_clean_shutdown() {
    let (mut spike, handle) = spike().await;

    let result = {
        let mut run = std::pin::pin!(spike.run());
        run_for!(run, Duration::from_millis(200));
        handle.shutdown().expect("guest accepts events");

        tokio::time::timeout(Duration::from_secs(5), &mut run)
            .await
            .expect("guest should return promptly after shutdown, not hang")
    };

    let guest_result = result.expect("host-side call to `run` should not trap");
    assert_eq!(
        guest_result,
        Ok(()),
        "guest should return Ok from `run` after a shutdown event"
    );

    // Nothing should be left in flight after a clean exit.
    spike.assert_concurrent_state_empty();
}

/// Gate: dropping the future that drives a guest with async work in flight
/// stops that work immediately and permanently.
///
/// Cancellation is host-driven by *dropping the Rust future*, not by epoch
/// interruption -- this is exactly why `instar-kernel` did not port Youth's
/// ticker thread. This gate is what makes that claim testable rather than
/// asserted. `wit-bindgen` 0.60.0 was pinned partly for a named fix in this
/// area (see docs/TOOLCHAIN.md).
///
/// Note what this gate does *not* claim: that the runtime immediately reclaims
/// the abandoned task's bookkeeping. It does not, and
/// [`gate0_abandoned_tasks_retain_state_until_store_is_dropped`] pins that
/// behaviour down deliberately.
#[tokio::test]
async fn gate0_cancellation_stops_in_flight_work() {
    let (mut spike, handle) = spike().await;

    {
        let mut run = std::pin::pin!(spike.run());
        run_for!(run, Duration::from_millis(200));

        // Put a long delay in flight, then let it get genuinely started.
        handle.send("delay:10000").expect("guest accepts events");
        run_for!(run, Duration::from_millis(200));
        assert_eq!(
            handle.metrics().delay_calls(),
            1,
            "the delay should be in flight before we cancel"
        );
        assert_eq!(
            handle.commits_utf8(),
            vec!["ready"],
            "the delay must not have completed yet -- otherwise this gate is \
             not testing cancellation of in-flight work"
        );

        // Dropping `run` here is the cancellation.
    }

    // Give the runtime every chance to let the cancelled work resurface: turn
    // the event loop, and wait well past the point where a still-live 10s
    // delay would have had its timer fire relative to the work already done.
    spike
        .drive_event_loop()
        .await
        .expect("driving the event loop after cancellation should not error");
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        handle.commits_utf8(),
        vec!["ready"],
        "cancelled work must never commit -- this is the safety property the \
         kernel actually depends on"
    );
    assert_eq!(
        handle.metrics().commit_calls(),
        1,
        "no host import should run on behalf of a cancelled task"
    );
}

/// Gate (documenting a real limitation, not a success): abandoning a *started*
/// guest task retains that task's runtime bookkeeping for the lifetime of the
/// `Store`, and abandoning repeatedly on one store accumulates it.
///
/// This is the one Gate 0 result that constrains Instar's design rather than
/// enabling it, so it is pinned by a test: if a future Wasmtime version starts
/// reclaiming this, this test fails loudly and the constraint can be lifted
/// with evidence.
///
/// Why it happens (best reading of the runtime, not a claim from its docs): a
/// suspended guest task owns a wasm stack that nothing can force-unwind, so
/// dropping the host future that was driving it can stop *scheduling* it but
/// cannot dismantle it. Wasmtime's own `assert_concurrent_state_empty` is
/// documented for catching tasks that leak "despite having completed" -- an
/// abandoned task never completes, so it is outside what that check considers
/// a leak. Driving the event loop afterwards does not reclaim it either
/// (verified).
///
/// Consequence for Instar: **cancelling a guest means tearing down its
/// instance**, not abandoning its task and starting another on the same store.
/// Per-operation cancellation, when Instar needs it, has to be modelled inside
/// the protocol (the guest cancels its own subtask) rather than by dropping
/// host futures out from under a live guest.
#[tokio::test]
async fn gate0_abandoned_tasks_retain_state_until_store_is_dropped() {
    let (mut spike, handle) = spike().await;
    assert_eq!(
        spike.concurrent_state_table_size(),
        0,
        "a freshly instantiated component should hold no concurrent state"
    );

    let mut sizes = Vec::new();
    for _ in 0..3 {
        {
            let mut run = std::pin::pin!(spike.run());
            run_for!(run, Duration::from_millis(150));
            handle.send("delay:10000").expect("guest accepts events");
            run_for!(run, Duration::from_millis(150));
        }
        spike
            .drive_event_loop()
            .await
            .expect("event loop should still be usable after an abandonment");
        sizes.push(spike.concurrent_state_table_size());
    }

    assert!(
        sizes[0] > 0,
        "expected an abandoned started task to retain state; it retained none, \
         which would mean this limitation no longer applies -- delete this test \
         and tighten gate0_cancellation_stops_in_flight_work instead"
    );
    let per_abandonment = sizes[0];
    assert_eq!(
        sizes,
        vec![per_abandonment, per_abandonment * 2, per_abandonment * 3],
        "retained state should grow by a fixed amount per abandoned task \
         ({per_abandonment} entries each); anything else means the retention \
         behaviour changed and docs/GATE-0.md needs re-verifying"
    );
}
