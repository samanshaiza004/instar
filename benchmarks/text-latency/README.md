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

**Three more beyond the original 15, added after a review found a gap the
list above didn't cover**: `backspace_at_end_1mib`, `backspace_at_end_10mib`,
and `delete_forward_large_doc`. Every large-document workload above measures
*insertion*; nothing measured *deletion*. That gap was hiding a real,
distinct bug: `Document::previous_grapheme_boundary`
(`crates/instar-editor-core/src/lib.rs`) scanned every grapheme from byte 0
up to the caret on every call, making Backspace `O(caret position)`
regardless of how well the rest of the editor performed — a document could
pass every graded insertion workload while Backspace near its end stayed
pathological. Fixed to a reverse, near-caret traversal (`crop::Rope`'s
`Graphemes` is a `DoubleEndedIterator`; `next_back()` from the caret is
`O(log n)`, not `O(caret position)` — see the fix's own doc comment).
`next_grapheme_boundary` (forward-delete) was already near-caret and did not
need the same fix; `delete_forward_large_doc` exists so a future regression
there is caught the same way, not because one is currently suspected.

This is a **separate finding from the keystroke-latency gate FAIL below**.
That investigation is about *insertion* scaling with document size, with the
leading suspect being a guest-side line/caret *lookup* (`primary_line`,
`line_of_byte`-style calls), not the grapheme-boundary walk this fix
addresses. Fixing Backspace does not by itself resolve that FAIL — re-run
the gate after both are addressed before expecting a PASS.

**And a third, upstream one this fix does not close.** Both
`next_grapheme_boundary` and `previous_grapheme_boundary` validate `byte`
through `Document::is_grapheme_boundary` before doing anything else, and
that call is itself `O(len_bytes() - byte)` in `crop` 0.4.3 — its own
`rope/utils.rs` walks the rope's chunk list backward from the *end* to find
`byte`, with a `// TODO: ... if we want to make this fast` left in the
crate's own source. Measured on a 10 MB document: ~35 us near byte 0, ~12 us
at the midpoint, ~0.1 us near the end. This is exactly why
`backspace_at_end_1mib`/`backspace_at_end_10mib` name "at end": that's where
`len_bytes() - byte` is small and this cost disappears, which is also
precisely the case the original bug report was about. It is **not** fixed
for Backspace/Delete performed near a document's start or middle — those
still pay a real, position-dependent cost, unrelated to and not addressed by
the reverse-traversal change here. See the doc comment on
`Document::is_grapheme_boundary` (`crates/instar-editor-core/src/lib.rs`)
for the full account and why a local reimplementation wasn't attempted here
(real Unicode grapheme-segmentation correctness surface, not a quick patch).

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

## The completion-invariant bug (why the gate is currently UNVALIDATED)

The harness went through two different broken ideas of "this interaction is
done," in sequence, before landing on the current one. Both are worth
recording, because both are the same category of mistake this benchmark's
own mutant catalog exists to catch: treating an *assumption about the
implementation* as a *causal signal*, instead of actually observing
completion.

**Attempt 1 — wait for the first new Surface revision.** The original
`measure_one` sent one interaction and waited for `surface_revision` to
increase by any amount. This broke for multi-event interactions (a drag:
press + 6 moves + release): the first revision bump satisfied the wait, but
the *remaining* injected events were still unprocessed, and were left to
complete during the *next* measured interaction instead of this one. Enough
of that pressure overflowed the guest's bounded event queue and terminated
the generation outright — a real instance of "an already-completed
[partial] frame satisfies the wait."

**Attempt 2 — wait for exactly one revision per queued message.** The fix
for attempt 1 counted `RuntimeHarness::guest_message_count` before and
after `inject`, and waited for the Surface revision to advance by exactly
that many. This is what actually produced the pre-fix 13.1 ms reference
run below. It broke the moment `guests/scratchpad` gained (correctly)
dirty-driven presentation: `present()` no longer runs unconditionally after
every event, only after ones that change something visible. A key release,
a passive pointer move, focus-gained, `ImeEnabled`, and metrics all reach
the guest's event loop and correctly produce zero new revisions. Attempt
2's arithmetic still expected one revision per message regardless, so it
could hang for up to `PATIENCE` waiting for a revision that was never
going to arrive — this is what a "keystroke sends keydown+keyup as two
messages, expects two revisions, but keyup is correctly invisible" workload
looks like from the outside, and it is not a system-load artifact: it
reproduced identically at a 60-second patience budget on this same machine.
A second, compounding version of the same mistake existed in `main.rs`
itself: `focus_surface`'s click was never settled before the next sample's
baseline was captured, so unsettled setup could satisfy part of an
unrelated measured interaction (the opposite failure: a false pass instead
of a false hang).

**Current design (`GateRun::measure_one`/`GateRun::settle` in
`src/gate.rs`)**: every workload declares `expects_render: bool` for the
interaction it measures — a fact about what the interaction *means*, not
something inferred from message counts. Completion is quiescence: the
harness polls until Surface-revision activity and host effects both go
idle for a few consecutive short polls, recording T5 at the *last* observed
revision change (so a burst of events that coalesces into fewer rendered
frames is treated as the legitimate optimization it is, not a correctness
failure) rather than the first. `GateRun::settle()` runs both before and
after every measured interaction, so setup and trailing effects never
contaminate a sample's baseline in either direction. Typing specifically
now measures keydown alone (`workloads::ascii_typing`); the paired keyup is
sent through `GateRun::send_untimed` afterward, deliberately outside the
timed interval (`workloads::release_character`'s doc comment has the full
argument for why release cannot be timed at all under dirty presentation).
`StageTimes::expects_render` lets `validate()` continue to require
`frames_rendered >= 1` when a render was expected, while correctly
accepting `frames_rendered == 0` for a workload that declared it did not
expect one — see `src/sample.rs`'s mutant tests for both directions.

This is landed and unit-tested (13 tests, `cargo test --release`), and
partial live verification (3 of the graded workloads, run against the real
guest) shows the fix working correctly: fast, correct timings, no false
hangs on non-visible events. **A full clean run has not yet completed.**
While re-verifying, a *separate, newly discovered* issue surfaced:
`bidi_text` currently hangs waiting for a Surface revision that never
arrives, reproducing identically at a 30-second patience budget — this
argues against a system-load explanation, since every other tested
workload completed in well under a millisecond in the same run. This has
not been root-caused. It was not present in the pre-fix reference run
below (`bidi_text` passed there, p95 0.67 ms), so something changed
between that run and now, in code this session did not author, and it is
reported here rather than worked around. Until it (and a clean full run)
are resolved, **no PASS/FAIL verdict from this benchmark should be
trusted** — see `docs/PHASE-3.md`'s "Latency gate: UNVALIDATED" section.

## Results (reference-macos-arm64-2026-08-14) -- historical, pre-fix

Preserved as the first failing baseline, not overwritten. This run used the
now-replaced "one revision per queued message" completion logic (see
above), which happened to be adequate for every workload it measured
*except* the ones that never got the chance to expose its flaw — so its
individual numbers below are trustworthy as *evidence a regression exists*,
but the harness itself should not be considered validated by this run.

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
- The joined Scratchpad seam test
  `crates/instar-shell/tests/scratchpad.rs`'s
  `pointer_selection_and_wheel_stay_guest_local_and_reuse_bounded_layouts`
  is green on the current master baseline. It is part of the required
  end-to-end signal and must remain green; a future benchmark or host change
  must root-cause any regression rather than carrying a stale limitation note.
