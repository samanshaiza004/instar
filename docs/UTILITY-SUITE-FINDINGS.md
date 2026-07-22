# Youth Utility Suite Findings

This is the central index for evidence produced by applications that probe
Youth's platform boundary. Full findings live with the application that
produced them. An entry is evidence, not automatic authorization for a feature.

## Applications

| App | Repository | Findings prefix | Status |
| --- | --- | --- | --- |
| Calculator | `/Users/keina/dev/youth-calculator` until publication | `CALC-F` | Gate C complete |

## Open findings

| ID | Category | Summary | Owner | Next decision |
| --- | --- | --- | --- | --- |
| CALC-F002 | Platform discovery | Command binding and canonical state calls are repetitive | Calculator / Youth | Resolve view-backed commands in the SDK; retain explicit typed durable state |
| CALC-F009 | Platform discovery | Explicit updates can diverge from reconstructed view output | Calculator / Youth | Gather evidence from more dynamic applications before choosing explicit patches, SDK diffing, or reactive dependencies |

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
