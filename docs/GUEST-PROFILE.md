# Youth Rust Guest Profile 0.0.2

This document records what a Youth guest component is allowed to import,
and why the current Rust guest imports more than `youth:*`.

## Why the counter imports WASI interfaces

`wasm32-wasip2` supports the Rust standard library, and that standard
library targets WASI Preview 2. Linking `std` therefore pulls in
`wasi:cli/*`, `wasi:io/*`, and `wasi:clocks/*` whether or not the guest
calls them. `wasm-tools component wit` shows the full list, which looks
broader than it is.

**Importing an interface does not grant authority.** The host decides
what these interfaces return. Youth builds a `WasiCtx` that inherits
nothing — no arguments, no environment, no stdio, no preopened
directories, no sockets, no network — so these imports are inert. See
[MILESTONE-0.md](MILESTONE-0.md) section 14.

This is a real linking and compatibility surface even so, which is why it
is budgeted rather than left implicit.

## The budget

```text
Required:
  youth:app/ui
  youth:state/store

Permitted inert WASIp2 imports:
  wasi:cli/environment          wasi:cli/terminal-input
  wasi:cli/exit                 wasi:cli/terminal-output
  wasi:cli/stderr               wasi:cli/terminal-stderr
  wasi:cli/stdin                wasi:cli/terminal-stdin
  wasi:cli/stdout               wasi:cli/terminal-stdout
  wasi:clocks/monotonic-clock
  wasi:io/error
  wasi:io/poll
  wasi:io/streams

Forbidden:
  any import outside that allowlist, including every
  wasi:filesystem/*, wasi:sockets/*, and wasi:http/* interface
```

`crates/youth-runtime/tests/import_profile.rs` enforces this against the
built component and fails when a toolchain update widens the surface.
Interface versions are compared without their `@version` suffix, so a
WASI patch bump does not read as a new capability; a genuinely new
interface does.

Widening the allowlist is a deliberate decision. Add the interface to
that test and to this document in the same change, with a note on why the
new capability is acceptable.

## Guest source restrictions

The first guest does not use `println!`, `eprintln!`, `std::fs`,
`std::net`, or `std::env`. Diagnostics are deferred until Youth defines an
explicit diagnostic import.

## Build-target gating

Guest crates begin with:

```rust
#![cfg(all(target_os = "wasi", target_env = "p2"))]
```

Component export symbols only link on the WASIp2 target, so without this
the crates fail to link during a host-target `cargo build --workspace`.
This is build-target gating: it does not vary the emitted component by
host OS or host architecture. The bytes are identical regardless of the
machine that built them, which is what the portability claim requires.

## Future profiles

Youth may later offer two profiles:

- **Standard guest** — Rust `std`, with this constrained WASIp2 support
  surface. The current profile.
- **Strict guest** — `no_std` or a narrower target, importing only
  `youth:*` and explicitly approved primitives.

`no_std` is deliberately not required now. It would damage the developer
experience to address what is, at this stage, a presentation concern.
