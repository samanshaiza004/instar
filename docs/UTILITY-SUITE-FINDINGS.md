# Youth Utility Suite Findings

This is the central index for evidence produced by applications that probe
Youth's platform boundary. Full findings live with the application that
produced them. An entry is evidence, not automatic authorization for a feature.

## Applications

| App | Repository | Findings prefix | Status |
| --- | --- | --- | --- |
| Calculator | sibling repository; local path recorded after creation | `CALC-F` | In progress |

## Open findings

| ID | Summary | Owner | Next decision |
| --- | --- | --- | --- |
| CALC-F001 | Protocol `0.0.2` cannot express calculator presentation or keyboard intent | Calculator / Youth | Prove exact gaps with the DP0 SDK before protocol work |

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
