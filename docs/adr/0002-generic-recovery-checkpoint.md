# ADR 0002: generic opaque checkpoint for guest-owned application truth

Status: proposed

## Context

Phase 3 (`docs/PHASE-3.md`, ADR 0001) made the guest the only authority over
application text: no host document replica, selection, undo, editor
commands, or text revision synchronization. That pivot is correct and stays.

It leaves a real gap. Unsaved guest-resident application state can disappear
when a generation traps, when ordered-input saturation terminates a
generation (the `"guest input queue overflow"` path in
`crates/instar-host/src/bridge.rs`), when a runtime generation is replaced,
or when the host process itself dies. None of Instar's existing mechanisms
address this, and none should be extended to: `ops.start(kind: string, ...)`
is a test-only string dispatcher (`GenerationState::start_operation`), not a
typed, bounded, safety-relevant capability, and using it here would put
durability-critical behavior outside this project's actual convention of a
dedicated interface and error taxonomy per capability domain.

## Decision

Add a generic, opaque checkpoint capability: `checkpoint.write(slot, seq,
bytes, durability) -> write-ack`, `checkpoint.read(slot) -> option<recovered>`,
`checkpoint.discard(slot)`. The host stores bytes under an opaque,
guest-chosen `slot` key inside a host-derived `recovery_scope` (owned by the
window/launch context that already survives generation replacement, the same
state `Bridge::on_terminal` already closes over). It never decodes, diffs,
merges, or applies those bytes. Full analysis, alternatives considered and
rejected, WIT shape, bounds, and test plan live in the design writeup this
ADR accompanies (see chat/session record; promote into this file's body if
this ADR is accepted).

Two durability tiers are exposed, not three, because a third
"power-loss-safe" tier cannot be honestly provided or verified from user
space:

* `buffered` — generation-safe. Survives guest trap, ordered-input
  saturation, and generation replacement, because it never leaves host
  process memory. Does not survive host process death, and is deliberately
  not called "process-restart-safe": a restart is definitionally the old
  process ending, so a tier defined by what survives *within* one process
  cannot honestly borrow that name.
* `flushed` — host-process-crash-safe. Persisted via a real durable-flush
  syscall before acknowledgement. Power-loss-safe only insofar as the
  platform's flush syscall is honest, which Instar cannot verify and does
  not claim to.

`best-effort` writes never block on disk I/O and are the default; `flush-now`
is an explicit, rare, guest-opted cost paid off the input path. Exact
zero-loss host-crash durability would require a synchronous flush barrier
before every keystroke's frame — that costs more than the entire p95 typing
latency budget by itself and is rejected on those grounds, explicitly, not
silently traded away.

## Consequences

* The host gains its first persistent on-disk state. It is namespaced,
  bounded (per-write size cap, per-scope slot-count cap), and never
  interpreted — a slot name is never used as a literal filesystem path.
* A guest that never calls `checkpoint.write` costs nothing; the mechanism is
  additive, not a new default obligation on every guest.
* Recovery is bounded, not exact: the loss window under a real host crash is
  the debounce interval (proposed ~2 s worst case under continuous input),
  named and tested, not assumed away.
* Leaves room for an additive `append` alongside `write` later (closer to a
  journal) if a real large-document guest demonstrates whole-value overwrite
  costs too much — same slot identity, same error taxonomy, no redesign.

## Proof obligations

A subprocess `SIGKILL` test that acknowledges a `flushed` write, kills the
host, and relaunches must recover exactly those bytes. A parallel test that
acknowledges only a `buffered` write and kills the host must recover the
*previous* flushed value or nothing — never falsely report the lost write as
recovered. A path-traversal fuzz test on `slot` bytes must show every
resulting on-disk path stays inside the scope directory. See the full mutant
catalog in the accompanying design writeup.
