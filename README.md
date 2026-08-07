# Instar

Instar is a native host for untrusted, architecture-independent WebAssembly
application components. Applications describe semantic retained UI through a
typed WIT contract; the host owns rendering, input, and the boundary around
every guest turn.

> **Status: early. Not usable yet.**
>
> Instar is a ground-up rewrite, currently in Phase 1. What exists today is a
> validated runtime premise and a kernel spike — not an application host you
> can build against. The public API, the WIT protocol, and the crate layout are
> all still expected to change. Much of the tree is still salvage material from
> the previous codebase (see [Inheritance](#inheritance)).

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
| `instar-kernel` | Wasmtime Component Model async runtime: engine config, guest lifecycle, event delivery. No rendering, windowing, or UI dependency of any kind. |
| `instar-paint` | Paint/display-list types. |
| `instar-render-vello-cpu` | CPU rendering backend. |

## Running the gates

The Gate 0 suite builds a real `wasm32-wasip2` guest component from source and
drives it through suspend/wake, concurrency, cancellation, and shutdown:

```bash
cargo test -p instar-kernel --test gate0
```

It runs on Linux, macOS, and Windows in CI — the claims are about a runtime,
not about one machine.

## Toolchain

Pinned deliberately, by comparison rather than by inheritance:
Wasmtime 47.0.3, wit-bindgen 0.60.0, Rust 1.97.1 stable, `wasm32-wasip2`. The
reasoning — including which specific upstream async fixes drove each pin — is
in [docs/TOOLCHAIN.md](docs/TOOLCHAIN.md).

## Inheritance

Instar began from a codebase called Youth and keeps its git history. Several
`youth-*` crates are still present as salvage material — source that later work
packages extract from and then delete, not code that gets fixed in place. They
are not part of Instar's design and should not be treated as current.

`docs/baselines/managed-youth-final/` records the pre-rewrite baseline so the
rewrite can be measured against it later. It is deliberately kept.

## License

MIT OR Apache-2.0.
