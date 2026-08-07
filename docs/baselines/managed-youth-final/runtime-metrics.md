# managed-youth-final baseline: partial result

Pre-rewrite baseline of the `managed-youth-final` tag (`fac5f8d`), captured
2026-08-06/07. **Partial**: the build and size numbers are complete, the
runtime measurements are not.

> Correction: an earlier version of this file said the release build stalled
> and that binary sizes were never captured. That was wrong — it described a
> second, redundant capture attempt that did stall on a disk-space problem,
> while an earlier attempt had already completed the release build and
> recorded sizes. The completed data is what is in this directory now.

## Captured

| Artifact | File | Notes |
|---|---|---|
| Commit hash | `commit.txt` | `fac5f8d`, plus `git log -1` |
| Dependency metadata | `cargo-metadata.json` | `cargo metadata --format-version=1` |
| Dependency tree | `cargo-tree.txt` | `cargo tree` |
| Release build log + timing | `build-log.txt` | `cargo build --release --workspace`, finished in **42m 17s** (2538s wall, 2197s user, 1131s sys) |
| Release binary sizes | `binary-sizes.txt` | `youth` 30.07 MB, `youth-capsule-launcher` 26.28 MB, `youth-desktop` 25.57 MB (Mach-O arm64) |

## Not captured

- **Test results.** `test-results.txt` holds only compilation output; the run
  never reached a single `test result:` line, so it records nothing about
  whether the suite passed. Treat the file as evidence of an attempt, not as a
  result.
- **Counter component size.**
- **Runtime measurements**: startup time, committed RSS, reserved virtual
  memory, thread count, idle CPU over 30s. These need the built `youth` binary
  driven against the counter component, which was never done.

## Why this is an acceptable place to stop

Per the Phase 1 plan, memory and startup are *discovery* metrics — "do not
invent targets before measuring the actual baseline" — not a gate that other
work packages depend on. WP1–WP3 (toolchain lock, kernel scaffold, Gate 0
spike) did not need any of this and proceeded independently. The missing
numbers matter later, for the WP9 comparison, not now.

The build and size numbers are also the ones most expensive to reproduce (42
minutes of release build) and they are done. What remains is comparatively
cheap once someone has a built binary in hand.

## Finishing it later

Runtime measurements, using the already-built binaries:

```bash
# startup + steady-state, driven against the counter component
/usr/bin/time -l target/release/youth <counter-component.wasm>
ps -o rss=,vsz=,%cpu= -p <pid>     # sample over a settled 30s idle window
ps -M -p <pid>                      # thread count
```

Test results, from a checkout of the tag:

```bash
cargo test --release --workspace
```

Counter component size: build `guests/counter` for `wasm32-wasip2` and record
the `.wasm` byte size.

Keep this methodology identical when it is redone — WP9 compares Instar's
numbers against these, and a differently-measured baseline is not a baseline.

## Note for whoever redoes this

Two captures were run concurrently against the same tag, in two different
scratch worktrees, and the second overwrote parts of the first's output before
stalling. That is why the files here span two runs and why this document needed
a correction. If you re-run it: use one worktree, and write to a fresh output
directory rather than over an existing one.
