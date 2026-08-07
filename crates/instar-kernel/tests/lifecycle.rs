//! WP4 guest lifecycle tests.
//!
//! These assert the rule Gate 0 forced (docs/PHASE-1.md, "Guest lifetime
//! boundary"): a guest's lifetime boundary is its `Store` plus component
//! instance, per-operation cancellation is a separate protocol-level
//! mechanism, and a superseded generation can never affect its successor.

use std::time::Duration;

use instar_kernel::runtime::{GenerationHandle, Runtime, RuntimeGeneration, guest_component_bytes};

/// Drives `fut` for `window`, expecting the guest to still be running after.
macro_rules! run_for {
    ($fut:expr, $window:expr) => {
        tokio::select! {
            biased;
            result = &mut $fut => panic!("guest exited before the test finished: {result:?}"),
            _ = tokio::time::sleep($window) => {}
        }
    };
}

async fn runtime() -> Runtime {
    let bytes = guest_component_bytes().expect("kernel guest fixture built by build.rs");
    Runtime::new(&bytes).expect("runtime builds with the kernel world linked")
}

/// Brings a generation up to the point where its guest is parked in
/// `next-event` with its initial commit done.
async fn started(runtime: &mut Runtime) -> (RuntimeGeneration, GenerationHandle) {
    let generation = runtime
        .new_generation()
        .await
        .expect("generation instantiates");
    let handle = generation.handle();
    (generation, handle)
}

/// Every generation gets its own `Store` and a fresh, higher id.
#[tokio::test]
async fn generations_are_sequential_and_independent() {
    let mut runtime = runtime().await;
    let kernel = runtime.kernel();

    let (first, _first_handle) = started(&mut runtime).await;
    assert_eq!(first.id().0, 1, "first generation should be gen1");
    assert_eq!(kernel.current_generation(), first.id());

    runtime.destroy_generation(first);

    let (second, _second_handle) = started(&mut runtime).await;
    assert_eq!(second.id().0, 2, "ids increase and are never reused");
    assert_eq!(kernel.current_generation(), second.id());

    runtime.destroy_generation(second);
}

/// The ordinary path: events in, commits out, clean shutdown.
#[tokio::test]
async fn generation_runs_and_shuts_down_cleanly() {
    let mut runtime = runtime().await;
    let kernel = runtime.kernel();
    let (mut generation, handle) = started(&mut runtime).await;

    let result = {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(200));

        handle.send("echo:hello").expect("guest accepts events");
        run_for!(run, Duration::from_millis(200));

        handle.shutdown().expect("guest accepts events");
        tokio::time::timeout(Duration::from_secs(5), &mut run)
            .await
            .expect("guest returns promptly after shutdown")
    };

    assert_eq!(
        result.expect("host call should not trap"),
        Ok(()),
        "guest should return Ok after shutdown"
    );
    assert_eq!(kernel.commits_utf8(), vec!["gen1:ready", "gen1:hello"]);
    assert_eq!(kernel.stale_commits_rejected(), 0);

    runtime.destroy_generation(generation);
}

/// An operation runs to completion and its result reaches the guest.
#[tokio::test]
async fn operations_complete_and_report_back() {
    let mut runtime = runtime().await;
    let kernel = runtime.kernel();
    let (mut generation, handle) = started(&mut runtime).await;

    {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(200));

        handle.send("op:delay:50").expect("guest accepts events");
        run_for!(run, Duration::from_millis(500));

        handle.send("op:echo:hi").expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));
    }

    assert_eq!(
        kernel.commits_utf8(),
        vec!["gen1:ready", "gen1:op-ok:delayed:50", "gen1:op-ok:hi"],
    );
    assert_eq!(
        kernel.live_operations(),
        0,
        "completed operations must not stay in the registry"
    );

    runtime.destroy_generation(generation);
}

/// **The WP4 headline test.** Per-operation cancellation cancels exactly one
/// operation and leaves the guest task alive and working.
///
/// This is the mechanism that replaces "drop the guest future", which
/// docs/PHASE-1.md reserves for destroying an entire generation.
#[tokio::test]
async fn cancelling_an_operation_leaves_the_guest_alive() {
    let mut runtime = runtime().await;
    let kernel = runtime.kernel();
    let (mut generation, handle) = started(&mut runtime).await;

    {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(200));

        // A 10s operation, cancelled almost immediately. If cancellation did
        // not work, this test would time out rather than fail an assertion.
        handle
            .send("op-cancel:10000")
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(500));

        assert_eq!(
            kernel.commits_utf8(),
            vec![
                "gen1:ready",
                "gen1:op-cancel:requested=true,outcome=cancelled"
            ],
            "the operation should report as cancelled, well before its 10s elapsed"
        );

        // The whole point: the guest task survived its operation being
        // cancelled and still responds.
        handle
            .send("echo:still-alive")
            .expect("guest is still accepting events");
        run_for!(run, Duration::from_millis(300));
    }

    assert_eq!(
        kernel.commits_utf8().last().map(String::as_str),
        Some("gen1:still-alive"),
        "guest must keep running after one of its operations is cancelled"
    );
    assert_eq!(
        kernel.live_operations(),
        0,
        "a cancelled operation must not stay in the registry"
    );

    runtime.destroy_generation(generation);
}

/// A guest asking about an operation it does not own gets `unknown`, not
/// someone else's result.
#[tokio::test]
async fn unknown_operations_are_rejected() {
    let mut runtime = runtime().await;
    let kernel = runtime.kernel();
    let (mut generation, handle) = started(&mut runtime).await;

    {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(200));
        handle.send("op-unknown").expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));
    }

    assert_eq!(
        kernel.commits_utf8().last().map(String::as_str),
        Some("gen1:op-error:unknown"),
    );

    runtime.destroy_generation(generation);
}

/// Tearing down a generation cancels the operations it owned, rather than
/// letting them outlive it and complete into a successor.
#[tokio::test]
async fn teardown_cancels_the_generations_operations() {
    let mut runtime = runtime().await;
    let kernel = runtime.kernel();
    let (mut generation, handle) = started(&mut runtime).await;

    {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(200));

        // Long operation, still in flight when we tear the generation down.
        handle.send("op:delay:10000").expect("guest accepts events");
        run_for!(run, Duration::from_millis(200));

        assert_eq!(
            kernel.live_operations(),
            1,
            "the operation should be in flight before teardown"
        );
    }

    let cancelled = runtime.destroy_generation(generation);
    assert_eq!(
        cancelled, 1,
        "teardown should cancel the generation's operation"
    );
    assert_eq!(
        kernel.live_operations(),
        0,
        "no operation may survive its generation"
    );
}

/// A superseded generation cannot commit into its successor's state.
///
/// This is stale-completion rejection at the point that matters: the check is
/// enforced by the host on every commit, not assumed because teardown is
/// "supposed to" have stopped everything.
#[tokio::test]
async fn stale_generations_cannot_commit() {
    let mut runtime = runtime().await;
    let kernel = runtime.kernel();

    // Bring up gen1 and park it, holding it alive deliberately.
    let (mut first, first_handle) = started(&mut runtime).await;
    let second = {
        let mut first_run = std::pin::pin!(first.run());
        run_for!(first_run, Duration::from_millis(200));
        assert_eq!(kernel.commits_utf8(), vec!["gen1:ready"]);

        // Create gen2 *without* destroying gen1. This is precisely the state
        // the lifetime rule forbids, constructed on purpose so the host's own
        // defence can be observed rather than assumed.
        let (second, _second_handle) = started(&mut runtime).await;
        assert_eq!(kernel.current_generation(), second.id());

        // gen1 is now stale. Anything it tries to commit must be refused --
        // and the refusal is an error the guest sees, so gen1's guest fails
        // out of its own event loop rather than continuing to run blind. That
        // is the desired shape: a superseded generation stops on its own
        // rather than lingering as a second live guest.
        first_handle
            .send("echo:from-stale-gen")
            .expect("gen1 still queues");

        let stale_outcome = tokio::time::timeout(Duration::from_secs(5), &mut first_run)
            .await
            .expect("a stale generation should fail fast, not hang");

        let guest_error = stale_outcome
            .expect("a rejected commit is a guest-visible error, not a host trap")
            .expect_err("gen1's guest should have failed after its commit was refused");
        assert!(
            guest_error.contains("StaleGeneration"),
            "gen1 should fail specifically because its generation was superseded, got: {guest_error}"
        );

        assert_eq!(
            kernel.commits_utf8(),
            vec!["gen1:ready"],
            "a stale generation's commit must not appear in the log"
        );
        assert_eq!(
            kernel.stale_commits_rejected(),
            1,
            "the stale commit should have been explicitly rejected"
        );

        second
    };

    runtime.destroy_generation(first);
    runtime.destroy_generation(second);
}

/// A trapping guest fails its generation without taking the host with it, and
/// the next generation starts clean.
#[tokio::test]
async fn a_trapping_guest_is_contained_and_replaced() {
    let mut runtime = runtime().await;
    let kernel = runtime.kernel();

    let (mut doomed, doomed_handle) = started(&mut runtime).await;
    let trap_result = {
        let mut run = std::pin::pin!(doomed.run());
        run_for!(run, Duration::from_millis(200));

        doomed_handle.send("trap").expect("guest accepts events");
        tokio::time::timeout(Duration::from_secs(5), &mut run)
            .await
            .expect("a trapping guest must not hang the host")
    };

    // A wasm trap surfaces as a host-side error, not a panic and not a hang.
    assert!(
        trap_result.is_err(),
        "a guest trap should surface as an error from the host call, got {trap_result:?}"
    );
    runtime.destroy_generation(doomed);

    // The host is still usable, and the replacement generation is unaffected.
    let (mut replacement, replacement_handle) = started(&mut runtime).await;
    assert_eq!(replacement.id().0, 2);
    {
        let mut run = std::pin::pin!(replacement.run());
        run_for!(run, Duration::from_millis(200));
        replacement_handle
            .send("echo:recovered")
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));
    }

    assert_eq!(
        kernel.commits_utf8(),
        vec!["gen1:ready", "gen2:ready", "gen2:recovered"],
        "the replacement generation should run normally after a trap"
    );
    assert_eq!(kernel.stale_commits_rejected(), 0);

    runtime.destroy_generation(replacement);
}
