use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use wasmtime::{Config, Engine, Strategy, WasmBacktraceDetails};

const EPOCH_TICK: Duration = Duration::from_millis(10);

/// Builds an engine with Youth's deliberately conservative Wasm feature set.
pub fn configured_engine() -> wasmtime::Result<Engine> {
    let mut config = Config::new();
    config
        .strategy(Strategy::Cranelift)
        .wasm_component_model(true)
        .wasm_component_model_async(false)
        .consume_fuel(true)
        .epoch_interruption(true)
        .wasm_backtrace_details(WasmBacktraceDetails::Enable)
        .wasm_gc(false)
        .wasm_threads(false)
        .wasm_shared_everything_threads(false)
        .wasm_memory64(false);
    Engine::new(&config)
}

/// Returns the one lazily initialized engine shared by all default app instances.
pub fn shared_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let engine = configured_engine().expect("Youth's static Wasmtime configuration is valid");
        let ticker_engine = engine.clone();
        thread::Builder::new()
            .name(String::from("youth-epoch"))
            .spawn(move || {
                loop {
                    thread::sleep(EPOCH_TICK);
                    ticker_engine.increment_epoch();
                }
            })
            .expect("Youth can start its epoch deadline thread");
        engine
    })
}

pub(crate) fn deadline_ticks(deadline: Duration) -> u64 {
    let tick_nanos = EPOCH_TICK.as_nanos();
    let ticks = deadline.as_nanos().div_ceil(tick_nanos);
    u64::try_from(ticks).unwrap_or(u64::MAX).max(1)
}
