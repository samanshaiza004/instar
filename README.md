# Youth

Youth is a native host for untrusted, architecture-independent WebAssembly
application components. Applications describe semantic retained UI through a
typed WIT contract; the host validates every tree and patch batch, enforces
revisions and event ordering, and contains guest execution with fuel, epoch
deadlines, and memory limits.

## Milestone 0 — Headless Protocol Core

No windows, no rendering. Milestone 0 proves the execution spine:

```text
load counter.wasm
→ validate component contract
→ instantiate in isolated store
→ call mount()
→ validate initial tree revision 0
→ enqueue activate(button-4)
→ call handle()
→ validate patch batch
→ atomically update tree to revision 1
→ print canonical tree
```

See [docs/MILESTONE-0.md](docs/MILESTONE-0.md) for the full specification.

## Layout

| Path | Purpose |
| --- | --- |
| `wit/youth-app/` | The `youth:app@0.0.1` WIT contract (single source of truth) |
| `crates/youth-tree` | Pure retained semantic-tree engine (no Wasm, no async) |
| `crates/youth-runtime` | Wasmtime host: loading, containment, serialized app worker |
| `crates/youth-cli` | `youth` CLI: validate, mount, activate, script, inspect |
| `guests/counter` | First valid Youth component (Rust, `wasm32-wasip2`) |
| `test-components/` | Malicious/invalid fixtures for containment tests |

## Building

```bash
cargo build --workspace
cargo build -p youth-counter --target wasm32-wasip2 --release
wasm-tools component wit target/wasm32-wasip2/release/youth_counter.wasm
```
