# Developer Preview 2 — Durable Scheduling

- Status: **Gate B complete** (platform capability); Gate C in progress (application adoption)
- Checkpoint: `timer-gate-b-platform`
- Application protocol: `youth:app@0.0.4`, with runtime compatibility for `0.0.3` and `0.0.2`
- Capability protocol: `youth:time@0.0.1`
- State protocol: `youth:state@0.0.1` (schema version 3)
- Driving application: [Youth Timer](https://github.com/samanshaiza004/youth-timer)
- Evidence: `TIMER-F001`, `TIMER-F002`, `TIMER-F003`, `TIMER-F006`, `TIMER-F008`, `TIMER-F011`

## Gate status

| Gate | Content | Status |
| --- | --- | --- |
| B-1a | `youth:time@0.0.1` WIT, protocol `0.0.4` beside `0.0.3`/`0.0.2` | Complete (`9e45a91`) |
| B-1b | SDK `context.time()`, contract-profile table | Complete (`a5f5355`) |
| B-1c | Generated projects emit `0.0.4` against the published SDK | Complete (`afdd34c`) |
| B-2 | Durable, transaction-bound schedule storage | Complete (`95c983e`) |
| B-3 | Deadline / wake / guest-clock seams, pure scheduler, reconciliation | Complete (`6d29a50`) |
| B-4a | Typed elapsed delivery, acknowledgement inside the turn transaction | Complete (`a924c92`) |
| B-4b | Host-initiated delivery through one worker mailbox, observers | Complete (`97d7b3b`) |
| C-1/C-2 (Timer) | Timer migrates durable state and adopts real schedules | Complete |
| C3-1 | `youth:app@0.0.5` WIT, `youth-tree` countdown schema (D7) | Complete (`38683db`) |
| C3-2 | Runtime dispatch for `0.0.5`, install-time reference validation (D7d) | Complete (`af83784`) |
| C3-3 | SDK `Countdown` builder, literal/countdown wire plumbing | Complete (`e30e001`) |
| C3-4 | Pure display resolution (D7c), desktop `WaitUntil` repaint (D7b), decisive test | Complete (`1d8c227`) |
| C-3 (Timer) | Timer adopts `Countdown` for its own countdown text node | Not started |
| C-4 | Recovery and release | Not started |

Gate B proved the **host capability** for scheduling; Gate C-3 proves it for
**presentation** — a countdown's digits are host-resolved and host-repainted,
never a guest turn. Both are platform capability, not yet application
evidence: the architectural loop closes only once Timer's own `view()`
declares a `Countdown` node instead of a static duration string. Until then
`TIMER-F004` remains open as *application* evidence, and the `0.0.5`
`ContractProfile` in `youth-project` is deliberately unpublished — see the
comment above `SUPPORTED_PROFILES` — pending the post-push two-commit
SDK-revision pin, the same discipline already used for `0.0.4` and its
pause/resume fix.

## Thesis

> A Youth application declares bounded temporal intent. The host owns clocks,
> sleeping, temporal presentation, durable reconciliation, and semantic delivery.

A countdown must be able to update for its full duration while the guest stays
idle, followed by one transactional semantic event when the deadline elapses.

## What the Timer proved, and what it corrected

The Timer's Gate A proof (`TIMER-F009`) established that the non-temporal
surface — mode machines, bounded configuration, commands, shortcuts, durable
persistence, restart recovery — needs **no protocol change**. DP2 is therefore
deliberately narrow. It adds scheduling, presentation, delivery, reconciliation,
and notification. It does not touch the application lifecycle, the persistence
API, or the command system.

`TIMER-F001` was corrected on 2026-07-25 after this specification's research:
a guest **can** read real monotonic time today by calling `Instant::now()`,
because `wasi:clocks/monotonic-clock` is a permitted import, is linked by
`add_to_linker_sync`, and `WasiClocksCtx::default()` installs the real host
clock. The missing capability was never *measurement*. It is being **woken**
when a duration passes, presenting without a turn, and surviving process exit.

That correction produces a design requirement DP2 must satisfy, below.

## Decisions

### D1 — Schedules live in the application's own state database

`StateStore` owns a private `rusqlite::Connection` with no accessor, and a
second connection to the same file could not join the open transaction — it
would contend against `BEGIN IMMEDIATE` and fail on the 250 ms busy timeout.
Since the design requires that "a rejected or rolled-back turn must not leave an
active schedule," schedule rows must be written **through `StateStore`, inside
the turn's existing transaction**.

Schedules therefore become new `youth_schedule*` tables in the same
`<state_root>/<app-id>/state.sqlite3`, following the existing convention that
`verify_schema_shape` enforces: `STRICT`, `WITHOUT ROWID`. Atomicity is then
free — the schedule commits exactly when the application state and semantic tree
commit, and rolls back with them.

This also answers `TIMER-F011`'s storage half: `StateStore::open` is a leaf-crate
operation requiring no engine, component, or guest (this is how
`youth state inspect` already works), so a host scheduler can read pending
schedules for an app that is not running.

This is the first schema change since `SCHEMA_VERSION = 1` and introduces
Youth's first migration path.

### D2 correction — `pause`/`resume` must return the new schedule

Found while wiring the Timer to real schedules (2026-07-27), before any
external consumer had adopted `youth:time@0.0.1`, so patched in place
rather than versioned: `schedule_pause`/`schedule_resume` in
`youth-state` already compute and return a full `ScheduleRecord` carrying
the new generation, but the WIT signatures were `pause: func(value:
schedule) -> result<_, schedule-error-code>` and the equivalent for
`resume` — the new generation was computed, then discarded before
crossing back to the guest. A guest that paused a schedule had no way to
learn the generation it would need to resume with, and would be rejected
by the host's own stale-generation check on the very next call. `pause`
and `resume` now return `result<schedule, schedule-error-code>`, exactly
like `schedule-after`.

### D2 — Schedule identity and generation are host-issued

Per `TIMER-F008`, a guest-invented counter cannot be trusted to reject a wake the
guest itself did not generate. `schedule-after` returns an **opaque** identity
and generation assigned by the host and checked by the host before any guest is
invoked. A guest may durably store and read that identity back; it may not
construct one. An application's own session counter (the Timer's
`completed_sessions`) remains guest-owned domain data and must not be conflated
with a schedule generation.

### D3 — Three separate time concerns, never one ambiguous "clock"

"Clock" must not become a single abstraction. There are three distinct roles.
They may share a source in production, but they stay separate interfaces:

```text
DeadlineClock      Determines when a durable deadline becomes due.
                   MUST be restart-stable, therefore wall/epoch time.

WakeDriver         Efficiently wakes a live process at a deadline.
                   Process-local only; monotonic sleeping.

Guest WASI clock   Prevents guest libraries observing uncontrolled real
                   time. Hermeticity only; never a scheduling input.
```

```rust
trait DeadlineClock { fn now_epoch_millis(&self) -> u64; }
trait WakeDriver {
    fn arm(&self, token: WakeToken, delay: Duration);
    fn cancel(&self, token: WakeToken);
}
```

**A monotonic clock cannot support overdue reconciliation across a process
restart.** Monotonic readings are meaningless once the process dies, so a
persisted deadline must be recorded against a restart-stable basis. Production
therefore uses wall time for durable reconciliation and monotonic sleeping for
process-local wake efficiency. The parameter carried into storage is named
`now_epoch_millis` precisely so a later "clock is clock" substitution cannot
silently turn every persisted deadline into garbage.

**System-clock rollback policy.** If wall time moves backward, an
already-armed process-local timer must not be silently extended. While the
process is alive, waking is driven by monotonic sleeping; wall time is consulted
only for restart and suspend reconciliation.

**The guest's WASI clock is not the scheduling API.** It is overridden via
`WasiCtxBuilder::monotonic_clock(..)` purely so a dependency calling
`Instant::now()` cannot introduce nondeterminism. `wasi:clocks/monotonic-clock`
cannot be removed from the allowlist — Rust `std` links it unconditionally — so
overriding is the only available control. Two rules are binding: WASI time is
not the Youth scheduling API, and a guest calling `Instant::now()` must never be
able to schedule host delivery or derive a durable host deadline. The scheduler
depends on `DeadlineClock` directly, never on the guest's WASI clock.

The Wasmtime **epoch** thread is a fourth, separate mechanism and is explicitly
*not* reused or virtualized. It exists to preempt runaway guests, its 10 ms tick
has no wall-clock meaning, and virtualizing it would invalidate the containment
tests that assert an infinite loop dies in bounded real time.

### D3a — The scheduler state machine is pure

The state machine stays independent of threads, Tokio, winit, and Wasmtime, so
virtual-clock tests can drive every transition exhaustively without sleeping.

```text
status:  running | paused | due | cancelled
token:   { schedule_id, generation }

inputs:  create, pause, resume, cancel, clock advanced, process opened,
         wake received, delivery committed, delivery rejected
outputs: persist mutation, arm wake, cancel wake, queue elapsed delivery,
         discard stale wake
```

**Stale-wake rejection happens before any guest turn is constructed.** A wake
carries application id, schedule id, and generation. On receipt the authoritative
schedule is re-read from storage — never trusted from an in-memory map, since a
pause, resume, or cancel may have committed after the wake was armed — and the
wake is discarded unless the schedule exists, is running, the generation matches,
the deadline is due, and no delivery is already pending.

**Overdue reconciliation on startup** loads running schedules ordered by deadline
then creation sequence, compares each against the `DeadlineClock`, transactionally
converts overdue ones to due, creates at most one pending delivery per generation,
arms future schedules with the `WakeDriver`, and leaves paused and cancelled
schedules unarmed. Reconciliation must be **idempotent**: opening the scheduler
twice must not produce duplicate deliveries.

### D4 — The project contract accepts a set of protocols

`youth-project` currently compares an application manifest's `protocol` against a
single `SUPPORTED_PROTOCOL`. With two published applications this breaks: the
calculator declares `0.0.3` and would be rejected the moment the host moves to
`0.0.4`, failing the external-calculator CI job.

`SUPPORTED_PROTOCOL` becomes a supported **set** with a distinguished default for
newly generated projects, mirroring how the runtime already accepts multiple
protocols. This is a direct consequence of having a second Utility Suite
application and should be recorded as such.

### D4a — A host stimulus is not an application command

`AppCommand` has meant "someone requested an operation and expects a result."
A wake is neither. The worker takes a typed envelope over **one FIFO mailbox**
fed by both sources, so it stays sequential without competing loops or polling:

```rust
enum WorkerMessage {
    Request { command: AppCommand, reply: ReplySender },
    WakeFired(WakeToken),
    DeliverPending,
    Shutdown,
}
```

Mailbox order *is* the host-observed order, which is what makes
cancel-vs-wake races deterministic rather than thread-timing dependent. Both
orderings are specified and tested: cancel-first makes the later wake stale;
wake-first makes the schedule due and the later cancel observes due state.

### D4b — A wake is a hint, never proof

`WakeToken { app_id, schedule_id, generation }` carries no authority. Before any
guest turn is constructed the authoritative record is re-read and **all** must
hold: the schedule exists; its status is appropriate; the generation matches;
the deadline is genuinely due; a pending delivery exists or is created exactly
once; that delivery has not already committed; and the loaded component's
protocol can represent schedule events (D4d). Otherwise the wake is discarded.

### D4c — The SDK decodes fail-closed

Two cases must not be conflated:

```text
Recognized event the application ignores
    -> a successful turn may acknowledge it

Event the SDK cannot represent
    -> do not invoke the application
    -> do not advance processed-through
    -> leave the pending delivery durable
```

The SDK decodes every incoming event *before* constructing `Events`. An
unsupported or malformed kind returns a wire/compatibility error. Applications
see events explicitly rather than through a lossy filter:

```rust
pub enum Event {
    Activated(NodeId),
    ScheduleElapsed { schedule: ScheduleId, generation: Generation, reason: ElapsedReason },
}
```

Convenience helpers may exist, but the typed event stays iterable so a delivery
can never become semantically invisible — the failure mode marked in the B-1b
adapter, where `processed_through` would acknowledge an event the application
never received.

### D4d — Schedules record the protocol that owns them

A durable schedule outlives the component that created it, so a downgrade is
reachable: a `0.0.4` Timer arms a schedule, the user later launches an older
component against the same app id and state root, and the deadline passes.
Youth must not drop the event nor feed it to a component that cannot decode it.
Schedule and pending-delivery rows therefore record the protocol/capability
required to consume them, and the host **fails closed**: the delivery stays
stored, the older component is never invoked with it, and the incompatibility is
reported.

### D4e — One committed outcome, one delivery per turn

A committed receipt is produced exactly once and never reconstructed afterward:

```rust
struct TurnOutcome { origin: TurnOrigin, receipt: TurnReceipt }
enum TurnOrigin { Requested(RequestId), ScheduleDelivery { schedule, generation } }
```

Requested turns reply to the requester and may publish; host-initiated turns
publish only. Observers (desktop, headless tests, CLI) consume one channel
carrying `TurnCommitted`, `Faulted`, and `SnapshotReplaced`, and see a patch
**only after commit and tree installation** — never one from a rolled-back
delivery.

Exactly one pending delivery is consumed per turn, ordered by deadline, then
creation sequence, then schedule id as a stable tie-break. Batching may be
designed later from measured evidence.

### D4e1 — Observers are mirrors, never authorities

Publication is a bounded broadcast, so a slow subscriber **can** miss
outcomes. That is acceptable only because durable state and the runtime's
retained tree remain authoritative; an observer's local copy never is. The
recovery rule is binding on every subscriber, including the desktop:

```text
observer receives lag / discontinuity
→ discard its local mirror assumptions
→ request the authoritative runtime snapshot
→ replace the mirror
→ continue from the snapshot revision
```

No subscriber may assume every `TurnOutcome` reaches it, and none may treat a
missed outcome as a lost change — the change is in committed state regardless.
This is why publication overflow is survivable rather than a correctness hole,
and it is the same recovery the renderer already performs on patch mismatch.

### D4f — Failure retains, and does not spin

At-least-once must not become a poison-event loop. A delivery that traps or
produces invalid output faults the instance, retains the pending delivery, and
performs **no immediate retry in that instance**; redelivery happens after a
controlled restart or recovery. Bounded backoff is deliberately deferred.

Once a delivery is pending, a `Due` schedule is no longer pausable, resumable,
or cancellable as though running. A reset creates a new generation and
transactionally retires any pending delivery belonging to the replaced
generation. A delivery already committed into an application turn cannot be
retroactively cancelled.

### D4g — Due detection does not require a guest

Marking a schedule due and making its delivery durable must work with **no live
instance** — requiring Wasmtime instantiation to notice a deadline would weaken
the D1 property B-2 established. Guest availability determines only when a
pending delivery may be *consumed*: with no instance the delivery is retained;
with one available, transactional delivery is attempted. Whether Youth
auto-mounts an unloaded application is a separate policy, deferred.

### D5 — Delivery is at-least-once, acknowledged by commit

A due schedule queues a durable pending delivery. The guest handles it in an
ordinary transactional turn, and the delivery is acknowledged **only after that
turn commits**. A trap, rejection, or commit failure leaves it pending for
redelivery. Guest handlers must be idempotent by schedule identity and
generation. Youth does not promise exactly-once delivery.

### D6 — Notification intent attaches to the schedule

Per `TIMER-F006`, a bounded notification descriptor is supplied at schedule
creation and persisted with it, so the host can attempt the notification when the
deadline passes **without** depending on a guest turn succeeding first.
Notification remains best-effort and strictly separate from durable elapsed
delivery; its failure never affects whether the schedule elapsed.

## Contract sketch

```wit
package youth:time@0.0.1;

interface scheduler {
    type schedule-id = u64;
    type generation = u64;

    record schedule {
        id: schedule-id,
        generation: generation,
    }

    record notification {
        title: string,
        body: string,
    }

    record schedule-options {
        notification: option<notification>,
    }

    enum schedule-error-code {
        invalid-duration,
        too-many-schedules,
        unknown-schedule,
        stale-generation,
        invalid-state,
        unavailable,
        internal,
    }

    schedule-after: func(millis: u64, options: schedule-options)
        -> result<schedule, schedule-error-code>;
    pause:  func(value: schedule) -> result<_, schedule-error-code>;
    resume: func(value: schedule) -> result<_, schedule-error-code>;
    cancel: func(value: schedule) -> result<_, schedule-error-code>;
}
```

The `0.0.4` application world adds the import and one event kind:

```wit
variant event-kind {
    activate(node-id),
    schedule-elapsed(elapsed-schedule),
}
```

Bounds, validated host-side and returned as errors rather than traps:

```text
maximum active schedules per app:  32
minimum duration:                 100 ms
maximum duration:              30 days
notification title:               256 bytes
notification body:              1 KiB
one pending delivery per schedule generation
```

## Gate B scope and sequence

Gate B proves schedule correctness **headlessly**, before any pixel or
notification work. Ordered so nothing is built on an unsettled assumption:

| Step | Content |
| --- | --- |
| B-1 | `youth:time@0.0.1` WIT, protocol `0.0.4` alongside `0.0.3`/`0.0.2`, host `Host` impl, SDK `context.time()`, import allowlist, project protocol set (D4) |
| B-2 | Durable schedule storage (D1), host-issued identity and generation (D2), transaction-bound create/pause/resume/cancel, schema migration |
| B-3 | Clock seam (D3) including the guest monotonic override, scheduler state machine, stale-wake rejection, restart and overdue reconciliation, deterministic headless tests |
| B-4 | Host-initiated elapsed delivery (D5) — worker wake path, fire-and-forget command, unsolicited receipt publication — settling `TIMER-F011`'s ownership half, plus `.youth-test` virtual-clock commands |

Gate C (countdown presentation, desktop deadline wakeups) and Gate D
(notifications, release evidence) follow and are out of Gate B scope.

## Definition of done for Gate B

- A guest can arm, pause, resume, and cancel a bounded schedule through
  `context.time()` with no WIT, ID, or generation machinery visible in
  application source.
- A schedule armed in a turn that later fails does not exist after that turn.
- A schedule survives process restart and is visible to a host that has not
  instantiated the guest.
- A stale generation's wake is rejected by the host before any guest invocation.
- An overdue schedule produces exactly one pending delivery on the next start.
- A failed elapsed turn leaves the delivery pending; a committed one acknowledges it.
- Headless tests advance a virtual clock and never wait on real time, including
  when the guest itself reads `Instant::now()`.
- `0.0.2` and `0.0.3` components continue to load, mount, and run unchanged.

## Non-goals

Calendar alarms, time zones, cron schedules, arbitrary recurring subscriptions,
guest-visible wall clocks, millisecond animation timers, process-independent
system alarms, a background daemon, notification actions, and multiple
simultaneous timers unless implementation evidence demands them.

## Gate C-3: host-owned countdown presentation

Gate B proved the schedule itself — identity, durability, elapsed delivery.
Gate C-2 wired an application to it. What remained open (`TIMER-F004`) was
display: a countdown that changes every second cannot be guest-rendered
without a guest turn per tick, and Youth has no periodic-tick capability —
deliberately, per the Gate B non-goals above. Gate C-3 closes this the way
the rest of the platform is shaped: the guest declares temporal *meaning*,
the host owns the clock read, the formatting cadence, and the repaint.

### D7 — A countdown is a declared reference, not a guest-computed string

A text node's content is either a literal string the guest sets, or a
reference to a schedule the guest owns, with a precision and a format. The
host resolves the reference to a display string **at presentation time**,
not at turn-commit time — the committed tree stores the reference, never a
computed value, so nothing about the display depends on when the guest last
ran.

```wit
record schedule-ref {
    id: u64,
    generation: u64,
}

enum time-precision {
    seconds,
}

enum countdown-format {
    minutes-seconds,
}

record countdown-data {
    schedule: schedule-ref,
    precision: time-precision,
    format: countdown-format,
}

variant text-content {
    literal(string),
    countdown(countdown-data),
}

record text-data {
    content: text-content,
    alignment: text-alignment,
}
```

`set-text`'s value becomes `text-content`, so a guest can retarget a node
between a literal string and a countdown reference across a mode change
(Timer's own need: literal `"05:00"` while Idle, `countdown(handle)` while
Running or Paused) — but a guest can never *supply* the displayed digits for
a countdown node; only which schedule, at what precision and format.

This is a wire-format change to `text-data`, so it ships as a new protocol
world, `youth:app@0.0.5`, alongside `0.0.4`/`0.0.3`/`0.0.2` unchanged —
following the same multi-version dispatch as every prior protocol bump.
Everything else in the `0.0.4` tree (layout, buttons, patches other than
`set-text`, events, the `youth:time@0.0.1` import) carries over unchanged.

### D7a — Redraw is presentation-only: no turn, no patch, no `TurnOutcome`

Recomputing a countdown's digits is **not** a guest turn. It does not go
through the worker mailbox, does not read or write durable state, produces
no patch, and commits nothing. It is the same category of operation as
painting a window: the host reads its own already-durable schedule record
(the same `DeadlineClock`-backed storage Gate B built), formats, and draws.
A build that routes countdown redraw through `WorkerMessage`, a guest
`handle` call, or a `TurnOutcome` has misread this design — that path exists
for `ScheduleElapsed`, which is a distinct, rarer, and genuinely
guest-meaningful event.

### D7b — Repaint is scheduled at the next display boundary, not on a loop

The host does not poll. Given a running schedule's deadline, the next
instant the *displayed* value will change is computable exactly (the next
whole-second boundary of the remaining duration, under D7c's rounding). The
desktop event loop arms exactly one wake for that instant
(`ControlFlow::WaitUntil`, replacing the idle `ControlFlow::Wait` in
`crates/youth-desktop/src/native.rs`) and recomputes on firing, arming the
next one. A paused schedule's remaining value is frozen and arms no wake at
all — nothing will change until `resume`, which is a guest-turn boundary
and repaints on its own.

### D7c — Rounding, due, and unavailable are fixed, not app-configurable

- Running: `display = ceil(remaining_ms / 1000)` seconds, formatted per
  `countdown-format` — `1.1s → 00:02`, `0.1s → 00:01`, never `00:00` while
  time genuinely remains.
- Due (`remaining_ms <= 0`): `00:00`, exactly, whether or not the
  `ScheduleElapsed` turn has yet been delivered and committed — the display
  and the durable elapsed event are independent observers of the same
  deadline, not sequenced against each other.
- Paused: the host-owned frozen remainder at the moment of `pause`,
  unrounded-surprises aside identical to the running rounding rule.
- Missing, cancelled, or generation-stale (the schedule moved on after this
  tree was installed, e.g. a concurrent `pause`/`resume`/`cancel`): a fixed
  unavailable glyph (`--:--`), never a stale or fabricated number, and never
  a render-time error.

### D7d — Reference validity is checked once, at install; staleness after is a display fallback, not a rejection

When a turn's patch batch is applied and about to be installed as the new
tree, every `countdown(schedule-ref)` it introduces is resolved against
this application's live schedule store. A reference to an id/generation the
app does not currently own — fabricated, mistyped, or belonging to another
app — is rejected as an application error before install, the same
integrity boundary as a stale-generation `pause`/`resume` call. This is a
one-time gate at commit time, not a standing invariant the renderer
re-checks: once installed, a **legitimately-created** reference that later
goes stale (the schedule elapses, gets cancelled, or its generation moves)
is D7c's unavailable-display case, not a reason to fail a redraw or force a
new guest turn. Multiple countdown nodes may reference the same schedule
with no uniqueness constraint.

### Gate C-3 scope and sequence

| Step | Content |
| --- | --- |
| C3-1 | `youth:time`-adjacent WIT: new `youth:app@0.0.5` world with `text-content`/`countdown-data`/`schedule-ref` (D7), `youth-tree` snapshot/patch/canonical-output support for the variant, runtime protocol dispatch and `youth-project` contract profile entry for `0.0.5` |
| C3-2 | SDK surface (`Countdown` builder, `TimePrecision`, `CountdownFormat`, `Update::set_countdown`), install-time reference validation (D7d) |
| C3-3 | Pure, host-testable display-resolution function (D7c) decoupled from windowing; `crates/youth-desktop` wiring to `ControlFlow::WaitUntil` (D7b); the decisive virtual-clock test: advancing to one second before due produces zero guest turns, advancing across due produces exactly one autonomous `ScheduleElapsed` turn |

### Definition of done for Gate C-3

- A countdown node's displayed value never requires a guest turn to change.
- Advancing a virtual clock across many display-boundary seconds produces
  zero guest turns; crossing the deadline produces exactly one.
- A paused countdown's display is stable and arms no repaint.
- A missing/cancelled/stale-generation reference renders `--:--`, never a
  stale number, a crash, or a rejected redraw.
- A fabricated or foreign schedule reference is rejected at turn-commit,
  before install — never silently accepted or silently displayed.
- `0.0.2`/`0.0.3`/`0.0.4` components continue to load, mount, and run
  unchanged; their text nodes have no countdown capability and none is
  implied for them.
