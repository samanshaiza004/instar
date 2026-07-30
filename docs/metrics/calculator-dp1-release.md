# Calculator DP1 release baseline

This is the first Utility Suite release baseline for the calculator. The raw
record is [calculator-dp1-macos-arm64.json](calculator-dp1-macos-arm64.json)
and follows metrics schema v1.

The local measurements were captured on 2026-07-22 with Youth commit
`c80244eb67f16341675c230e5790ef210feab2cd`, calculator commit
`696d2fbac58a99116fec28238ca51f5a4a5c2acd`, Rust `1.97.1`, Wasmtime `46.0.1`,
macOS `26.5.2`, arm64. The release component is 124,589 bytes with SHA-256
`9ea0c22b89a7121c41d595fac10170871bb865fafa96f8df6222ee0eb08e2bd4`.

That hash identifies the locally measured macOS build. The machine-readable
canonical artifact record is
[`calculator-dp1-canonical-artifact.json`](calculator-dp1-canonical-artifact.json).
Gate D CI run
[`30504489792`](https://github.com/samanshaiza004/youth/actions/runs/30504489792)
built one canonical 124,589-byte component on Ubuntu and mounted those exact
bytes on Ubuntu, Windows, and macOS. Its certified SHA-256 is
`d1eb0ab24d423d77b4535134c6a3ea53e2563eaef37f8d02ed068cf7b4ae9c4d`.
Host-local builds also passed on every platform, but are intentionally treated
as source-portability evidence rather than reproducible-build evidence.

The source-install and startup values are single-sample local baselines. The
zero values for component-wire bytes, state-commit latency, and multi-instance
RSS mean that those counters were not instrumented in DP1; they are not claims
of zero cost. Numeric regression budgets begin after two comparable releases,
as defined by the DP1 contract.

Hard-gate evidence is independent of those provisional measurements:

- `youth check`, `youth test`, and `youth build --release` pass for the
  published external repository.
- The native smoke run presents the calculator and the supervised runtime
  exits cleanly.
- The host CI matrix validates the same locked component on Ubuntu, Windows,
  and macOS; the final cross-platform identity job requires one SHA-256 across
  all three hosts.
- The calculator source exposes zero raw-WIT concepts and has no local path
  dependency on Youth.
