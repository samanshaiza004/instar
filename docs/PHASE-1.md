# Instar Phase 1

> **Provenance — read this first.**
>
> This document is a **reconstruction**, not the original Phase 1 plan. The
> original was written in a working session and never committed; by the time
> that gap was noticed, the source text was no longer recoverable. Everything
> below is drawn from one of three places, marked per section:
>
> - **[artifact]** — recoverable from the repo itself (committed code, WIT
>   comments, `TOOLCHAIN.md`, `GATE-0.md`, commit messages, tags).
> - **[directive]** — stated directly by the project owner and transcribed
>   here verbatim or near-verbatim.
> - **[inferred]** — reconstructed from surrounding evidence. Treat as
>   provisional; correct it rather than building on it.
>
> If the original plan text resurfaces, replace this file with it wholesale.
> Nothing here should be treated as authoritative over the original.

## Premise [artifact]

The predecessor codebase (Youth, tag `managed-youth-final`, commit `fac5f8d`)
drove its runtime with a polling loop and a 10ms epoch ticker thread. Instar's
premise is that a guest should sit idle at zero cost and be woken by the host.

That premise is only worth building on if the WebAssembly Component Model's
async support genuinely supports it — so it was tested first, before anything
was built on it. See [GATE-0.md](GATE-0.md).

## Hard idle gates [artifact]

Recorded in `crates/instar-kernel/wit/world.wit` and enforced by
`crates/instar-kernel/tests/gate0.rs`:

> A guest that calls `next-event` in a loop and does nothing else between calls
> must show zero host-import calls, zero polling, and zero CPU use while
> settled.

No permanent polling thread may exist in `instar-kernel`. This is why Youth's
`youth-epoch` ticker was deleted outright rather than ported disabled — see the
rationale in `crates/instar-kernel/src/engine.rs`.

## Forbidden dependencies [artifact]

`instar-kernel` must never depend on winit, Taffy, Vello, softbuffer, a text
renderer, `instar-ui`, or counter-specific types. Stated in the crate's own
docs and `Cargo.toml` description; verified in WP2 via `cargo metadata`.

## Guest lifetime boundary [directive]

Established after Gate 0 exposed that abandoning a started guest task retains
its runtime bookkeeping (see [GATE-0.md](GATE-0.md), Finding 5):

```text
Guest lifetime boundary = Store + component instance.

Never:
drop a suspended guest future
then reuse that Store as if nothing happened.

Guest cancellation/restart:
1. mark generation dead
2. stop accepting its commits
3. cancel host-owned child operations
4. drop the whole instance/Store
5. create fresh Store + instance
6. increment generation
```

Per-operation cancellation is a **separate** mechanism:

```text
guest asks host to cancel operation X
→ host cancels X
→ guest task stays alive
```

Dropping the main guest future is reserved for destroying an entire guest
generation. It is not the per-operation cancellation path.

The correctness boundary that falls out of this:

```rust
if completion.generation != current_generation {
    discard();
}
```

## Toolchain [artifact]

Wasmtime 47.0.3, wit-bindgen 0.60.0, Rust 1.97.1 stable, `wasm32-wasip2`,
wasm-tools 1.255.0. Chosen by head-to-head comparison rather than inherited.
Full reasoning, including the specific upstream async fixes that drove each
pin, is in [TOOLCHAIN.md](TOOLCHAIN.md).

`wasmtime-wasi` brings Tokio in transitively. There is no goal of eliminating
Tokio from the kernel for its own sake — measure first, optimize from evidence.
[directive]

## Measurement policy [artifact]

> Memory and startup are discovery metrics during Phase 1. Do not invent
> targets before measuring the actual baseline.

The pre-rewrite baseline lives in `baselines/managed-youth-final/` and is
partial; what is and isn't captured is documented there.

## Work packages

WP0–WP3 are complete. [artifact — task history and commits]

| WP | Scope | Status |
|---|---|---|
| WP0.1 | Tag `managed-youth-final`, branch `instar-phase-1` | done |
| WP0.2 | Preserve and commit inherited in-progress cleanup | done |
| WP0.3 | Capture the `managed-youth-final` baseline | done (partial — see baseline docs) |
| WP0.4 | Rename `youth-paint`, `youth-render-vello-cpu` → `instar-*` | done |
| WP1 | Choose and pin the toolchain; build an empty component fixture | done |
| WP2 | Scaffold `instar-kernel` with Component Model async enabled | done |
| WP3 | Headless kernel spike — **Gate 0** | done, GO |

Remaining sequence [directive]:

**WP4 — runtime lifecycle** (done)
- `RuntimeGeneration`
- one `Store` per generation
- operation registry
- protocol-level operation cancellation
- whole-generation teardown/restart
- stale completion rejection
- trap fixture
- abandoned-task regression fixture
- bounded-memory soak: 1,000 generation create/suspend/teardown cycles,
  asserting host bookkeeping stays bounded, RSS does not grow linearly, and
  active-task count returns to baseline

**WP5** — `instar-ui` plus minimal button interaction, together (done)
**WP6** — `instar-window`
**WP7** — `instar-host`
**WP8** — counter guest and fixtures
**WP9** — CI rewrite; compare against the WP0.3 baseline

## Gate contingency [directive] — CLOSED

Gate 0 was not to be considered globally closed on a single-platform result:
the product claim is Linux + Windows + macOS, so the contingency stayed open
until `.github/workflows/gate0.yml` passed on all three.

**It has.** All three platforms passed on 2026-08-07 (run 31182995718). The
Gate 0 contingency is closed and the GO verdict is global.

## Out of scope for Phase 1 [artifact]

Noted in `engine.rs` while deciding not to port epoch interruption: quota
enforcement and malicious-guest test suites are excluded. Guest hangs on
CPU-bound, non-yielding code are not a Phase 1 concern.
