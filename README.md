# Youth

Youth is a native host for untrusted, architecture-independent WebAssembly
application components. Applications describe semantic retained UI through a
typed WIT contract; the host owns rendering, input, durable state, and the
transaction boundary around every guest turn.

## Create an application

Install the CLI, check the local toolchain, and generate a standalone Tally
application:

```bash
youth doctor
youth new tally
cd tally
youth check
youth test
youth build --release
youth dev
```

`youth new tally` derives the durable application ID `dev.youth.tally` from
the project name. The ID is not a display name: it namespaces the project's
durable state and other host-owned resources so two applications cannot
accidentally share them. Choose a different stable identity when you need
one, for example `youth new tally --id dev.saman.tally`.

The generated application uses a revision-pinned `youth-sdk`, a strict
language-neutral project manifest and lock, an inspectable WIT snapshot, and
semantic tests backed by the real runtime. New projects currently target
`youth:app@0.0.9`. See the [Quickstart](docs/book/src/quickstart.md), the
historical [DP0 contract](docs/DEVELOPER-PREVIEW-0.md), and the durable
[tooling findings](docs/DEVELOPER-PREVIEW-FINDINGS.md).

## Utility Suite status

The completed Utility Suite applications are independent architecture probes:

| Application | Capability pressure | Release evidence |
| --- | --- | --- |
| [Calculator](https://github.com/samanshaiza004/youth-calculator) | Rows, grid layout, keyboard focus, commands, formatting | Canonical component certified on Ubuntu, Windows, and macOS |
| [Timer](https://github.com/samanshaiza004/youth-timer) | Host clocks, durable schedules, elapsed delivery, recovery | Gate C-4 recovery complete |
| [Todo](https://github.com/samanshaiza004/youth-todo) | Dynamic collections, stable identities, structural updates, migration | `utility-todo-gate-d-release`; canonical component certified on all three hosts |
| Scratchpad | Host-owned text editing, one capability-granted file, transactional Save effects | Gate B implemented on `codex/scratchpad-gate-b`; cross-platform certification pending |

Todo remains on `youth:app@0.0.5`. Its findings are evidence for future
platform decisions, not an authorization for list nodes, structured state,
state enumeration, or automatic tree diffing.

## Editor capability and text entry

The host now owns a native text-editing surface: a stable `Editor` node
identity owns one host-local session (live buffer, cursor, selection, IME
composition, undo/redo, clipboard, scrolling) with zero guest turns for
ordinary typing. The guest sees only whole-buffer `snapshot`/`accept`/
`replace` through the `youth:editor` capability (`youth:app@0.0.6`), never
raw byte offsets or platform key events. `youth:app@0.0.7` additively adds a
modifier field to declared shortcuts, so a focused Editor and an app-level
`Primary+S` Save command coexist. `youth:app@0.0.8` adds one narrowly granted
host-owned text document, exact-byte conflict detection, post-commit atomic
Save, coalesced dirty notification, and opaque host-issued document versions.
`youth:app@0.0.9` adds a `grow` field to the wire node record for a forthcoming
responsive-layout capability; the host layout engine does not use it yet.
File contents remain out of guest memory and Youth state. See
[docs/MILESTONE-2.md](docs/MILESTONE-2.md) for the full contract, lifecycle,
and input-precedence rules and
[docs/SCRATCHPAD-GATE-B.md](docs/SCRATCHPAD-GATE-B.md) for the file effect.
Scratchpad, developed as a separate application
repository the same way Calculator, Timer, and Todo were, is the first
Editor-capable application and exercises this boundary end to end.

## Transactional Visible Counter foundation

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
| `wit/youth-app-v0.0.9/` … `wit/youth-app-v0.0.3/`, `wit/youth-app/` | Versioned `youth:app` contracts, `0.0.2` through `0.0.9`, all frozen and simultaneously supported; new projects embed the `0.0.9` snapshot from the CLI template |
| `crates/youth-tree` | Pure retained semantic-tree engine (no Wasm, no async) |
| `crates/youth-state` | Typed, quota-limited SQLite state and offline verification/repair |
| `crates/youth-runtime` | Wasmtime host: loading, containment, serialized app worker, host-owned Editor sessions |
| `crates/youth-sdk` | Guest-facing builders, typed state, lifecycle, and component export adapter |
| `crates/youth-project` | Strict `Youth.toml`, `Youth.lock`, Cargo, and vendored-WIT contract |
| `crates/youth-test` | Semantic `.youth-test` parser and real headless runner |
| `crates/youth-interaction` | Renderer-independent focus, shortcut, and Editor-input policy (no Wasm) |
| `crates/youth-editor-engine` | Unicode-correct text editing/layout on Parley; the only crate depending on `parley` |
| `crates/youth-text-render-cpu` | Swash-backed CPU glyph rasterization for Editor rendering |
| `crates/youth-desktop` | Deterministic layout/raster/input plus the native window (winit, softbuffer, AccessKit) |
| `crates/youth-cli` | Project generation, doctor/check/build/test/dev, native run, and state tools; the packaged `youth` binary |
| `guests/counter` | Durable counter component (Rust, `wasm32-wasip2`) |
| `test-components/` | Malicious/invalid fixtures and SDK-backed capability fixtures for containment and integration tests |

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

## Distribution

The `youth` CLI binary is packaged for release with [`dist`](https://github.com/axodotdev/cargo-dist)
(`dist-workspace.toml`, `.github/workflows/release.yml`). See
[docs/DISTRIBUTION.md](docs/DISTRIBUTION.md) for supported platforms, install
paths, prerequisites, uninstall steps, and the release procedure.

Offline maintenance never prints stored values. An explicit state root and app
ID select `state/<app-id>/state.sqlite3`:

```bash
cargo run -p youth-cli -- state inspect \
  --app-id dev.youth.counter --state-dir ./state
cargo run -p youth-cli -- state verify \
  --app-id dev.youth.counter --state-dir ./state
```
