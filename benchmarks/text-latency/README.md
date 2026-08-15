# Phase 3 text-latency benchmark

Real end-to-end evidence for the userland-authority pivot's typing target:
**p95 native-input → rasterized-pixels ≤ 5 ms**
(`docs/adr/0001-userland-text-authority.md`, `docs/PHASE-3.md`).

This is a **new, stricter** target, not a restatement of an older gate — the
existing UI-interaction latency gate (`crates/instar-host/tests/bridge.rs`)
is looser (P50/P95/P99 = 7/8/16 ms). ≤5 ms for text had to be earned
independently, and no benchmark for it existed before this one: the only
prior art (`crates/instar-shell/src/bin/uibench.rs`) measures the old
host-side UI-tree pipeline, not the Phase 3 kernel/Surface/guest path.

## Run it

```bash
cargo run --release --manifest-path benchmarks/text-latency/Cargo.toml -- \
  measure --iterations 150 --output benchmarks/text-latency/results/reference
```

`cargo test --release --manifest-path benchmarks/text-latency/Cargo.toml`
runs the harness's own unit/mutant tests (12 tests, no guest required).

## What this measures, and what it doesn't

Every workload drives the **real, unmodified `guests/scratchpad` component**
— the guest-owned text-editing policy proof for Phase 3 — through
`instar_shell::test_harness::RuntimeHarness`, the same production path every
existing "real E2E" test in this repo already uses:

```text
winit::event::WindowEvent / WindowOutput
    -> instar_window::winit_adapter::translate
    -> HostBridge::on_window_event
    -> Host (real guest dispatch)
    -> real guest (instar-editor-core document, real WIT calls)
    -> real TextLayout (Parley) / real Surface scene
    -> Presenter::render (real Vello CPU rasterization)
```

**T0** = the harness feeds a real `WindowEvent`/`WindowOutput` into the
production bridge. **T5** = `Presenter::render` returns RGBA pixels.
`Presenter::render` **rasterizes; it does not hand anything to a
compositor.** The gate is measured on rasterized pixels being available,
not on-screen presentation — compositor/vsync/display-server latency is a
different, later concern (a native-shell smoke test), deliberately excluded
here so this number is reproducible without a display server.

This repo's own pixel-level tests (`crates/instar-shell/tests/render.rs`)
already treat exactly this "headless, real production code path" as *real*
rather than *synthetic*, for the same reason: winit's own event loop needs a
display server CI doesn't have, and what winit contributes to the real path
— a queue, a wake, a buffer — is exercised directly.

### GATE build only

This benchmark ships **only the GATE build**: production `guests/scratchpad`,
completely unmodified, `world kernel`, zero timing hostcalls. The original
plan also specified a DIAGNOSTIC build (an off-by-default `bench-probe`
Cargo feature adding `probe.mark`/`probe.report` WIT calls to a
`world kernel-bench` for a full T0→T1→T2→T3→T4→T5 stage breakdown with
per-stage guest-work counters). The **host-side half of that plumbing is
built and merged** (`crates/instar-kernel/wit/bench.wit`,
`crates/instar-kernel/src/bench_probe.rs`, feature-gated T3/T4
instrumentation in `crates/instar-host/src/lib.rs`) and is exercised by
this crate's own unit tests, but the **guest-side half** (adding the
`bench-probe` feature and `mark`/`report` calls to `guests/scratchpad`
itself) was not completed in this session — `guests/scratchpad` was under
active, substantial, concurrent development by another session throughout,
and the responsible tradeoff was to measure the real, currently-landed guest
rather than fork it or race that session's edits. This is why the report
below has no T1–T4 breakdown: it has the number that matters for the gate,
not yet the full stage decomposition for the "why."

## Workload coverage

12 of the 15 required workloads are implemented and measured (see
`src/workloads.rs`): ordinary ASCII typing, key repeat, Unicode combining
text, bidi text, IME commit, multiline preedit updates, a large text-commit
stress test, pointer placement, drag selection, rapid scrolling, and
document-size backdrops (1 MiB / 10 MiB / pathological long line) each
paired with one measured ordinary keystroke.

**Not implemented in this session**, both because they need a small,
guest-reachable addition to `guests/scratchpad` that the same concurrent-
development concern above applies to:

- **Two-caret insertion.** `guests/scratchpad`'s multi-caret policy
  (`ADR 0001`'s own closure proof) is exercised by its unit tests, but there
  is currently no *input-reachable* way to create a second caret through
  the real event loop — only by constructing `Scratchpad` state directly in
  a unit test.
- **Background/unrelated-work workload (A/B/C).** Characterizing whether an
  unrelated concurrent generation starves Scratchpad's latency (graded),
  whether cooperative background work inside Scratchpad still permits
  editing (graded), and what a genuinely hung Scratchpad guest looks like
  (characterization only, not graded — the authority pivot deliberately
  permits this) is designed but not implemented.

## A significant discovery during implementation: the wire text cap

`instar_ui_protocol::SurfaceEvent::decode` hard-rejects any single event's
text over `limits::MAX_TEXT_BYTES` (4096 bytes) — checked unconditionally.
**A native "100 KB paste" cannot be delivered as one `ImeCommit` event under
the current protocol at all.** This benchmark's "100 KB paste" workload was
redesigned as a single commit at the protocol's actual achievable maximum
(`max_bounded_text_commit`, ~4000 bytes), explicitly labeled as such rather
than silently understating a 100 KB claim. The document-size backdrops (1
MiB / 10 MiB) build up via many small chunked commits instead of one giant
one, for the same reason.

A second, related implementation bug is worth naming because it is exactly
the shape of bug this benchmark's own mutant catalog exists to catch:
an earlier version of the harness waited only for the *first* Surface
revision after a multi-event interaction (e.g. a drag: press + 6 moves +
release = 7 events), not for every event to finish being processed. The
leftover unprocessed events silently bled into the *next* measured
interaction's wait, eventually overflowing the guest's bounded event queue
and terminating the generation — a real instance of "an already-completed
[partial] frame satisfies the wait," discovered by the benchmark stalling
rather than by a targeted mutant test catching it after the fact. The fix
(`GateRun::measure_one` in `src/gate.rs`) waits for the Surface revision to
advance by exactly the number of messages `RuntimeHarness::guest_message_count`
reports were actually queued, draining every injected event before
returning.

## Results (reference-macos-arm64-2026-08-14)

**Verdict: FAIL.** Worst graded p95: **13.1 ms** (target: ≤5 ms), from
`keystroke_after_1mib_doc`. Full numbers in `summary.csv` /
`metadata.txt` in this run's results directory.

| workload | p50 | p95 | p99 | max | pass |
|---|---:|---:|---:|---:|:---:|
| ascii_typing | 0.23 ms | 0.25 ms | 0.41 ms | 0.51 ms | ✅ |
| key_repeat | 0.14 ms | 0.16 ms | 0.16 ms | 0.26 ms | ✅ |
| unicode_combining | 0.20 ms | 0.29 ms | 0.34 ms | 0.35 ms | ✅ |
| bidi_text | 0.44 ms | 0.67 ms | 0.73 ms | 4.76 ms | ✅ |
| ime_commit | 0.43 ms | 0.52 ms | 0.60 ms | 235.03 ms | ✅ |
| multiline_preedit | 0.11 ms | 0.16 ms | 0.26 ms | 0.43 ms | ✅ |
| max_bounded_text_commit | 0.17 ms | 0.61 ms | 0.86 ms | 0.86 ms | ✅ |
| pointer_placement | 0.21 ms | 0.26 ms | 0.46 ms | 0.61 ms | ✅ |
| drag_selection | 0.56 ms | 0.66 ms | 0.78 ms | 0.80 ms | ✅ |
| rapid_scrolling | 0.95 ms | 1.16 ms | 1.37 ms | 2.27 ms | ✅ |
| **keystroke_after_1mib_doc** | **7.85 ms** | **13.12 ms** | 15.71 ms | 15.71 ms | ❌ |
| **keystroke_after_10mib_doc** | **7.37 ms** | **8.15 ms** | 9.04 ms | 9.04 ms | ❌ |
| keystroke_pathological_long_line | 2.71 ms | 2.77 ms | 2.85 ms | 2.85 ms | ✅ |

(128 KiB single unbroken line; smaller than the 1/10 MiB documents on
purpose — see below.)

Reference machine: Apple M1, macOS, `rustc 1.97.1`, release profile, 640×480
@ 1.0 scale. Numbers were taken with other builds/tests running concurrently
in the same working tree (several other active sessions); treat as
order-of-magnitude and reproduce before trusting for a specific decision —
the same caveat `docs/DOS-STARVATION-AUDIT.md` and
`docs/baselines/PERFORMANCE.md` give their own numbers.

## Diagnosis

**Ordinary interactive editing is fast — well under budget.** Every
workload that edits a small/empty document (typing, IME, pointer, drag,
scroll, a max-size single commit) lands with p95 under ~1.2 ms, comfortably
inside the 5 ms target, with rendering genuinely happening each time (the
printed pixel checksum is non-zero and varies run to run).

**The gate fails specifically when editing inside a large preloaded
document**, and the shape of the failure is itself informative:

- 1 MiB and 10 MiB documents (paragraphed, newline every ~200 bytes) both
  cost ~7-8 ms p50, ~8-13 ms p95 for a *single ordinary keystroke* —
  10-25x the un-preloaded case.
- The 128 KiB **pathological single unbroken line** — the shape
  `docs/DOS-STARVATION-AUDIT.md`'s finding F1 specifically measured as
  expensive (a single 4 KiB unbroken line already costing 4.75-12.7 ms to
  shape) — passes the gate at p95 = 2.8 ms, *cheaper* than the much smaller
  1 MiB paragraphed document.

That ordering is the actual finding: the dominant cost here does not look
like F1's per-call shaping cost for one long unbroken run (that would
predict the *pathological-long-line* workload to be the worst offender, and
it is not). It looks like something scaling with **overall document size or
line count** rather than with the visible edit — exactly the "critical
assertion" this benchmark exists to check
(*"work is proportional to visible/changed presentation, not document
size"*), and evidence that it is currently **violated** for large
documents.

Candidates worth checking first, in order of how directly they match this
shape (none require reintroducing host document ownership):

1. **`guests/scratchpad`'s own line/caret lookup** (`primary_line`,
   `line_of_byte`-style calls) — if finding "the line at the caret" scans
   proportionally to document size or total line count rather than seeking
   directly, every keystroke pays for the whole document regardless of
   what's visible. This is guest-owned code, so it's a userland fix, not an
   authority-model change.
2. **`docs/DOS-STARVATION-AUDIT.md`'s F7** — `LayoutRegistry::create`'s
   live-layout count scan is `O(total slots ever allocated this process)`,
   not `O(cap)`. A long-running benchmark process that has created many
   layouts over its lifetime would see this compound, though it doesn't by
   itself explain why 1 MiB is worse than 10 MiB in this run's numbers.
3. **F4** (same audit) — `rebuild_scene` composites every retained
   surface's scene on every accepted update, not just the touched one; not
   document-size-proportional by construction, but worth ruling out.

None of these require reversing the userland-authority pivot; all three are
generic host- or guest-side algorithmic fixes (seek instead of scan, bound
the registry scan, or narrow `rebuild_scene`'s composited set), matching
`docs/DOS-STARVATION-AUDIT.md`'s own G1-G4 recommendations' spirit: fix the
generic layer that dominates, don't special-case editors.

## Known limitations of this run

- DIAGNOSTIC-mode stage breakdown (T1-T4, guest-work/boundary-byte
  counters) is not included — see "GATE build only" above. The host-side
  half of that plumbing exists and is unit-tested
  (`crates/instar-kernel/src/bench_probe.rs`); finishing it means adding a
  `bench-probe` feature to `guests/scratchpad` itself.
- Two-caret insertion and the background/unrelated-work workload (A/B/C)
  are not implemented — see "Workload coverage" above.
- One pre-existing test in this working tree,
  `crates/instar-shell/tests/scratchpad.rs`'s
  `pointer_selection_and_wheel_stay_guest_local_and_reuse_bounded_layouts`,
  currently fails on a color-comparison assertion. This benchmark's changes
  do not touch `guests/scratchpad`, `instar-editor-core`, or that test file,
  and every shared-crate change this benchmark made is behind an
  off-by-default Cargo feature not enabled by that test's build — this
  looks like unrelated, pre-existing, still-in-progress work from a
  concurrent session, not something introduced here, but it was not
  independently root-caused.
