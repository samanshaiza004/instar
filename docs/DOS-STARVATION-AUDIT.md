# Surface/TextLayout: CPU denial-of-service and presentation-starvation audit

What this is: a review of the Phase 3 Surface/TextLayout model
(`instar-text-layout`, `instar-surface-protocol`, and their host-side wiring
in `instar-kernel`/`instar-host`) for whether a guest that only ever submits
individually legal, individually bounded requests can still starve native
input or presentation by repeating them. It does not propose or require any
change to editor semantics, and no finding here interprets what guest text or
scene bytes *mean* — every gate recommended is mechanical (bytes, counts,
elapsed time, in-flight count).

Numbers below were taken with `cargo test --release` (the workspace pins
`lto = true` for release, `Cargo.toml:53`), under some contention from other
concurrent builds — the same caveat `docs/baselines/PERFORMANCE.md` gives its
own numbers. Treat them as order-of-magnitude and reproduce before trusting
them for a specific decision; reproduction notes are given per finding. The
measurement code itself was added as temporary `#[ignore]`d tests, run once,
and reverted — it is intentionally not part of the permanent suite.

## Findings

### F1 — `create_layout` does expensive work before two of its three bound checks

`crates/instar-text-layout/src/lib.rs:1282-1319`.

```
1290  if text.len() > MAX_LAYOUT_TEXT_BYTES { return Err(...) }   // checked first
1308  let mut layout = self.shape_keyless_with_line_height(...)    // expensive
1309  layout.break_lines(width);                                   // expensive
1310  layout.align(alignment);                                     // expensive
1312  if lines > MAX_LAYOUT_LINES { return Err(...) }               // checked after
1316  if clusters > MAX_LAYOUT_CLUSTERS { return Err(...) }         // checked after
```

Only the byte-length cap gates admission. Line-count and cluster-count are
refusals *after* shape + break + align already ran. A request engineered to
trip `TooManyLines` costs the host the full shaping pass and creates no
`LayoutRegistry` entry — it never touches `MAX_LIVE_LAYOUTS`
(`crates/instar-host/src/presentation_host.rs:61`), so repeating it leaves no
trace in any existing counter and never becomes cheaper or more refused over
time.

Measured, warm, release, `cargo test --release -p instar-text-layout`:

| Input at `MAX_LAYOUT_TEXT_BYTES` (4096 B) | Outcome | Time |
|---|---|---:|
| Single unbroken 4096 B word, no wrap width | accepted | 4.75-7.29 ms (5 samples) |
| Same, `width=50.0` | accepted | 12.0 ms |
| Alternating `"a "`, `width=8.0` | accepted | 12.7 ms |
| 2048x `"a\n"` (forces > `MAX_LAYOUT_LINES`) | refused, `TooManyLines` | 8.7 ms single / 10.1 ms avg over 10 |

Every one of these, including the refused one, exceeds a p95 <= 5 ms
per-keystroke budget by itself.

### F2 — no per-generation concurrency or rate limit on presentation requests

`crates/instar-kernel/src/runtime.rs:228-247` (`submit_presentation`),
`crates/instar-kernel/src/presentation.rs:211-228` (`try_mark_surface`),
`crates/instar-host/src/bridge.rs:87-98` (queue capacity rationale).

`kernel-ui.commit` is single-flight per generation
(`commit_in_progress_rejections`, `runtime.rs:180`, `275-277`). `create-layout`
has no equivalent: `try_mark_surface` only guards `UpdateSurface`, and only
per distinct target `NodeKey`:

```rust
// presentation.rs:216
let PresentationOperation::UpdateSurface { target, .. } = &self.operation else {
    return true;   // every CreateLayout is "fine to submit", unconditionally
};
```

One generation can have as many concurrent `create-layout` calls in flight as
its own async runtime lets it spawn, and one `update-surface` per distinct
Surface — up to 4096 (`MAX_NODES`, `crates/instar-ui-protocol/src/lib.rs:61`).

The shared runtime->main queue's capacity was sized on an assumption that only
half its traffic honors:

```rust
// bridge.rs:87-98
/// ...Runtime->main is bounded by the same number for symmetry, but its
/// natural ceiling is far lower — a guest task has at most one commit
/// outstanding, because it is suspended on the reply.
pub const QUEUE_CAPACITY: usize = 256;
```

True for `UiCommit`. Not true for `Presentation` requests, which share this
exact queue (`MainThreadChannels.events: SyncSender<HostUserEvent>`,
`bridge.rs:182`, `223-238`).

### F3 — `serve_presentation` runs synchronously on the winit thread, no per-item preemption

`crates/instar-host/src/bridge.rs:898-938` (`pump`/`pump_bounded`),
`crates/instar-host/src/lib.rs:511-592` (`serve_presentation`).

`pump_bounded`'s own doc comment: *"This is what a winit `user_event` handler
calls."* Its loop checks the time budget (`PUMP_TIME_BUDGET = 1ms`, line 890)
only *after* `on_user_event` returns, never during:

```rust
// bridge.rs:909-936
loop {
    // terminal-outcome check
    match self.events.try_recv() {
        Ok(event) => {
            effects.extend(self.on_user_event(event));   // serve_presentation runs here, inline
            processed += 1;
            if processed >= item_budget || Instant::now() >= deadline { ... return }
        }
        Err(_) => return effects,
    }
}
```

`on_user_event -> serve_presentation` is where `create_layout`,
`instar_surface_protocol::decode`, `stage_scene` (lowering), and
`rebuild_scene` all run back to back with no yield point. `PUMP_ITEM_BUDGET =
64` counts items, not cost — 64 back-to-back expensive items in one pump call
is exactly as legal as 64 cheap ones. Since `on_window_event` (native
pointer/keyboard/resize, `bridge.rs:800`) and `pump` share the same winit
event-loop callback, a thread busy inside `serve_presentation` cannot deliver
a queued keystroke or paint a frame until that call returns. This is the
literal mechanism of main-thread starvation here.

### F4 — `rebuild_scene` cost scales with all accumulated window state, not the touched surface

`crates/instar-host/src/lib.rs:711-754`.

```rust
PresentationState::App => match (window.tree.as_ref(), window.layout.as_ref()) {
    (Some(tree), Some(layout)) => self.scenes.app_scene_with_surfaces(
        tree, layout, &window.scroll, metrics, ...,
        &window.surface_scenes,   // every surface's retained scene, not just the touched one
    ),
```

Called unconditionally after every successful `UpdateSurface` (`lib.rs:578`).
A guest that first populates many surfaces near their per-request cap, then
issues cheap single-command updates to any one of them, pays this full-window
cost every time. No cap exists on total surfaces x total bytes across a
window — only the per-request 1 MiB cap and the indirect `MAX_NODES = 4096`
cap on surface count. Not directly measured (needs a populated multi-surface
window fixture); flagged as the next thing to measure, not guessed at.

### F5 — scene decode is well-gated and cheap at the legal maximum (positive finding)

`crates/instar-surface-protocol/src/lib.rs:216-275`.

Unlike F1, size (line 217) and command count (line 232) are checked *before*
the decode loop, and clip/transform depth incrementally *inside* it (lines
243, 252). Measured, warm, `cargo test --release -p instar-surface-protocol`,
average of 20:

| Scene at the legal maximum | Time |
|---|---:|
| 65,534 `PushClip`/`PopClip` pairs (589,815 B) | 1.99 ms |
| 49,931 `FillRect` (1,048,560 B) | 2.63 ms |

Both comfortably inside budget. Lowering (`stage_scene`,
`presentation_host.rs:264-283`) is O(commands), each a no-op or an
`Arc::clone` — structurally cheap, not separately measured. Decode/lowering
are not where this system's DoS risk lives; F1 and F4 are.

### F6 — teardown cancellation works for queued work, not in-progress work

`crates/instar-host/src/bridge.rs:994-1008, 1043-1047`.

```rust
// bridge.rs:998
if request.generation() != self.generation {
    request.refuse(PresentationRefusal::StaleGeneration);
    return Vec::new();
}
```

Real and correct for anything still queued. `retire` (`1043-1047`) is one
line: it zeroes `self.generation`, which is what this check reads. It does
nothing for a request already past this check and running inside
`serve_presentation` — that call has no cancellation token or yield point, so
it runs to completion regardless of what happens to its generation meanwhile.
The only real per-operation cancellation (`op.abort.abort()`, `runtime.rs:135,
157, 962`) is wired to `ops.cancel`, not to presentation operations.

Minor secondary point: `retire` doesn't proactively drain the shared queue of
that generation's still-queued requests; they're refused lazily, one at a
time, as `pump_bounded` reaches them. Cheap per item, but real head-of-line
delay for the successor generation.

### F7 — `LayoutRegistry::create`'s live-count check scans the whole process's slot history

`crates/instar-host/src/presentation_host.rs:47-63`.

```rust
let live = self.slots.iter().flatten()
    .filter(|slot| slot.generation == generation && slot.guest_lease)
    .count();
```

`self.slots` only grows across the host process's lifetime, not per
generation. Cost is proportional to total slots ever allocated by any
generation this process has run, not to the 4096-item cap being checked. Not
a severe DoS lever on its own (per-item cost is a cheap filter+count, bounded
per generation by the 4096 cap), but a scaling smell in a process that runs
many short-lived generations.

### F8 — runtime-thread starvation: not found to be significant here, by design

`crates/instar-kernel/src/runtime.rs:474-528, 902-945`.

Both `create_layout` and `update_surface` host functions do only cheap,
bounded work synchronously on the runtime thread (a resource-table push, or an
O(<=4096) resource-table lookup per borrowed layout handle bounded by
`MAX_RESOURCE_REFERENCES`), then `.await` a oneshot reply — which suspends
only that guest task, not the executor. The expensive work has already been
moved to the main thread by construction. Stated explicitly since the audit's
target invariants call for characterizing every listed axis, including the
ones with good news.

## Worst-case measured operations (summary)

| Operation | Legal max | Measured warm cost | vs. p95 <= 5 ms |
|---|---|---:|---|
| `create_layout`, single 4096 B unbroken run | accepted | 4.75-7.29 ms | at/above budget alone |
| `create_layout`, 4096 B narrow wrap width | accepted | 12.0 ms | 2.4x budget alone |
| `create_layout`, 4096 B forced hard breaks | refused | 8.7-10.1 ms | 1.7-2x budget, for a rejected call |
| `Scene::decode`, 65,534 commands | accepted | 1.99 ms | within budget |
| `Scene::decode`, 49,931 `FillRect` (1 MiB) | accepted | 2.63 ms | within budget |
| `rebuild_scene` at scale | - | not measured | flagged, not guessed |

## Recommended minimal work gates

All four reuse patterns already present in this codebase rather than
introducing new categories of machinery.

**G1 — byte-level hard-break pre-scan before shaping.** Count `\n`/`\r`
against `MAX_LAYOUT_LINES` before calling
`shape_keyless_with_line_height`/`break_lines`/`align`. Closes F1's
cheaply-detectable worst sub-case (the refused-but-8-10ms hard-newline case)
at near-zero cost. Cannot close the general narrow-wrap-width case (F1's 12 ms
row) — that genuinely requires shaping to know. Leave that residual to G2/G3.

**G2 — extend single-flight to presentation operations, per generation.**
Mirror `commit_in_progress_rejections`: at most one outstanding
`CreateLayout`; keep the per-target `UpdateSurface` dedup but also cap total
outstanding `UpdateSurface` per generation to a small constant. Restores the
invariant `QUEUE_CAPACITY`'s own comment already assumes (F2).

**G3 — a small per-generation time budget on presentation serving.**
Single-flight bounds concurrency, not repetition — a guest can still legally
chain call -> reply -> call -> reply forever. Attribute each
`serve_presentation` call's measured wall time to its generation; once
cumulative cost in a short rolling window exceeds a budget sized directly from
F1's numbers (e.g., so that even at the worst measured ~13 ms/op one
generation can't exceed roughly half of any given window), refuse further
presentation requests from that generation with a new refusal until the
window resets. A counter and a clock comparison, not a scheduler.

**G4 — `retire` proactively drains this generation's queued requests.**
Cheap, closes F6's minor head-of-line component. Optional if the channel type
makes selective draining awkward — F6's real finding (in-progress
cancellation) isn't fixed by this either way.

None of G1-G4 touch `MAX_LAYOUT_TEXT_BYTES`, `MAX_SCENE_BYTES`, or any other
size constant. If the residual worst case (F1's 12 ms row) is still
unacceptable after G1-G3, the next lever is shrinking `MAX_LAYOUT_TEXT_BYTES`
itself — exactly the "provisional... pending the documented 4/8/16 KiB
reference benchmark" calibration `docs/PHASE-3.md` already flags as open.
This audit's numbers are a data point for that benchmark, not a reason to
invent something new.

## Explicitly not worth adding

* No content-based or "does this look hostile" classification of text or
  scenes — every gate above is mechanical, and this would be the first place
  host code started interpreting guest data.
* No general CPU-time metering/billing system for guest operations — four
  small local counters (G1-G4) fully cover what was found.
* No preemptible/cancellable shaping (patching Parley, or a killable worker
  thread with a hard timeout) — once G3 bounds cumulative exposure, the worst
  single blocking event (~13 ms, measured) is a rare bounded tail, not a
  repeatable weapon. Not worth the engineering cost or the new failure modes
  ("what does a mid-shape kill leave behind?") for one bounded event.
* No moving shaping/decode to a separate thread pool with async completion —
  a materially bigger change (two threads now touch layout/scene state) to
  solve what bounding frequency and concurrency of the existing synchronous
  calls already solves.
* No cross-window/cross-generation fairness scheduler — no evidence of a
  cross-window contention problem, only within-generation and within-window
  ones.
* No adaptive/heuristic tuning of the size constants — keep bounds static and
  measurement-derived, per `docs/baselines/PERFORMANCE.md`: "do not optimize
  the constant before the model is known."

## Mutant / test plan

| # | Mutant | Caught by |
|---|---|---|
| T1 | Revert G1: drop the hard-break pre-scan | `create_layout` on 2048x`"a\n"` (4096 B) must return `TooManyLines` in well under 1 ms, not the measured 8.7-10 ms. Time-bounds the rejection path specifically. |
| T2 | G2's single-flight check bypassed for `CreateLayout` | Two concurrent `create-layout` calls from one generation; assert the second is refused, mirroring `runtime.rs:1422` (`concurrent_commits_are_single_flight_and_the_slot_releases`) for the new operation kind. |
| T3 | G3's rolling window resets every call (never actually limits) | N presentation calls back to back where N x worst-measured-cost exceeds the intended window budget; assert at least one is refused before N are all served. |
| T4 | G3's counter is global instead of per-generation | Generation A exhausts its budget; assert generation B's presentation calls are still served normally in the same window. |
| T5 | G4/F6's staleness check removed or checked after `serve_presentation` | Queue several presentation requests, trap the generation, assert none reach `serve_presentation` (instrument a call counter). |
| T6 | `pump_bounded`'s deadline check removed | Extend the existing `pump_respects_its_time_budget` (`bridge.rs:1255`) to exercise a presentation item, not only commits, if it doesn't already. |
| T7 | `pump_bounded`'s item-count check removed | Extend `pump_respects_its_item_budget_and_schedules_continuation` (`bridge.rs:1233`) the same way. |
| T8 | `Scene::decode`'s command-count check moves after the loop | A malformed scene claiming a huge header count with a truncated body must fail fast (bounded time), not attempt to decode as many commands as the truncated bytes allow. |
| T9 | `stage_scene`'s `layouts[layout_slot as usize]` reached with a slot not validated against the actual slice length at the call site | Fuzz/property test for `resource_count` vs. actual `layouts.len()` drift; assert no panic. Adjacent to CPU-DoS scope but a real host-crash vector worth its own test. |
| T10 | F4's cost characterization, once measured | Not written yet, deliberately — F4 has no gate recommended until it's measured. The measurement (populate a window with N surfaces near the byte cap, time `update-surface` on one as N grows) is the prerequisite; its own regression test is the right T10. |

Scope note: no change proposed here touches `instar-editor-core` or the
Phase 3 guest-side proof. Every finding and every recommended gate is
host-side and mechanical.
