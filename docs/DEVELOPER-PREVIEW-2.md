# Developer Preview 2 — Durable Scheduling

- Status: Proposed
- Application protocol: `youth:app@0.0.4`, with runtime compatibility for `0.0.3` and `0.0.2`
- Capability protocol: `youth:time@0.0.1`
- State protocol: `youth:state@0.0.1` (schema version 2)
- Driving application: [Youth Timer](https://github.com/samanshaiza004/youth-timer)
- Evidence: `TIMER-F001`, `TIMER-F002`, `TIMER-F003`, `TIMER-F006`, `TIMER-F008`, `TIMER-F011`

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

### D2 — Schedule identity and generation are host-issued

Per `TIMER-F008`, a guest-invented counter cannot be trusted to reject a wake the
guest itself did not generate. `schedule-after` returns an **opaque** identity
and generation assigned by the host and checked by the host before any guest is
invoked. A guest may durably store and read that identity back; it may not
construct one. An application's own session counter (the Timer's
`completed_sessions`) remains guest-owned domain data and must not be conflated
with a schedule generation.

### D3 — One clock seam drives both the scheduler and the guest's clock

A `Clock` abstraction (`SystemClock`, `VirtualClock`) is introduced and threaded
through `YouthAppConfig`, which is constructed in exactly four places.

Critically, the same seam must back the **guest's** monotonic clock via
`WasiCtxBuilder::monotonic_clock(..)`. `wasi:clocks/monotonic-clock` cannot
simply be removed from the import allowlist — Rust `std` links it
unconditionally, so every existing component imports it and would fail
validation. Overriding it is both possible and correct: under a virtual clock,
a guest calling `Instant::now()` observes virtual time, and headless tests become
genuinely time-hermetic end to end rather than hermetic only up to the SDK
boundary.

The Wasmtime **epoch** thread is explicitly *not* reused or virtualized. It
exists to preempt runaway guests, its 10 ms tick has no wall-clock meaning, and
virtualizing it would invalidate the containment tests that assert an infinite
loop dies in bounded real time.

### D4 — The project contract accepts a set of protocols

`youth-project` currently compares an application manifest's `protocol` against a
single `SUPPORTED_PROTOCOL`. With two published applications this breaks: the
calculator declares `0.0.3` and would be rejected the moment the host moves to
`0.0.4`, failing the external-calculator CI job.

`SUPPORTED_PROTOCOL` becomes a supported **set** with a distinguished default for
newly generated projects, mirroring how the runtime already accepts multiple
protocols. This is a direct consequence of having a second Utility Suite
application and should be recorded as such.

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
