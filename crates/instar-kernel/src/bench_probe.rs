//! Benchmark-only diagnostics for the Phase 3 text-latency benchmark
//! (`benchmarks/text-latency`). Exists entirely behind the `bench-probe`
//! feature, which no production build enables -- see `wit/bench.wit` for why
//! this stays out of `world kernel`, Instar's actual application ABI.
//!
//! `instar:kernel/probe` (`wit/bench.wit`) is intentionally tiny: two
//! functions, no resources. Rather than a second
//! `wasmtime::component::bindgen!` world binding -- which would either
//! duplicate `world kernel`'s generated types under a new namespace, or
//! require `with`-remapping every one of them just to reuse what the
//! existing `Kernel::add_to_linker` call already provides -- `probe` is
//! registered by hand with `Linker::instance`. A `kernel-bench` guest's
//! every other import (`kernel-runtime`, `kernel-ui`, `text-layouts`,
//! `surfaces`, `ops`) is already satisfied by that same call, because
//! `world kernel-bench` is `include kernel; import probe;` -- identical
//! interfaces, one extra one.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use wasmtime::component::Linker;

/// Stage numbers `probe.mark`/`probe.report` callers agree on out of band
/// (WIT does not have shared enum constants across a host/guest boundary).
/// `guests/scratchpad`'s `bench-probe` feature uses the same numbers; see
/// its doc comment for the mirrored copy. `T1`/`T2` name the two guest-only
/// instants this benchmark cannot observe from the host; `T3`/`T4` exist
/// here only as sanity cross-checks against the host's own timing, not as
/// the authoritative reading for those stages.
pub const STAGE_T1_GUEST_RECEIVED: u32 = 1;
pub const STAGE_T2_TRANSACTION_COMPLETE: u32 = 2;
/// Host-recorded only (`record_host_mark`), one entry per `create-layout`
/// call attributed to the current sample -- T3 is the *last* one for a
/// sample, not the first, since one interaction can request several rows.
pub const STAGE_T3_LAYOUT_COMPLETE: u32 = 3;
pub const STAGE_T4_SCENE_ACCEPTED: u32 = 4;

/// `probe.report` counters, same cross-boundary-agreement convention.
pub const COUNTER_DOCUMENT_BYTES_MATERIALIZED: u32 = 1;
pub const COUNTER_VISIBLE_BYTES_PROJECTED: u32 = 2;
/// Host-recorded only, via `record_host_counter`.
pub const COUNTER_EVENT_RX_BYTES: u32 = 3;
pub const COUNTER_LAYOUT_TEXT_BYTES: u32 = 4;
pub const COUNTER_SCENE_BYTES: u32 = 5;
pub const COUNTER_OTHER_GUEST_HOST_BYTES: u32 = 6;

/// The sample most recently marked [`STAGE_T1_GUEST_RECEIVED`]. The
/// benchmark's guest processes one event to completion (all its `mark`/
/// `report` calls) before the next `next-event` call can return a new one
/// (`guests/scratchpad`'s event loop is one sequential task), so a single
/// global cell -- not a per-generation map -- correctly attributes
/// host-side instrumentation (T3, T4, boundary byte counters) to "whichever
/// sample the guest is currently handling," with one caveat: the
/// background-work workload (A) runs a second, unrelated generation
/// concurrently, so its host-side timing is scoped by wall-clock bracketing
/// in the harness instead of sample attribution. See
/// `benchmarks/text-latency`.
static CURRENT_SAMPLE: AtomicU64 = AtomicU64::new(0);

pub fn current_sample() -> u64 {
    CURRENT_SAMPLE.load(Ordering::Acquire)
}

/// The one shared reference instant every T0..T5 timestamp in the
/// benchmark -- host-side (`T0`, `T3`, `T4`, `T5`) and guest-reported
/// (`T1`, `T2`, via `probe.mark`) alike -- subtracts against. Guest calls
/// reach this through `mark`'s host implementation below; the harness reads
/// it directly for its own host-side timestamps, so every timestamp in a
/// run shares one epoch with no calibration step.
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn bench_epoch() -> Instant {
    *EPOCH
}

/// One recorded probe call, in the order the host observed it.
#[derive(Debug, Clone, Copy)]
pub enum ProbeEvent {
    /// `probe.mark(sample, stage)`. `ns` is nanoseconds since [`bench_epoch`].
    Mark { sample: u64, stage: u32, ns: u64 },
    /// `probe.report(sample, counter, value)`.
    Report {
        sample: u64,
        counter: u32,
        value: u64,
    },
}

/// Bounded so a benchmark process that runs far longer than expected
/// degrades to dropping its oldest diagnostics rather than growing without
/// limit. Sized well past any single measured run this benchmark performs;
/// see `benchmarks/text-latency` for actual per-run event counts.
const LOG_CAPACITY: usize = 1 << 20;

static LOG: Mutex<VecDeque<ProbeEvent>> = Mutex::new(VecDeque::new());

fn push(event: ProbeEvent) {
    let mut log = LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if log.len() >= LOG_CAPACITY {
        log.pop_front();
    }
    log.push_back(event);
}

/// Removes and returns every event recorded so far, in call order. The
/// harness calls this between measured workloads so one workload's samples
/// never leak into the next's report.
pub fn drain_probe_log() -> Vec<ProbeEvent> {
    let mut log = LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    log.drain(..).collect()
}

/// Host-side instrumentation helper: records a `mark`-shaped event for
/// [`current_sample`] without a WIT round trip, for stages the host
/// observes directly (T3, T4) rather than through a guest `probe.mark`
/// call. `instar-host` calls this; it cannot call `probe::mark` itself,
/// since that WIT function only exists to be *imported by a guest*.
pub fn record_host_mark(stage: u32) -> u64 {
    let ns = u64::try_from(bench_epoch().elapsed().as_nanos()).unwrap_or(u64::MAX);
    push(ProbeEvent::Mark {
        sample: current_sample(),
        stage,
        ns,
    });
    ns
}

/// Host-side instrumentation helper: records a `report`-shaped event for
/// [`current_sample`], for boundary/work counters the host measures
/// directly (event bytes, scene bytes, layout text bytes) rather than a
/// guest-reported figure.
pub fn record_host_counter(counter: u32, value: u64) {
    push(ProbeEvent::Report {
        sample: current_sample(),
        counter,
        value,
    });
}

/// Registers `instar:kernel/probe` on `linker`. Call this once, alongside
/// the existing `Kernel::add_to_linker` call, only when the `bench-probe`
/// feature is enabled. A guest instantiated against `world kernel` (every
/// production guest, and the GATE build's `guests/scratchpad`) never
/// imports `probe` and is entirely unaffected by whether this was called; a
/// guest instantiated against `world kernel-bench` (the DIAGNOSTIC build)
/// requires it.
pub fn add_to_linker<T: 'static>(linker: &mut Linker<T>) -> wasmtime::Result<()> {
    let mut instance = linker.instance("instar:kernel/probe")?;
    instance.func_wrap(
        "mark",
        |_store: wasmtime::StoreContextMut<'_, T>, (sample, stage): (u64, u32)| {
            let ns = u64::try_from(bench_epoch().elapsed().as_nanos()).unwrap_or(u64::MAX);
            if stage == STAGE_T1_GUEST_RECEIVED {
                CURRENT_SAMPLE.store(sample, Ordering::Release);
            }
            push(ProbeEvent::Mark { sample, stage, ns });
            Ok((ns,))
        },
    )?;
    instance.func_wrap(
        "report",
        |_store: wasmtime::StoreContextMut<'_, T>, (sample, counter, value): (u64, u32, u64)| {
            push(ProbeEvent::Report {
                sample,
                counter,
                value,
            });
            Ok(())
        },
    )?;
    Ok(())
}
