//! HARDEN-3 kernel policy tests.
//!
//! The end-to-end table for all five guests and the bridge behavior gates
//! live in `crates/instar-host/tests/resource_policy.rs`. This file pins the
//! kernel-side contract: every generation installs Instar's one policy, a
//! ceiling violation is an ordinary Wasmtime error instead of a host abort,
//! and the engine actually carries epoch interruption for shutdown.

use std::time::Duration;

use instar_kernel::engine::configured_engine;
use instar_kernel::resource::ResourcePolicy;
use instar_kernel::runtime::{Runtime, guest_component_bytes};

#[tokio::test]
async fn generations_install_the_policy_and_record_startup_evidence() {
    let bytes = guest_component_bytes().expect("kernel-guest fixture built by build.rs");
    let mut runtime = Runtime::new(&bytes).expect("runtime builds");
    let mut generation = runtime
        .new_generation()
        .await
        .expect("generation instantiates under the default policy");
    let metrics = generation.metrics();

    {
        let mut run = std::pin::pin!(generation.run());
        tokio::select! {
            biased;
            result = &mut run => panic!("guest exited during startup: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
        }
    }

    let snapshot = metrics.snapshot();
    let policy = runtime.policy();
    assert!(
        snapshot.memories_required > 0 && snapshot.memories_required <= policy.memories as u64,
        "measured {} memories against a {} ceiling",
        snapshot.memories_required,
        policy.memories
    );
    assert!(
        snapshot.tables_required > 0 && snapshot.tables_required <= policy.tables as u64,
        "measured {} tables against a {} ceiling",
        snapshot.tables_required,
        policy.tables
    );
    assert!(
        snapshot.peak_memory_bytes > 0 && snapshot.peak_memory_bytes <= policy.memory_bytes as u64,
        "measured {} peak memory bytes against a {} ceiling",
        snapshot.peak_memory_bytes,
        policy.memory_bytes
    );

    runtime.destroy_generation(generation);
}
/// A Store whose instance ceiling is below the component's demand must fail
/// instantiation with a Wasmtime error, never abort the host process.
#[tokio::test]
async fn an_instance_ceiling_below_demand_is_a_contained_instantiation_error() {
    let bytes = guest_component_bytes().expect("kernel-guest fixture built by build.rs");
    let policy = ResourcePolicy::instar_default().with_instances(0);
    let mut runtime = Runtime::new_with_policy(&bytes, policy).expect("runtime builds");

    let error = match runtime.new_generation().await {
        Ok(_) => panic!("zero instances must be refused, not silently tolerated"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("resource limit exceeded"),
        "the failure should be Wasmtime's resource-limit error, got: {error:#}"
    );
}

/// The engine carries epoch instrumentation; Instar simply never ticks it
/// during idle operation. The runtime thread's shutdown path is the only
/// increment.
#[test]
fn the_engine_has_epoch_interruption_enabled() {
    let engine = configured_engine().expect("engine config is valid");
    assert!(
        engine.get_epoch_interruption(),
        "epoch interruption must be compiled in for out-of-band shutdown"
    );
}

/// A generation created after the engine epoch has moved still gets a fresh
/// deadline: the deadline is relative to the current epoch, so one shutdown
/// signal cannot permanently disable a successor's store.
#[tokio::test]
async fn a_generation_after_an_epoch_increment_still_runs() {
    let bytes = guest_component_bytes().expect("kernel-guest fixture built by build.rs");
    let mut runtime = Runtime::new(&bytes).expect("runtime builds");
    let engine = runtime.engine();

    let mut first = runtime.new_generation().await.expect("first generation");
    {
        let mut run = std::pin::pin!(first.run());
        tokio::select! {
            biased;
            result = &mut run => panic!("first guest exited early: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
    engine.increment_epoch();
    runtime.destroy_generation(first);

    // A fresh store after the increment must not immediately trap: its own
    // deadline is relative to the new current epoch.
    let mut second = runtime.new_generation().await.expect("second generation");
    {
        let mut run = std::pin::pin!(second.run());
        tokio::select! {
            biased;
            result = &mut run => panic!("second guest exited early: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
    runtime.destroy_generation(second);
}
