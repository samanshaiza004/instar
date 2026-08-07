# Instar

Instar is a native host for untrusted, architecture-independent WebAssembly
application components. Applications describe semantic retained UI through a
typed WIT contract; the host owns rendering, input, and the boundary around
every guest turn.

> **Status: early. Phase 1 complete; not stable.**
>
> Instar runs a real WebAssembly component in a real window: a guest that sits
> idle at zero cost, is woken by a click, describes an interface it owns no
> geometry in, and gets it rendered — and when it dies, the host says so on a
> surface the guest cannot influence.
>
> That is a validated foundation, not a product. The public API, the WIT
> protocol, and the crate layout are all still expected to change, and there is
> no compatibility promise of any kind.
>
> What Phase 1 proved, cost, and left as scaffolding:
> [docs/PHASE-1-RESULTS.md](docs/PHASE-1-RESULTS.md).

## Why the rewrite

The predecessor codebase drove its runtime with a polling loop and a 10ms
ticker thread. Instar's premise is that a guest should sit idle at *zero* cost
and be woken by the host — which is only worth building on if the underlying
runtime genuinely supports it.

That premise was tested before anything was built on it. **Gate 0 passed**: a
real WebAssembly Component Model guest suspends on an async host import, is
woken by the host, makes concurrent progress across independent async
operations, and shuts down cleanly — with an idle guest costing no polls at
all. The measurements, the method, and the one limitation it exposed are in
[docs/GATE-0.md](docs/GATE-0.md).

That limitation shapes the design rather than blocking it: a suspended guest
task owns a wasm stack nothing can force-unwind, so **the guest lifetime
boundary is the `Store` plus component instance**. Cancelling a guest means
tearing down its whole generation and starting a fresh one; per-operation
cancellation is a protocol concern, handled by the guest cancelling its own
subtask.

## Layout

| Crate | What it is |
|---|---|
| `instar-ui-protocol` | The wire format, and the only Instar crate a guest links. Zero dependencies. |
| `instar-kernel` | Wasmtime Component Model async runtime: engine config, guest lifecycle, event delivery. No rendering, windowing, or UI dependency of any kind. |
| `instar-ui` | The retained tree, Taffy layout, hit-testing. Never sees DPI. |
| `instar-window` | winit translation and DPI conversion. Never sees a `NodeKey`. |
| `instar-paint` | Paint intent: scene and command types. |
| `instar-render-vello-cpu` | CPU rendering backend. |
| `instar-host` | Orchestration: routing, the metrics barrier, the two-thread bridge, scene lowering. |
| `instar-shell` | The event loop, presentation, the font, the binary. |
| `instar-guest-build` | Build-script support for compiling guests. |

`guests/` holds every WebAssembly component, each built from source rather than
committed as an artifact. See [guests/README.md](guests/README.md).

The architecture, and why each boundary is where it is:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The wire format:
[docs/PROTOCOL-0.md](docs/PROTOCOL-0.md).

## Running it

```bash
cargo run --release
```

A window with a counter, a reset, and a button that crashes the guest on
purpose — which is worth clicking, because a crash surface nobody has seen is a
crash surface that does not work.

## Running the gates

The Gate 0 suite builds a real `wasm32-wasip2` guest component from source and
drives it through suspend/wake, concurrency, cancellation, and shutdown:

```bash
cargo test -p instar-kernel --test gate0
```

The whole suite, including the bridge acceptance gate and the pixel-level
render tests:

```bash
cargo test --workspace
```

Both run on Linux, macOS, and Windows in CI — the claims are about a runtime,
not about one machine.

## Overhead

What each layer costs, measured rather than estimated:

```bash
cargo run --release --bin overhead
```

Numbers and method in [docs/OVERHEAD.md](docs/OVERHEAD.md).

## Toolchain

Pinned deliberately, by comparison rather than by inheritance:
Wasmtime 47.0.3, wit-bindgen 0.60.0, Rust 1.97.1 stable, `wasm32-wasip2`. The
reasoning — including which specific upstream async fixes drove each pin — is
in [docs/TOOLCHAIN.md](docs/TOOLCHAIN.md).

## Inheritance

Instar began from a codebase called Youth and keeps its git history. The
`youth-*` crates were salvage material — source that later work packages
extracted from and then deleted — and as of Phase 1 they are gone. Every one was
either lifted (the kernel's engine config, the windowing and host layers, the
paint and render crates) or superseded.

The source is not lost: it is all at the `managed-youth-final` tag, and
`docs/baselines/managed-youth-final/` records the pre-rewrite baseline so the
rewrite can be measured against it. Both are deliberately kept.

## License

MIT OR Apache-2.0.
