# Developer Preview Findings

This is the durable evidence log for building applications outside the Youth
repository. It is not an authoritative protocol specification and does not
automatically authorize new features.

## Open findings

| ID | Summary | Status | Tooling implication |
| --- | --- | --- | --- |
| DP0-F001 | A guest currently owns all protocol plumbing | Open | Extract `youth-sdk` from the external Tally experience |

## Finding template

### DP0-Fxxx — Short title

- **Status:** Open / Addressed / Deferred
- **Observed:** YYYY-MM-DD
- **Application:** Name
- **Workflow stage:** Command or manual step
- **Platform:** OS and architecture
- **Local path:** Implementation-local path, when relevant
- **Commit:** Commit or `uncommitted`
- **Evidence:** Reproduction and concrete observation
- **Developer impact:** What the application author must understand or do
- **Decision:** What Youth will do, or why it is deferred
- **Tooling implication:** Reusable tooling suggested by the evidence
- **Resolution:** Tests, commits, or documents that close the finding

## Findings

### DP0-F001 — A guest currently owns all protocol plumbing

- **Status:** Open
- **Observed:** 2026-07-21
- **Application:** Milestone 1 counter, before external Tally
- **Workflow stage:** Manual guest authoring
- **Platform:** Platform-independent source inspection
- **Local path:** `guests/counter/src/lib.rs`
- **Commit:** `599fb93`
- **Evidence:** The guest invokes `wit_bindgen::generate!`, imports generated
  modules, assigns raw `u64` node IDs, tracks mount and revision state, creates
  wire snapshots and patch batches, acknowledges event sequences, converts
  state errors, and calls the generated export macro.
- **Developer impact:** An app author must understand the host protocol and
  Component Model plumbing before expressing one button and one text value.
- **Decision:** Build one external Tally with this plumbing, then extract the
  minimum explicit SDK API from observed friction.
- **Tooling implication:** `youth-sdk` must own bindings, exports, lifecycle,
  revisions, acknowledgements, typed state, builders, and wire conversion.
- **Resolution:** Pending Gate A external Tally and SDK tests.

