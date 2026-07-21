# Youth

Youth is a native host for untrusted, architecture-independent WebAssembly
application components. Applications describe semantic retained UI through a
typed WIT contract; the host owns rendering, input, durable state, and the
transaction boundary around every guest turn.

## Milestone 1 — Transactional Visible Counter

Milestone 1 presents the counter in a native window while preserving the
headless protocol core. A turn becomes visible only after its semantic output
and durable state agree:

```text
clone authoritative tree
→ begin SQLite transaction
→ run the guest against staged state
→ validate acknowledgements and staged semantic tree
→ commit SQLite
→ install the authoritative tree
→ publish the exact patch to the renderer
```

Traps, invalid output, quota failures, and failed commits roll back state and
leave the prior tree authoritative. The runtime also exposes a host-owned
snapshot for renderer recovery without calling the guest. See
[docs/MILESTONE-1.md](docs/MILESTONE-1.md) for the complete contract and
[docs/MILESTONE-0.md](docs/MILESTONE-0.md) for the preserved protocol base.

## Layout

| Path | Purpose |
| --- | --- |
| `wit/youth-app/` | The `youth:app@0.0.2` and `youth:state@0.0.1` WIT contracts |
| `crates/youth-tree` | Pure retained semantic-tree engine (no Wasm, no async) |
| `crates/youth-state` | Typed, quota-limited SQLite state and offline verification/repair |
| `crates/youth-runtime` | Wasmtime host: loading, containment, serialized app worker |
| `crates/youth-desktop` | Deterministic layout/raster/input plus the provisional native window |
| `crates/youth-cli` | Headless tools, native `run`, and state maintenance commands |
| `guests/counter` | Durable counter component (Rust, `wasm32-wasip2`) |
| `test-components/` | Malicious/invalid fixtures for containment tests |

## Building

```bash
cargo build --workspace
cargo build -p youth-counter --target wasm32-wasip2 --release
wasm-tools component wit target/wasm32-wasip2/release/youth_counter.wasm
```

Run the counter with durable state (the default) or an in-memory database:

```bash
cargo run -p youth-cli -- run \
  target/wasm32-wasip2/release/youth_counter.wasm \
  --app-id dev.youth.counter

cargo run -p youth-cli -- run \
  target/wasm32-wasip2/release/youth_counter.wasm \
  --app-id dev.youth.counter --ephemeral
```

Offline maintenance never prints stored values. An explicit state root and app
ID select `state/<app-id>/state.sqlite3`:

```bash
cargo run -p youth-cli -- state inspect \
  --app-id dev.youth.counter --state-dir ./state
cargo run -p youth-cli -- state verify \
  --app-id dev.youth.counter --state-dir ./state
```
