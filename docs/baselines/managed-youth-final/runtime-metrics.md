# managed-youth-final baseline: partial result

## Status: incomplete, stopped due to a host disk-space constraint

WP0.3 (docs/PHASE-1.md) called for a full baseline of the `managed-youth-final`
tag: commit hash, `cargo metadata`/`cargo tree`, build/test results, release
binary size, release build time, desktop startup time, desktop committed RSS,
desktop reserved virtual memory, desktop thread count, desktop idle CPU over
30 seconds, and counter component size.

What was actually captured, in a standalone checkout of `managed-youth-final`:

- `commit.txt` — commit hash and `git log -1`. Complete.
- `cargo-metadata.json` — `cargo metadata --format-version=1`. Complete.
- `cargo-tree.txt` — `cargo tree`. Complete.
- `build-log.txt` — **incomplete**. `cargo build --release --workspace` got
  through compiling essentially the entire 39-crate workspace but stalled on
  the final three LTO-linked binaries (`youth`, `youth-capsule-launcher`,
  `youth-desktop`) for close to an hour without completing. This machine's
  disk ran critically low during the session (down to ~1.8-2.8GB free on a
  494GB volume, most of which is consumed by something outside this
  project — see the conversation this baseline was captured in). Fat-LTO
  linking needs substantial scratch space; the stall is almost certainly
  disk-pressure-induced I/O degradation, not a problem with the build itself
  or the pinned toolchain. The process was killed and its worktree removed to
  reclaim ~1.8GB rather than let it continue competing for disk indefinitely.
- Test results, binary sizes, counter component size, startup time, RSS,
  reserved VM, thread count, idle CPU: **not captured**. All of these need
  either the completed release build (binary sizes, startup/RSS/etc.) or a
  `cargo test --release` pass (test results) that was never reached.

## This is an acceptable partial outcome, not a blocker

Per docs/PHASE-1.md: "Memory and startup are discovery metrics during Phase
1. Do not invent targets before measuring the actual baseline" — these
numbers were always exploratory, not a gate Phase 1's other work packages
depend on. WP1-WP3 (toolchain lock, kernel scaffold, headless async spike)
do not need this baseline to proceed, and did in fact proceed while this was
still running.

## Redo later, on a machine/session with more headroom

To finish this baseline: free up disk space (well clear of the ~5GB+ fat-LTO
linking three winit/tokio/vello/accesskit/wasmtime binaries appears to need
as scratch space, based on this attempt), then re-run using the same
methodology this file documents:

```bash
git clone <repo> <scratch-dir> && cd <scratch-dir> && git checkout managed-youth-final
git rev-parse HEAD && git log -1                          # commit.txt
cargo metadata --format-version=1                          # cargo-metadata.json
cargo tree                                                  # cargo-tree.txt
time cargo build --release --workspace                     # build-log.txt
cargo test --release --workspace                            # test-results.txt
# binary-sizes.txt: ls -la target/release/{youth,youth-desktop}
# counter-component-size.txt: build guests/counter for wasm32-wasip2, record .wasm size
# runtime metrics: launch the built `youth` binary against the counter
# component, sample `ps -o rss=,vsz=,%cpu= -p <pid>` over a settled 30s
# idle window, and count threads via `ps -M -p <pid>`.
```

This methodology is the one WP3.2 and WP9 reuse for the equivalent Instar
measurements, so whenever this gets redone, keep it identical for a valid
comparison rather than improvising a different one.
