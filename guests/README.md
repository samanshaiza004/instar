# Instar guests

Every WebAssembly component in the repository, in one place (WP8). Each is its
own workspace, targets `wasm32-wasip2`, and is **built from source** by the
build script of whichever crate needs it — never committed as a `.wasm`.

That last part is deliberate. A checked-in artifact could pass a gate the
current toolchain would fail, which matters most for Gate 0 (whose entire
question is whether *this* toolchain's async support behaves) and matters
everywhere else because a guest is the only thing here compiled for a different
target under a different set of assumptions. See `crates/instar-guest-build`.

| Guest | Built by | Used for |
|---|---|---|
| `counter` | `instar-ui`, `instar-shell` | the interface the shell runs, and the UI interaction tests |
| `hostile` | `instar-host` | the bridge acceptance gate and every way a guest may misbehave |
| `kernel-guest` | `instar-kernel` | generation lifecycle and host operations (WP4) |
| `kernel-spike-guest` | `instar-kernel` | Gate 0 — idle suspension at zero cost (WP3) |

## The one rule

**A guest links `instar-ui-protocol` and nothing else of Instar's.** Kernel-level
guests link none of it at all.

This is what lets `instar-ui` take on a layout engine and `instar-host` take on
a renderer: none of it can reach a guest, so none of it becomes a compatibility
obligation. It is enforced by
`crates/instar-shell/tests/layering.rs`, as a subset rule rather than a
blocklist — a blocklist stops covering the crate that does not exist yet.

## Why `counter` is not a fixture

`instar-ui`'s interaction tests and `instar-shell`'s render tests both run the
guest the shell actually ships. Before WP8 each had its own near-copy, which is
the arrangement where a fixture slowly drifts into testing a program nobody
runs.

## Why `hostile` exists

The bridge's interesting properties are all about what happens when things go
wrong, and a well-behaved guest cannot exercise any of them. `hostile` commits
garbage, commits well-formed nonsense, commits more than the protocol allows,
goes silent, traps, and fails with an error larger than the host will display —
each reachable by clicking a button, through the ordinary wire format, with no
back door in the host.

One finding worth keeping, from building it: **a trap's message is not the
guest's.** What the host receives from a panicking guest is Wasmtime's
backtrace; the guest's own panic text goes to the guest's stderr and never
crosses the boundary. The error a guest returns from `run` is the only string
in this world whose length the guest controls outright.
