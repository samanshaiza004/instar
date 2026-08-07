use wasmtime::{Config, Engine, Strategy, WasmBacktraceDetails};

/// Builds an engine with Instar's deliberately conservative Wasm feature
/// set. Seeded from `youth-runtime/src/engine.rs`'s `configured_engine`,
/// with one deliberate change: Component Model async is turned **on**
/// (`wasm_component_model_async(true)`) -- this is the entire premise of
/// Gate 0. See the Phase 1 plan's WP2/WP3 and docs/TOOLCHAIN.md.
///
/// Two things are deliberately *not* carried over from `youth-runtime`:
///
/// - The `youth-epoch` 10ms ticker thread (and the `epoch_interruption`/
///   `consume_fuel` config it drove) is gone, not disabled. It existed to
///   interrupt non-yielding, CPU-bound guest code on a wall-clock deadline
///   -- an unrelated concern from suspend/wake, and exactly the kind of
///   permanent polling thread Phase 1's hard idle gates forbid. Cancelling
///   an in-flight *async* host import (this crate's actual concern) is
///   done by dropping the Rust future driving it, not by epoch ticks.
///   Revisit only if guest hangs on CPU-bound (non-yielding) code become a
///   real problem -- out of Phase 1 scope per its own exclusion list
///   ("quota or malicious-guest test suites").
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
        .wasm_gc(false)
        .wasm_threads(false)
        .wasm_shared_everything_threads(false)
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
