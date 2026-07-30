# Release evidence

Youth publishes schema-versioned raw JSON and a readable summary for every
preview release. The schema lives at `docs/metrics/schema-v1.json`; results are
identified by exact Youth, SDK, application, component, toolchain, operating
system, architecture, CPU, runner, profile, and Wasmtime-cache coordinates.

Measurements keep unlike costs separate: source installation is not a future
prebuilt installation; cold and warm Wasmtime caches are distinct; headless
memory and desktop memory are distinct; runtime-turn and presentation latency
are distinct; logical serialized payload size is not claimed to be physical
memory copied by Wasmtime; state-commit latency is a subset of a turn.

DP3 establishes the first multi-application baselines rather than regression
budgets. Hard gates are exact canonical component identity across hosts, zero
idle guest calls, transactional crash recovery, zero raw-WIT concepts in each
external application, semantic convergence where enabled, and explicit
reporting of native accessibility projection as unavailable. Unsupported
measurements are unavailable with reasons, never silently reported as zero.
