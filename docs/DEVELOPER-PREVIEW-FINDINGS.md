# Developer Preview Findings

This is the durable evidence log for building applications outside the Youth
repository. It is not an authoritative protocol specification and does not
automatically authorize new features.

## Open findings

| ID | Summary | Next decision |
| --- | --- | --- |
| DP0-F008 | GitHub forces legacy action runtimes onto Node.js 24 | Review supported action-major upgrades separately from the CI correctness fix |

## Findings index

| ID | Summary | Status | Tooling implication |
| --- | --- | --- | --- |
| DP0-F001 | A guest currently owns all protocol plumbing | Addressed | `youth-sdk` owns bindings, lifecycle, state, and wire conversion |
| DP0-F002 | Symbolic-ID prefix notation was ambiguous | Addressed | Lock exact bytes through canonical vectors in every SDK |
| DP0-F003 | A shallow template copy omitted nested WIT | Addressed | Hash and generate the complete recursive snapshot |
| DP0-F004 | Supervisor proof should not depend on presentation | Addressed | Run process tests headlessly and native smoke separately |
| DP0-F005 | Generated artifact names follow the destination | Addressed | Generate CI fixtures into the documented `tally` directory |
| DP0-F006 | CI evidence must not pollute or exhaust the workspace | Addressed | Use runner-temporary evidence, one feature-complete test build, and reduced debug artifacts |
| DP0-F007 | Artifact metadata leaked into the checkout | Addressed | Pass digests as job outputs and distribute only ignored test fixtures |
| DP0-F008 | GitHub forces legacy action runtimes onto Node.js 24 | Open | Keep action-runtime compatibility warnings visible and review upgrades explicitly |

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
  Tally `5191d0d` then aligns every tracked project file byte-for-byte with the
  parameter-resolved generator template.

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

### DP0-F003 — A shallow template copy omitted nested WIT

- **Status:** Addressed
- **Observed:** 2026-07-21
- **Application:** Tally
- **Workflow stage:** `youth check`
- **Platform:** macOS aarch64
- **Local path:** `/Users/keina/dev/youth-tally`
- **Commit:** Tally `af7a093`; Youth `c607bf4`
- **Evidence:** An initial file listing capped at depth four found
  `wit/youth/youth-app.wit` but omitted the deeper
  `wit/youth/deps/youth-state/store.wit`. A generated snapshot containing only
  the application WIT produced a different digest and was not the complete
  inspectable contract.
- **Developer impact:** A generated project could pass Rust compilation through
  SDK-owned bindings while shipping an incomplete language-neutral snapshot.
- **Decision:** Generation includes every recursive regular file below
  `wit/youth`. The complete Tally snapshot retains the original digest
  `71ca278ff0dbf618dcb9ad174e0843f9c397cbe38742b1aec262689b913d4e3c`.
- **Tooling implication:** All commands call `youth-project::hash_wit_tree`;
  no command assembles the digest independently.
- **Resolution:** The embedded template includes the nested state WIT and its
  test materializes both files before checking the locked digest. Project hash
  tests still prove that content and path mutations change it.

### DP0-F004 — Supervisor proof should not depend on presentation

- **Status:** Addressed
- **Observed:** 2026-07-21
- **Application:** Generated Tally
- **Workflow stage:** `youth dev --headless-supervisor`
- **Platform:** macOS aarch64
- **Local path:** `/private/tmp/youth-dev.Nc6tor/tally`
- **Commit:** `74b4fe3`
- **Evidence:** The headless child mounted, a valid source edit rebuilt and
  restarted it, an invalid Rust edit failed while the prior child stayed live,
  and a corrected edit restarted successfully. Ctrl-C then stopped and reaped
  the child through the same bounded stdin-shutdown path used by desktop dev.
- **Developer impact:** Rebuild/process regressions can otherwise be mistaken
  for hosted-window failures, especially on macOS and Windows CI runners.
- **Decision:** Keep watcher, rebuild, retention, shutdown, and state-root
  evidence presentation-independent. Test native window creation separately;
  reserve the combined source-edit/window E2E for Ubuntu/Xvfb until other
  hosted displays are stable.
- **Tooling implication:** `youth dev` has an internal headless supervisor mode
  and its child has a private stdin shutdown protocol; neither changes the app
  protocol or public guest API.
- **Resolution:** Watch-input unit tests, cross-platform headless-child tests,
  the manual valid/invalid/recovery exercise, and split CI gates cover it.

### DP0-F005 — Generated artifact names follow the destination

- **Status:** Addressed
- **Observed:** 2026-07-21
- **Application:** Generated Tally
- **Workflow stage:** Ubuntu source-edit and restart E2E
- **Platform:** Ubuntu x86-64 GitHub-hosted runner
- **Local path:** `$RUNNER_TEMP/youth-external-tally`
- **Commit:** `ci: make developer preview gates deterministic`
- **Evidence:** `youth new` correctly derives the Cargo package from the
  destination basename. CI generated `youth-external-tally`, producing
  `youth_external_tally.wasm`, while the later E2E invoked the nonexistent
  `target/wasm32-wasip2/debug/tally.wasm` and failed before presentation.
- **Developer impact:** Scripts that guess a component name independently of
  the project contract can fail even though `youth check`, `test`, and `build`
  all succeed.
- **Decision:** Make the external CI fixture mirror the documented workflow by
  generating a directory whose basename is exactly `tally`.
- **Tooling implication:** Project-oriented commands remain the authority for
  package resolution; direct artifact access in narrow integration tests must
  use a fixture with an intentionally fixed package name.
- **Resolution:** The external workflow and Ubuntu E2E now share
  `$RUNNER_TEMP/youth-external/tally`, locking the expected `tally.wasm` name.

### DP0-F006 — CI evidence must not pollute or exhaust the workspace

- **Status:** Addressed
- **Observed:** 2026-07-21
- **Application:** Youth host matrix
- **Workflow stage:** Canonical snapshots, test failpoints, release builds, and
  final clean-tree verification
- **Platform:** Ubuntu, Windows, and macOS GitHub-hosted runners
- **Local path:** Youth workspace and `$RUNNER_TEMP`
- **Commit:** `ci: make developer preview gates deterministic`
- **Evidence:** The canonical snapshot gate wrote `three-clicks.actual` and
  `three-clicks.expected` into the repository, making the final clean-tree gate
  fail on every host. A separate feature-enabled transaction test build and a
  second desktop release invocation also duplicated large Wasmtime artifacts;
  one Ubuntu run reported disk exhaustion while compiling them.
- **Developer impact:** Valid code can appear broken when evidence files leak
  into source control scope or redundant compiler profiles exceed hosted-runner
  storage.
- **Decision:** Write transient evidence beneath `RUNNER_TEMP`, run the full
  workspace test suite once with transaction failpoints enabled, remove the
  redundant desktop release invocation, disable incremental compilation, and
  omit debug information from CI dev/test artifacts.
- **Tooling implication:** CI gates should share compiler profiles and treat
  workspace cleanliness and resource use as explicit correctness constraints.
- **Resolution:** The workflow now prints clean-tree diagnostics, keeps
  canonical hash inputs outside the checkout, and reduces duplicate artifacts.

### DP0-F007 — Artifact metadata leaked into the checkout

- **Status:** Addressed
- **Observed:** 2026-07-21
- **Application:** Youth host matrix
- **Workflow stage:** Artifact download and final clean-tree verification
- **Platform:** Ubuntu, Windows, and macOS GitHub-hosted runners
- **Local path:** Youth workspace root
- **Commit:** `ci: keep guest metadata out of checkout`
- **Evidence:** The guest job created `counter.sha256` beside the repository,
  uploaded it with the Wasm fixtures, and every host downloaded it into the
  checkout. The final gate consequently reported `?? counter.sha256` even
  though all functional checks passed.
- **Developer impact:** A build artifact can invalidate source-tree hygiene
  merely because its extraction location overlaps the checkout.
- **Decision:** Do not create or distribute a digest file. Send the digest
  through the existing GitHub Actions job output and keep the artifact limited
  to Wasm fixtures. Download those fixtures explicitly beneath the ignored
  `target/` tree because runtime integration tests consume those exact paths.
- **Tooling implication:** CI metadata belongs in action outputs or
  runner-temporary storage; only test inputs with deliberate ignored
  destinations may be downloaded into the checkout.
- **Resolution:** The hash step now computes one shell value, publishes it as
  the guest job output, and logs it without writing `counter.sha256`. The host
  download action extracts the Wasm-only artifact directly into
  `target/wasm32-wasip2/release`.

### DP0-F008 — GitHub forces legacy action runtimes onto Node.js 24

- **Status:** Open
- **Observed:** 2026-07-21
- **Application:** Youth host matrix
- **Workflow stage:** Action setup and artifact transfer
- **Platform:** Ubuntu, Windows, and macOS GitHub-hosted runners
- **Local path:** `.github/workflows/ci.yml`
- **Commit:** `598d9ba`
- **Evidence:** GitHub annotates `actions/checkout@v4` and the v4 artifact
  actions because their declared Node.js 20 runtime is deprecated and the
  hosted service is forcing execution on Node.js 24.
- **Developer impact:** The workflow still runs, but future hosted-runner
  enforcement could turn the warning into a setup failure.
- **Decision:** Keep the warning open. Review supported major upgrades as a
  separate dependency change after the correctness regression is green; do not
  mix unverified action upgrades into the artifact-path repair.
- **Tooling implication:** CI action runtimes need the same explicit upgrade
  review as Rust dependencies and toolchain pins.
- **Resolution:** Pending a focused compatibility upgrade and green matrix.
