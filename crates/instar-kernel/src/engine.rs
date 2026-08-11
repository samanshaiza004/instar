use wasmtime::{Config, Engine, Strategy, WasmBacktraceDetails};

/// Builds an engine with Instar's deliberately conservative Wasm feature
/// set. Seeded from `youth-runtime/src/engine.rs`'s `configured_engine`,
/// with one deliberate change: Component Model async is turned **on**
/// (`wasm_component_model_async(true)`) -- this is the entire premise of
/// Gate 0. See docs/PHASE-1.md WP2/WP3 and docs/TOOLCHAIN.md.
///
/// Two things are deliberately *not* carried over from `youth-runtime`:
///
/// - The `youth-epoch` 10ms ticker thread is gone, not disabled. Epoch
///   interruption itself is enabled (`epoch_interruption(true)` below), but
///   there is no periodic thread incrementing the epoch: Wasmtime's epoch
///   counter is left untouched during normal idle operation, which is what
///   keeps Gate 0's no-polling property intact. The only epoch increment in
///   Instar is the out-of-band RuntimeThread shutdown signal (HARDEN-3), which
///   needs the instrumentation to interrupt a non-yielding guest that never
///   calls a host import. `consume_fuel` remains off; fuel is Youth's quota
///   scheduler and this is containment, not a CPU budget. Cancelling an
///   in-flight *async* host import (this crate's actual concern) is still
///   done by dropping the Rust future driving it, not by epoch ticks.
/// - The transactional turn machinery in `youth-runtime/src/host.rs` and
///   `src/worker.rs` (`TurnReceipt`, `HostState`, mount/activate/resync) is
///   not ported at all. It's entirely `youth-state`-coupled and is on the
///   Phase 1 delete list; `instar-kernel` has no notion of durable state.
pub fn configured_engine() -> wasmtime::Result<Engine> {
    let mut config = Config::new();
    config
        .strategy(Strategy::Cranelift)
        .wasm_component_model(true)
        .wasm_component_model_async(true)
        .wasm_backtrace_details(WasmBacktraceDetails::Enable)
        // Instrument Wasm so a Store whose epoch deadline is reached traps.
        // No ticker drives this: the epoch only moves when shutdown asks it
        // to, so idle guests and the no-polling gate are untouched.
        .epoch_interruption(true)
        // GC, threads and memory64 used to be switched off here. They are now
        // absent from the build instead: `instar-kernel` takes Wasmtime with
        // `default-features = false`, so the proposals are not compiled in and
        // there is nothing to disable. A capability that does not exist cannot
        // be re-enabled by a later edit to this function.
        .wasm_memory64(false);
    Engine::new(&config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_engine_builds() {
        configured_engine().expect("engine config is valid");
    }
}
