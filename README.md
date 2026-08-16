# Instar

Instar was a native host for untrusted, architecture-independent WebAssembly
application components. Applications described semantic retained UI through a
typed WIT contract; the host owned rendering, input, and the boundary around
every guest turn.

[Website](https://instar.samanshaiza.com/) ·
[Install guide](https://instar.samanshaiza.com/docs/getting-started/install) ·
[Documentation](https://instar.samanshaiza.com/docs/)

> **Status: on hiatus, indefinitely. Kept as a reference, not developed
> further in its current form.**
>
> ### Why
>
> Instar's thesis was that a whole desktop application should live behind a
> Wasm Component Model boundary. Three phases of work — async runtime,
> retained UI, and a host-owned text replica — went into proving that
> boundary could carry a real application without giving up latency or
> ownership clarity. It can. That was never the question that mattered.
>
> The question that mattered: **what does the boundary buy, for an
> application the host itself is writing?** Every hard problem the project
> solved — authority reconciliation, generational teardown, resync after a
> forced desync, host/guest revision protocols, IME across a process
> boundary, epoch interruption, capability leases — exists *only because a
> boundary is there*. None of it is free, all of it is real engineering, and
> none of it would exist in a native Rust application built directly on the
> same underlying stack (Taffy, Parley, Vello, AccessKit).
>
> The one property that justifies paying that cost is running code the host
> does not trust. The README described Instar as a host for *untrusted*
> components from the start, but nothing built in Phases 1–3 exercised that:
> every guest was first-party Rust, written by the same person building the
> host, with no capability model, no permission story, and no adversarial
> input this project treated as real. The tax was paid; the thing it pays
> for was never built.
>
> Phase 3 made this legible rather than hiding it. Text is the most
> latency-sensitive interaction in a desktop application, and putting it
> behind the boundary meant either the host owning a replica — solved, at
> real cost, in `docs/PHASE-3.md` — or the guest owning it, which reintroduces
> the round-trip the whole architecture existed to avoid. Both are
> consequences of the boundary. Choosing a text editor as the flagship
> application was choosing the hardest possible case to validate a premise
> that was never actually being tested.
>
> Strip the boundary out and what remains is a retained-mode GUI stack on
> Taffy, Parley, Vello, and AccessKit — which is also what
> [Xilem/Masonry](https://github.com/linebender/xilem) is, natively, with no
> IPC boundary in the middle. Instar's differentiator over that stack was a
> boundary that cost latency and bought nothing exercised.
>
> ### What was real
>
> The engineering discipline held up under its own pressure: Gate 0 tested
> the async premise before anything was built on it
> ([docs/GATE-0.md](docs/GATE-0.md)); the crate seams stayed honest under
> real load (the kernel has no render dependency, `instar-ui` never sees DPI,
> `instar-window` never sees a `NodeKey`); wrong decisions were recorded
> rather than quietly reverted
> ([docs/DESIGN-LEDGER.md](docs/DESIGN-LEDGER.md)); provisional numbers were
> labeled provisional. That discipline is the part of this repository worth
> reading regardless of the verdict above.
>
> ### Where this goes, if it goes anywhere
>
> Not as a runtime every first-party Instar application runs inside. If this
> resumes, it resumes narrower: a sandboxed extension/plugin host for code
> the *application* deliberately does not trust — closer to how Zed runs its
> extensions in Wasm while keeping its own editor native. That version has a
> concrete user story (a third-party plugin that must not read arbitrary
> files or hang the host) instead of an architectural one, and most of the
> kernel — generations, capability leases, resource accounting, teardown —
> is directly reusable there. Everything upstream of that decision (a
> first-party application living inside the boundary) is what stops here.
>
> The phase documents are the historical record of how each decision was
> reached, including the ones that turned out wrong:
> [docs/PHASE-1-RESULTS.md](docs/PHASE-1-RESULTS.md),
> [docs/PHASE-2-RESULTS.md](docs/PHASE-2-RESULTS.md),
> [docs/PHASE-3.md](docs/PHASE-3.md).

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
| `instar-ui-protocol` | Semantic UI snapshots and neutral Surface input. Zero dependencies. |
| `instar-surface-protocol` | Independent bounded Surface scene wire. Zero dependencies. |
| `instar-editor-core` | Optional guest-side document and editing primitives. Replaceable policy, not a host contract. |
| `instar-sdk` | Optional guest-side snapshot builder and event router. Depends only on `instar-ui-protocol`; the wire stays hand-encodable. |
| `instar-kernel` | Wasmtime Component Model async runtime: engine config, guest lifecycle, event delivery. No rendering, windowing, or UI dependency of any kind. |
| `instar-ui` | The retained tree, Taffy layout, hit-testing. Never sees DPI. |
| `instar-text-layout` | Host-owned immutable text shaping and layout seam. |
| `instar-window` | winit translation and DPI conversion. Never sees a `NodeKey`. |
| `instar-paint` | Paint intent: scene and command types. |
| `instar-render-vello-cpu` | CPU rendering backend. |
| `instar-host` | Orchestration: routing, the metrics barrier, the two-thread bridge, scene lowering. |
| `instar-shell` | The event loop, presentation, the font, the binary. |
| `instar-guest-build` | Build-script support for compiling guests. |
| `recovery-harness` | Generic checkpoint and recovery test support. |

`guests/` holds every WebAssembly component, each built from source rather than
committed as an artifact. See [guests/README.md](guests/README.md).

How Instar works and why each boundary is where it is:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The current protocols:
[docs/PROTOCOL.md](docs/PROTOCOL.md). The Phase 1 wire record is archived at
[docs/history/PROTOCOL-0.md](docs/history/PROTOCOL-0.md). What it costs, and
what CI does and does not enforce:
[docs/baselines/PERFORMANCE.md](docs/baselines/PERFORMANCE.md).

`docs/PHASE-1.md` and `docs/PHASE-2.md` are the record of *how* the decisions
were reached, including the ones that turned out wrong. They are logs, not
references — when they disagree with `ARCHITECTURE.md`, that file is the one to
trust.

## Running it

`instar` takes a component and runs it. Build the example counter, then run it:

```bash
cargo build --release --manifest-path guests/counter/Cargo.toml --target wasm32-wasip2
```

```bash
cargo run --release --package instar-shell --bin instar -- run \
  guests/counter/target/wasm32-wasip2/release/counter.wasm
```

A window with a counter, a reset, and a button that crashes the guest on
purpose — worth clicking, because a crash surface nobody has seen is a crash
surface that does not work.

`--debug` reports lifecycle, commits, and frame timings on stderr.

**`run` is the only command.** `new`, `build`, `dev`, `package`, `inspect`,
`validate`, and `doctor` do not exist, deliberately: each would freeze
assumptions about manifests, build systems, package layout, SDKs, and
distribution that this project has not learned yet. A command added now would
be a guess preserved as an interface.

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
cargo run --release --example overhead
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
