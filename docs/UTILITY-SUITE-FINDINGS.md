# Youth Utility Suite Findings

This is the central index for evidence produced by applications that probe
Youth's platform boundary. Full findings live with the application that
produced them. An entry is evidence, not automatic authorization for a feature.

## Applications

| App | Repository | Findings prefix | Status |
| --- | --- | --- | --- |
| Calculator | [samanshaiza004/youth-calculator](https://github.com/samanshaiza004/youth-calculator) | `CALC-F` | Gate D complete; one canonical component certified on Ubuntu, Windows, and macOS |
| Timer | [samanshaiza004/youth-timer](https://github.com/samanshaiza004/youth-timer) | `TIMER-F` | Gate A app proof published; probes `youth:time` scope ahead of any implementation |

## Open findings

| ID | Category | Summary | Owner | Next decision |
| --- | --- | --- | --- | --- |
| CALC-F002 | Platform discovery | Command binding and canonical state calls are repetitive | Calculator / Youth | Resolve view-backed commands in the SDK; retain explicit typed durable state |
| CALC-F009 | Platform discovery | Explicit updates can diverge from reconstructed view output | Calculator / Youth | Gather evidence from more dynamic applications before choosing explicit patches, SDK diffing, or reactive dependencies |
| TIMER-F001 | Platform discovery | No host-owned temporal capability: a countdown cannot represent or present time-dependent state at all | Timer / Youth | Design declarative scheduling + host-owned temporal presentation; explicitly not a guest-visible `now()` |
| TIMER-F002 | Platform discovery | No host-initiated application turn; only `activate(node-id)` exists | Timer / Youth | Design host-initiated delivery (schedule-elapsed event kind); distinct gap from TIMER-F001, not resolved by it |
| TIMER-F005 | Platform discovery | A `handle`-only `enabled` omission produced a real, reproduced retained-tree divergence (keyboard shortcut silently stopped resolving) | Timer / Youth | Materially strengthens CALC-F009; build a `--verify-view-convergence` test mode in `crates/youth-test` (design proposed in Timer's `FINDINGS.md`) before considering SDK diffing or reactive dependencies |
| TIMER-F006 | Platform discovery | Notification delivery design choice: guest-requested-after-elapse vs. schedule-attached descriptor | Timer / Youth | Recommends attaching a bounded notification descriptor to the schedule at creation, not a general effects API; decide before Gate C |
| TIMER-F008 | Platform discovery | Application session counters and host schedule delivery identity must not be conflated | Timer / Youth | `youth:time` schedule generation must be host-issued and checked by the host, never guest-invented |

## Accepted evidence

| ID | Category | App evidence | Observation | Platform conclusion |
| --- | --- | --- | --- | --- |
| CALC-F001 | Platform discovery | `e971316`; `ba5f00c` | A correct, persistent calculator becomes one vertical sequence of nineteen controls under DP0 | Addressed by bounded row/grid/alignment/shortcut semantics in protocol `0.0.3`; exact geometry remains host policy |
| CALC-F002 | Platform discovery | `e971316` | View IDs and activation matching repeat for every command; canonical model persistence expands into typed calls | View-backed command identity belongs in the SDK; structured state remains unproven |
| CALC-F003 | Boundary confirmation | `e971316` | Calculator source contains no generated bindings, numeric node IDs, revisions, acknowledgements, or patches | Preserve the DP0 SDK boundary through protocol `0.0.3` |
| CALC-F004 | Platform discovery | `utility-calculator-gate-b-layout` | Supporting `0.0.2` and `0.0.3` directly in presentation would duplicate policy and destabilize old fixtures | Both worlds normalize at the runtime boundary; tree, interaction, and renderer code are version-independent |
| CALC-F005 | Tooling defect | `ba5f00c` external `youth test` on the first aligned display | The test runner matched the compact `Text` representation instead of the semantic text role | Assertions now use normalized text accessors, so alignment does not change test meaning |
| CALC-F006 | Tooling defect | `ba5f00c` fresh Cargo resolution of the Git-pinned SDK | An invalid package-name placeholder in the embedded template produced a noisy repository-discovery diagnostic | Reviewable templates must remain valid source artifacts; generation now replaces a valid sentinel and the template directory is excluded from the workspace |
| CALC-F007 | Platform discovery | `8ef8e40` calculator keyboard acceptance test | Node activation alone could not prove host-owned focus and shortcut policy through the real runtime | The narrow test DSL now drives logical keys and asserts semantic focus without exposing native key codes or sending keyboard events to the guest |
| CALC-F008 | Platform discovery | `8ef8e40` native calculator presentation; Youth `2761e35` | The provisional renderer displayed unsupported ASCII punctuation (`+`, `/`, `*`, `.`, `=`) as `?` | Addressed with deterministic coverage for all printable ASCII and representative raster fixtures; broader Unicode text remains a separate pre-editor requirement |
| CALC-F009 | Platform discovery | `8ef8e40` `view`, `handle`, and restart test | One shared formatter prevents duplicated display logic, but the app still names the affected node and its patch can diverge from a reconstructed view | Convergence is an intended invariant but is not generally enforced; retain explicit patches for DP1 and collect evidence before selecting SDK tree diffing or reactive dependencies |
| CALC-F010 | Tooling defect | Gate D CI on Ubuntu, Windows, and macOS at calculator `fe962a0` | Independently built components all passed functional gates but produced different bytes, so comparing their hashes did not prove the intended same-component portability claim | Build one canonical component once and mount those exact bytes on every host; retain host-local builds as separate source-portability evidence |
| TIMER-F001 | Platform discovery | `ab65ec3`/`3ecc342` WIT and SDK inspection | No time, clock, or duration type exists anywhere in the guest-facing contract; the absence of a guest clock is architecturally intentional, not an oversight | `youth:time` should provide declarative scheduling and host-owned temporal presentation; adding `now()` would resolve the letter of this finding while missing its point |
| TIMER-F003 | Platform discovery | `3ecc342` `src/model.rs` | `remaining_seconds` must be tracked and persisted as ordinary guest state because no host schedule exists to reconstruct it from | Temporary Gate A scaffolding with an explicit deletion path recorded, not an alternative production path to preserve alongside a future schedule |
| TIMER-F005 | Platform discovery | `ab65ec3` first real `youth test` run | A `handle` that set only display text (correctly) left button `enabled` state stale on the retained tree, so a declared keyboard shortcut silently resolved to nothing | Turns CALC-F009 from a predicted risk into a reproduced divergence bug; justifies a convergence-checker test mode, not yet SDK tree diffing or reactive dependencies |
| TIMER-F009 | Boundary confirmation | `ab65ec3` 22 unit tests plus one full `.youth-test` scenario, zero raw WIT concepts | The mode machine, bounded configuration, session counting, eleven distinct commands with shortcuts, durable persistence, and restart recovery all built without any protocol change | The next `youth:time` work should stay narrow — scheduling, presentation, delivery, reconciliation, notification — and should not touch application lifecycle, persistence API, or command system |
| TIMER-F011 | Platform discovery | Youth `97d7b3b` Gate B | A durable schedule had no representation outliving one spawned app instance | Closed: schedule creation, persistence, due detection, wake validation, pending delivery, and acknowledgement are host-owned; the guest owns only its transactional reaction. Auto-mount of an unloaded app remains deferred policy (DP2 D4g) |
| TIMER-F010 | Boundary confirmation | `ab65ec3` `tests/basic.youth-test` paused-advance case | `activate advance-1s` reaches the guest's `handle` while the button is presented disabled; `crates/youth-interaction`'s `enabled_buttons` filtering is not applied to direct activation | `enabled` is presentation policy only, not access control; every guest must independently validate command preconditions. The test DSL's `activate` should eventually be documented (or renamed) as direct semantic injection, distinct from a future interaction-policy-respecting `click` |

## Classification

- **Platform discovery:** Evidence about protocol, SDK, host policy, renderer,
  or application-authoring boundaries. It may justify a narrowly scoped platform
  change after the owning layer is identified.
- **Tooling defect:** A generator, build, test, or diagnostic bug. Fixing it
  does not imply a runtime or protocol capability.
- **Boundary confirmation:** Evidence that an existing abstraction continues to
  hide the intended lower-level machinery under a more demanding application.

## Finding requirements

Every application finding records:

- date, app, command or workflow stage, platform, local path, and commit;
- reproducible observation and evidence;
- developer impact and status: open, addressed, or deferred;
- what could not be expressed;
- what felt repetitive;
- what leaked WIT, lifecycle, revision, ID, patch, or host details;
- what required host policy;
- which protocol addition was unavoidable, if any;
- what can remain application or SDK behavior;
- performance, size, portability, or authoring impact; and
- tests and commits that resolved it.

Application repositories assign stable IDs and update a finding in the same
commit that addresses or defers it. This index links the accepted conclusion
without duplicating the canonical evidence.
