//! HARDEN-3 acceptance: one measured resource policy, epoch-interruptible
//! shutdown, and containment of policy violations.
//!
//! Two kinds of test live here:
//!
//! 1. The measurement table. Every existing guest -- counter, gallery,
//!    hostile, kernel-guest, kernel-spike-guest -- is instantiated and run
//!    through its real startup, and its resource evidence is recorded. The
//!    memory/table counts come from Wasmtime's `Component::resources_required`;
//!    accepted runtime peaks come from the store limiter. Core-instance demand
//!    is measured by probing the smallest `instances` limit under which the
//!    component instantiates, because Wasmtime 47 exposes no per-instance
//!    creation callback or public enumerator.
//! 2. The behavior gates: non-yielding Wasm is interrupted by shutdown, a
//!    memory growth past the policy is a contained guest trap, and shutdown
//!    stays bounded for a guest suspended inside a host operation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use instar_host::bridge::{HostBridge, Wake};
use instar_host::{HostEffect, HostWindow};
use instar_kernel::resource::{ResourceMetricsSnapshot, ResourcePolicy};
use instar_kernel::runtime::{Runtime, guest_component_bytes};
use instar_kernel::spike::{Spike, guest_component_bytes as spike_guest_component_bytes};
use instar_ui::{NodeKey, NodeKind};
use instar_window::{
    LogicalPoint, LogicalSize, PhysicalSize, PointerButton, PointerState, RawPointerEvent,
    WindowId, WindowMetricsChanged, WindowOutput,
};

const WINDOW: WindowId = WindowId::from_raw(1);

/// The hostile fixture's HARDEN-3 node keys, learned from the wire like every
/// other key the host knows.
const SPIN: NodeKey = NodeKey::first(13);
const GROW: NodeKey = NodeKey::first(14);
const SLEEP: NodeKey = NodeKey::first(15);

/// The outer bound for anything that should not need a second at all.
const PATIENCE: Duration = Duration::from_secs(5);

/// How long a shutdown that must interrupt executing Wasm or wait out a
/// suspended host operation is allowed to take. The runtime's own grace for a
/// suspended guest is one second; this is the externally observed firm bound.
const FIRM_SHUTDOWN: Duration = Duration::from_secs(3);

fn metrics(scale: f64) -> WindowMetricsChanged {
    WindowMetricsChanged {
        window_id: WINDOW,
        logical_size: LogicalSize {
            width: 400.0,
            height: 400.0,
        },
        physical_size: PhysicalSize {
            width: (400.0 * scale) as u32,
            height: (400.0 * scale) as u32,
        },
        scale_factor: scale,
    }
}
fn pointer(state: PointerState, x: f64, y: f64) -> WindowOutput {
    WindowOutput::Pointer(RawPointerEvent {
        window_id: WINDOW,
        logical_pos: LogicalPoint::new(x, y),
        button: PointerButton::Primary,
        state,
    })
}

fn centre(bridge: &HostBridge, key: NodeKey) -> (f64, f64) {
    let rect = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::layout)
        .and_then(|layout| layout.get(key))
        .unwrap_or_else(|| panic!("{key} should have host-computed geometry"));
    (
        f64::from(rect.x + rect.width / 2),
        f64::from(rect.y + rect.height / 2),
    )
}

fn label(bridge: &HostBridge) -> String {
    match bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::tree)
        .and_then(|tree| tree.find(NodeKey::first(2)))
        .map(|node| &node.kind)
    {
        Some(NodeKind::Text { text }) => text.clone(),
        other => panic!("the fixture's label should be a text node, found {other:?}"),
    }
}

#[derive(Default)]
struct Wakes(AtomicU64);

fn spawn() -> (HostBridge, Arc<Wakes>) {
    let component = std::fs::read(env!("HOSTILE_WASM")).expect("the hostile guest is built");
    let wakes = Arc::new(Wakes::default());
    let counter = Arc::clone(&wakes);
    let wake: Wake = Arc::new(move || {
        counter.0.fetch_add(1, Ordering::SeqCst);
    });
    let bridge = HostBridge::spawn(component, WINDOW, wake).expect("the guest starts");
    (bridge, wakes)
}

fn ready() -> (HostBridge, Arc<Wakes>) {
    let (mut bridge, wakes) = spawn();
    bridge.on_window_event(WindowOutput::MetricsChanged(metrics(1.0)));
    await_commit(&mut bridge).expect("the guest commits its opening interface");
    (bridge, wakes)
}

fn click(bridge: &mut HostBridge, key: NodeKey) {
    let (x, y) = centre(bridge, key);
    bridge.on_window_event(pointer(PointerState::Pressed, x, y));
    bridge.on_window_event(pointer(PointerState::Released, x, y));
}

fn await_commit(bridge: &mut HostBridge) -> Option<Duration> {
    let target = bridge.commit_sequence() + 1;
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        bridge.wait(Duration::from_millis(50));
        if bridge.commit_sequence() >= target {
            return Some(started.elapsed());
        }
    }
    None
}

fn await_guest_gone(bridge: &mut HostBridge) -> Option<String> {
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        for effect in bridge.wait(Duration::from_millis(50)) {
            if let HostEffect::GuestGone { error, .. } = effect {
                return error;
            }
        }
    }
    panic!("the guest never reported that it was gone");
}

// --- Measurement table ------------------------------------------------------

/// One row of the five-guest measurement table.
#[derive(Debug, Clone, Copy)]
struct GuestMeasurement {
    name: &'static str,
    instances: usize,
    metrics: ResourceMetricsSnapshot,
}

/// Runs one kernel-world component through its real startup: instantiate,
/// enter `run`, and let it reach its first suspension.
async fn measure_runtime_guest(name: &'static str, bytes: &[u8]) -> GuestMeasurement {
    let mut runtime = Runtime::new(bytes).expect("runtime builds");
    let mut generation = runtime
        .new_generation()
        .await
        .expect("generation instantiates under the default policy");
    let metrics = generation.metrics();

    {
        let mut run = std::pin::pin!(generation.run());
        tokio::select! {
            biased;
            result = &mut run => {
                panic!("{name} exited during startup instead of suspending: {result:?}")
            }
            _ = tokio::time::sleep(Duration::from_millis(400)) => {}
        }
    }
    runtime.destroy_generation(generation);

    GuestMeasurement {
        name,
        instances: probe_runtime_instances(bytes).await,
        metrics: metrics.snapshot(),
    }
}

/// The smallest `instances` ceiling under which a fresh Store instantiates the
/// component. That ceiling equals the component's core-instance demand during
/// instantiate/run startup.
async fn probe_runtime_instances(bytes: &[u8]) -> usize {
    let default = ResourcePolicy::instar_default();
    for instances in 1..=default.instances {
        let policy = default.with_instances(instances);
        let mut runtime = Runtime::new_with_policy(bytes, policy).expect("runtime builds");
        if runtime.new_generation().await.is_ok() {
            return instances;
        }
    }
    panic!("component does not instantiate even at the default instance ceiling")
}

async fn measure_spike_guest() -> GuestMeasurement {
    let bytes = spike_guest_component_bytes().expect("kernel-spike fixture built");
    let (mut spike, handle) = Spike::new(&bytes)
        .await
        .expect("the spike guest instantiates");
    let resource_metrics = handle.resource_metrics();

    {
        let mut run = std::pin::pin!(spike.run());
        tokio::select! {
            biased;
            result = &mut run => panic!("kernel-spike exited during startup: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(400)) => {}
        }
    }

    let default = ResourcePolicy::instar_default();
    let mut instances = None;
    for candidate in 1..=default.instances {
        let policy = default.with_instances(candidate);
        if Spike::new_with_policy(&bytes, policy).await.is_ok() {
            instances = Some(candidate);
            break;
        }
    }

    GuestMeasurement {
        name: "kernel-spike-guest",
        instances: instances.expect("the spike guest fits the default instance ceiling"),
        metrics: resource_metrics.snapshot(),
    }
}

fn format_table(rows: &[GuestMeasurement]) -> String {
    let mut lines = vec![format!(
        "{:<22} {:>9} {:>9} {:>9} {:>14} {:>14}",
        "guest", "instances", "memories", "tables", "peak bytes", "peak table elems"
    )];
    for row in rows {
        lines.push(format!(
            "{:<22} {:>9} {:>9} {:>9} {:>14} {:>14}",
            row.name,
            row.instances,
            row.metrics.memories_required,
            row.metrics.tables_required,
            row.metrics.peak_memory_bytes,
            row.metrics.peak_table_elements,
        ));
    }
    lines.join("\n")
}

/// The five guests the measurement table is required to cover.
///
/// Named rather than counted: a row silently dropping out of the table would
/// otherwise leave every remaining assertion true, and the test green while
/// measuring one guest fewer than it claims.
const MEASURED_GUESTS: [&str; 5] = [
    "counter",
    "gallery",
    "hostile",
    "kernel-guest",
    "kernel-spike-guest",
];

/// Floors a real startup must clear, well under every measured value but far
/// above what a stalled measurement would report.
///
/// These exist because `measured <= ceiling` alone is satisfied by zero -- and
/// by any stub. A measurement path that stopped producing real numbers would
/// pass the headroom assertions while proving nothing, so each row must also
/// show a startup big enough to be a startup. The smallest measured peak is
/// ~1.06 MiB against a 64 KiB floor, and the smallest measured table is 105
/// elements against a floor of 16.
const FLOOR_PEAK_MEMORY_BYTES: u64 = 64 * 1024;
const FLOOR_PEAK_TABLE_ELEMENTS: u64 = 16;

/// Every existing guest fits under the one Instar policy with substantial
/// headroom, and the table is printed so a policy change is a visible,
/// evidence-backed decision rather than a guess.
#[tokio::test]
async fn all_five_guests_fit_the_default_policy_with_headroom() {
    let counter = std::fs::read(env!("COUNTER_WASM")).expect("counter guest built");
    let gallery = std::fs::read(env!("GALLERY_WASM")).expect("gallery guest built");
    let hostile = std::fs::read(env!("HOSTILE_WASM")).expect("hostile guest built");
    let kernel = guest_component_bytes().expect("kernel-guest fixture built");

    let rows = vec![
        measure_runtime_guest("counter", &counter).await,
        measure_runtime_guest("gallery", &gallery).await,
        measure_runtime_guest("hostile", &hostile).await,
        measure_runtime_guest("kernel-guest", &kernel).await,
        measure_spike_guest().await,
    ];

    println!(
        "\nHARDEN-3 measured resource table\n{}\n",
        format_table(&rows)
    );

    // The table covers the guests it claims to cover: measuring fewer guests
    // must be a failure, not a shorter table.
    let measured: Vec<&str> = rows.iter().map(|row| row.name).collect();
    assert_eq!(
        measured, MEASURED_GUESTS,
        "the measurement table must cover exactly the five guests it is named for"
    );

    let policy = ResourcePolicy::instar_default();
    for row in &rows {
        assert!(
            row.instances > 0 && row.instances <= policy.instances / 4,
            "{} needs {}-core-instance demand, not below a quarter of the {} ceiling",
            row.name,
            row.instances,
            policy.instances
        );
        assert!(
            row.metrics.memories_required > 0
                && row.metrics.memories_required <= policy.memories as u64 / 4,
            "{} requires {} memories, not below a quarter of the {} ceiling",
            row.name,
            row.metrics.memories_required,
            policy.memories
        );
        assert!(
            row.metrics.tables_required > 0
                && row.metrics.tables_required <= policy.tables as u64 / 4,
            "{} requires {} tables, not below a quarter of the {} ceiling",
            row.name,
            row.metrics.tables_required,
            policy.tables
        );
        assert!(
            row.metrics.peak_memory_bytes >= FLOOR_PEAK_MEMORY_BYTES
                && row.metrics.peak_memory_bytes <= (policy.memory_bytes / 8) as u64,
            "{} peaked at {} linear-memory bytes, not between the {} floor of a real \
             startup and an eighth of the {} ceiling",
            row.name,
            row.metrics.peak_memory_bytes,
            FLOOR_PEAK_MEMORY_BYTES,
            policy.memory_bytes
        );
        assert!(
            row.metrics.peak_table_elements >= FLOOR_PEAK_TABLE_ELEMENTS
                && row.metrics.peak_table_elements <= (policy.table_elements / 8) as u64,
            "{} peaked at {} table elements, not between the {} floor of a real startup \
             and an eighth of the {} ceiling",
            row.name,
            row.metrics.peak_table_elements,
            FLOOR_PEAK_TABLE_ELEMENTS,
            policy.table_elements
        );
    }
}

// --- HARDEN-3 behavior gates ------------------------------------------------

/// A hostile guest enters a real non-yielding Wasm loop; shutdown must
/// interrupt it via the engine epoch, terminate the runtime thread, and join
/// within a firm bound. The Store is destroyed before the terminal outcome is
/// stored, so observing the guest-gone outcome after `shutdown()` is also the
/// evidence that teardown ran.
#[test]
fn shutdown_interrupts_a_non_yielding_wasm_loop_within_a_firm_bound() {
    let (mut bridge, _wakes) = ready();

    click(&mut bridge, SPIN);
    await_commit(&mut bridge).expect("the guest reports it is about to spin");
    assert_eq!(label(&bridge), "Spinning...");

    let started = Instant::now();
    bridge.shutdown();
    let elapsed = started.elapsed();
    assert!(
        elapsed < FIRM_SHUTDOWN,
        "shutdown of executing Wasm took {elapsed:?}; the epoch interrupt did not fire"
    );

    let effects = bridge.pump();
    let error = effects
        .iter()
        .find_map(|effect| match effect {
            HostEffect::GuestGone { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("the terminated spin guest must report a terminal outcome");
    let error = error.expect("an interrupted spin loop is a trap, not a clean exit");
    assert!(
        error.contains("interrupt") || error.contains("epoch"),
        "the spin trap should be the epoch interrupt, got: {error}"
    );
    assert_eq!(bridge.live_operations(), 0);
}

/// Memory growth past the policy must be refused as a guest trap, leaving the
/// host process and the bridge machinery intact.
#[test]
fn memory_growth_past_policy_is_a_contained_guest_failure() {
    let (mut bridge, _wakes) = ready();

    click(&mut bridge, GROW);
    let error = await_guest_gone(&mut bridge).expect("the policy violation is a guest failure");
    assert!(
        error.contains("growing memory") || error.contains("memory"),
        "the trap should name the refused memory growth, got: {error}"
    );

    assert!(
        bridge.pump().is_empty(),
        "terminal state must already have been drained by await_guest_gone"
    );
    // The host survived: it can still be shut down cleanly.
    bridge.shutdown();
}

/// A guest suspended inside a host operation cannot be epoch-interrupted (no
/// Wasm is executing), so shutdown relies on the cooperative signal plus the
/// runtime's bounded grace. It must still terminate and join promptly, and the
/// suspended operation must be cancelled with the Store.
#[test]
fn shutdown_with_a_guest_suspended_in_a_host_operation_is_bounded() {
    let (mut bridge, _wakes) = ready();

    click(&mut bridge, SLEEP);
    await_commit(&mut bridge).expect("the guest reports it is about to sleep");
    assert_eq!(label(&bridge), "Sleeping...");
    let started = Instant::now();
    while bridge.live_operations() < 1 && started.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        bridge.live_operations() >= 1,
        "the sleep mode should have a host operation in flight"
    );

    let started = Instant::now();
    bridge.shutdown();
    let elapsed = started.elapsed();
    assert!(
        elapsed < FIRM_SHUTDOWN,
        "shutdown of a guest suspended in a host operation took {elapsed:?}"
    );

    assert!(
        bridge
            .pump()
            .iter()
            .any(|effect| matches!(effect, HostEffect::GuestGone { .. })),
        "the suspended guest must be reported gone after shutdown"
    );
    assert_eq!(
        bridge.live_operations(),
        0,
        "teardown must cancel the host operation it owned"
    );
}

/// The normal hostile lifecycle still works with the policy and epoch
/// instrumentation installed: it is not a mode only exercised by failure
/// gates.
#[test]
fn normal_hostile_lifecycle_still_works() {
    let (mut bridge, _wakes) = ready();

    click(&mut bridge, NodeKey::first(3));
    await_commit(&mut bridge).expect("a click commits");
    assert_eq!(label(&bridge), "Clicked 1 times, 0 bulk");
    assert_eq!(bridge.stats().stale_commits, 0);
    assert_eq!(bridge.stats().rejected_commits, 0);

    bridge.shutdown();
}

/// The measured startup evidence is non-vacuous: a guest that stopped creating
/// resources (or whose measurement silently stopped) would make the headroom
/// assertions trivially true, so the table rows are checked for meaning too.
#[tokio::test]
async fn measurement_rows_are_non_vacuous() {
    let hostile = std::fs::read(env!("HOSTILE_WASM")).expect("hostile guest built");
    let row = measure_runtime_guest("hostile", &hostile).await;
    assert!(
        row.metrics.peak_memory_bytes >= 64 * 1024,
        "startup must allocate memory"
    );
    assert!(row.metrics.memories_required >= 1);
    assert!(row.metrics.tables_required >= 1);
}
