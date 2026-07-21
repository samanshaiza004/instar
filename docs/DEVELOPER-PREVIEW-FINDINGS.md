# Developer Preview Findings

This is the durable evidence log for building applications outside the Youth
repository. It is not an authoritative protocol specification and does not
automatically authorize new features.

## Open findings

| ID | Summary | Status | Tooling implication |
| --- | --- | --- | --- |
| DP0-F001 | A guest currently owns all protocol plumbing | Addressed | `youth-sdk` owns bindings, lifecycle, state, and wire conversion |
| DP0-F002 | Symbolic-ID prefix notation was ambiguous | Addressed | Lock exact bytes through canonical vectors in every SDK |

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

- **Status:** Addressed
- **Observed:** 2026-07-21
- **Application:** Milestone 1 counter, before external Tally
- **Workflow stage:** Manual guest authoring
- **Platform:** Platform-independent source inspection
- **Local path:** `/Users/keina/dev/youth-tally`
- **Commit:** Tally `74ccae5`; Youth `c5e2b0f`; migrated Tally `2f2f5a7`
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
- **Resolution:** The standalone repository first proved the raw component at
  `74ccae5`, then `youth-sdk` commit `c5e2b0f` replaced 190 lines of app-owned
  plumbing. Tally `2f2f5a7` builds from that exact Git revision with no path
  dependency. SDK unit tests, component validation, runtime persistence, and
  restart/resync integration tests pass.

### DP0-F002 — Symbolic-ID prefix notation was ambiguous

- **Status:** Addressed
- **Observed:** 2026-07-21
- **Application:** SDK reference tests
- **Workflow stage:** Symbolic node ID implementation
- **Platform:** Platform-independent
- **Local path:** `crates/youth-sdk/src/lib.rs`
- **Commit:** `c5e2b0f`
- **Evidence:** The notation `youth:node-id:v1\0` can mean a NUL escape in
  source code or the literal ASCII bytes backslash and zero. The approved
  canonical vectors correspond to the literal two-byte suffix.
- **Developer impact:** SDK implementations in different languages could map
  the same symbolic name to different wire IDs.
- **Decision:** The canonical vectors are authoritative. The domain suffix is
  byte `0x5c` followed by byte `0x30`, and the contract now says so explicitly.
- **Tooling implication:** Every SDK and host-side test runner must run the same
  three canonical vectors.
- **Resolution:** SDK unit tests lock `count`, `increment`, and `café`.
