# Youth Utility Suite Findings

This is the central index for evidence produced by applications that probe
Youth's platform boundary. Full findings live with the application that
produced them. An entry is evidence, not automatic authorization for a feature.

## Applications

| App | Repository | Findings prefix | Status |
| --- | --- | --- | --- |
| Calculator | `/Users/keina/dev/youth-calculator` until publication | `CALC-F` | Gate A complete |

## Open findings

| ID | Summary | Owner | Next decision |
| --- | --- | --- | --- |
| CALC-F001 | Protocol `0.0.2` cannot express calculator presentation or keyboard intent | Calculator / Youth | Add only the layout, alignment, and logical-shortcut semantics proved by app commit `e971316` |

## Accepted evidence

| ID | App commit | Observation | Platform conclusion |
| --- | --- | --- | --- |
| CALC-F001 | `e971316` | A correct, persistent calculator becomes one vertical sequence of nineteen controls | Layout and alignment intent must cross protocol; exact geometry remains host policy |
| CALC-F002 | `e971316` | View IDs and activation matching repeat for every command; canonical model persistence expands into typed calls | View-backed command identity belongs in the SDK; structured state remains unproven |
| CALC-F003 | `e971316` | Calculator source contains no generated bindings, numeric node IDs, revisions, acknowledgements, or patches | Preserve the DP0 SDK boundary through protocol `0.0.3` |

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
