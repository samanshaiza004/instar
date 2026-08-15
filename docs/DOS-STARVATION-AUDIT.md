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

**This document was corrected after review.** The first pass mis-transcribed
one measurement (see "Corrections" below), which weakened F1 in one specific
respect and led to reprioritizing this whole audit. The corrections are kept
visible rather than silently folded in, because the review that caught them —
including a serious finding this pass missed entirely (F9) — is as much a part
of the record as the original numbers.

## Corrections after review

1. **F1's "refused, `TooManyLines`" reading was wrong.** The original pass
   reported the 2048x`"a\n"` case (4096 B, 2048 hard breaks) as refused at
   8.7-10 ms. Rerunning it: that input is **accepted** (`lines=2049,
   clusters=4096`, both under their 4096 caps), not refused — the original
   report printed `result.err()`, which was `None`, and that `None` got
   mistranscribed into prose as a rejection. A corrected reproducer that
   actually exceeds `MAX_LAYOUT_LINES` (4096 consecutive `\n` bytes, 0 content
   — see F1 below) **is** refused, and is warm-cheap (~0.7-1.1 ms), not
   expensive. The "expensive to reject, repeatable at zero live-layout cost"
   claim in the original F1 is **not supported** by any input found so far —
   see the revised F1.
2. **The original single-sample readings for the width-constrained cases
   (12.0 ms, 12.7 ms) are now suspect.** Warm-repeating the 2048x`"a\n"` case
   10 times gave 1.43 ms/call average, after a single first-call reading of
   ~31-33 ms in two separate runs — a >20x gap between "first call to this
   specific input" and "warm steady state" that the original pass's
   methodology (mostly single samples, one 5-sample series) did not control
   for. The 12.0/12.7 ms rows are retracted pending a warm-repeated rerun;
   they are not currently known to be real per-call costs.
3. **F2's claim about `create-layout` concurrency was likely wrong, and was
   asserted rather than checked.** `create-layout` is declared `func` (not
   `async func`) in `crates/instar-kernel/wit/kernel.wit:153`, unlike
   `commit` (`kernel.wit:97`, `async func`) and `update-surface`
   (`kernel.wit:177`, `async func`). Per Component Model semantics, a
   synchronous import call blocks the calling instance until it returns —
   the guest cannot issue a second overlapping `create-layout` call before
   the first one's synchronous call site returns, independent of anything the
   host does. Combined with `RuntimeGeneration` being exactly one `Store`, one
   instance, one guest task (`runtime.rs:1102`), this means `create-layout` is
   very likely already single-flight per generation by ABI construction, with
   no host-side gate needed. This has **not** been proven with an actual
   hostile guest component — see the revised F2 and the new top action item.
4. **`TooManyClusters` being dead code is now empirically supported, not just
   argued.** 4096 plain ASCII bytes (the maximum count of single-byte, 1:1
   byte:cluster text) produced exactly `clusters=4096` — at the cap, never
   over it, matching the proof that a cluster can never span zero source
   bytes, so `clusters <= text.len() <= MAX_LAYOUT_TEXT_BYTES ==
   MAX_LAYOUT_CLUSTERS`. See revised F1.
5. **A real containment hole was missed entirely: F9.** `MAX_LIVE_LAYOUTS`
   only counts layouts the guest currently holds an open handle to, not
   layouts a retained Surface scene still holds an `Arc` to after the guest's
   handle is dropped. Added below, and it is now the single most important
   finding in this document.

None of this changes F3, F4, F5, F6, F7, or F8 — those held up on rereading
and are unmodified below.

## Findings

### F1 — `create_layout` checks two of its three bounds after doing the work they'd refuse (structural finding stands; cost claim is weaker than first reported)

`crates/instar-text-layout/src/lib.rs:1282-1319`.

```
1290  if text.len() > MAX_LAYOUT_TEXT_BYTES { return Err(...) }   // checked first
1308  let mut layout = self.shape_keyless_with_line_height(...)    // expensive
1309  layout.break_lines(width);                                   // expensive
1310  layout.align(alignment);                                     // expensive
1312  if lines > MAX_LAYOUT_LINES { return Err(...) }               // checked after
1316  if clusters > MAX_LAYOUT_CLUSTERS { return Err(...) }         // checked after
```

This ordering is a code fact, confirmed by line number, independent of any
measurement. What is **not** currently established is that it's exploitable
for cheap, repeatable, expensive-and-refused work — the one construction
tried for that (see corrections above) turned out to refuse cheaply.

Measured, warm (repeated 10x, not single-sample), release,
`cargo test --release -p instar-text-layout`:

| Input at `MAX_LAYOUT_TEXT_BYTES` (4096 B) | Outcome | Time |
|---|---|---:|
| Single unbroken 4096 B word, no wrap width | accepted | 4.75-7.29 ms (5 samples) |
| 4096x `\n`, 0 content bytes (`lines=4097`, over the 4096 cap) | **refused**, `TooManyLines(4097)` | ~0.7-1.1 ms/call, 10/10 refused |
| 2048x `"a\n"` (`lines=2049`, `clusters=4096`, both under cap) | accepted | 1.43 ms/call avg over 10 (single first-call reading was 31-33 ms — see corrections) |
| 4096x `"a"`, plain ASCII (max possible 1-byte clusters) | accepted, `clusters=4096` (never exceeds) | not separately timed |

Retracted pending a proper warm-repeated rerun (not currently trusted):

| Input | Original single-sample reading |
|---|---:|
| 4096 B single word, `width=50.0` | 12.0 ms |
| Alternating `"a "`, `width=8.0` | 12.7 ms |

**What survives:** the check-ordering is real, and one accepted operation (a
single long unbroken run at the byte cap) reliably costs 4.75-7.29 ms warm —
on its own, at or above a p95 <= 5 ms budget. What does **not** currently
survive: the claim that a refused request can be made expensive and repeated
for free. No input tried so far demonstrates that; the two constructions
tested were either cheap-and-refused or cheap-and-accepted. The
width-constrained cases might still be genuinely expensive — they are simply
unverified, not confirmed.

**Also confirmed:** `TooManyClusters` (`lib.rs:1316`) cannot fire under the
current constants. A cluster cannot correspond to zero source bytes, so
`clusters <= text.len()` always; since `text.len()` is already capped at
`MAX_LAYOUT_TEXT_BYTES = 4096 = MAX_LAYOUT_CLUSTERS`, `clusters >
MAX_LAYOUT_CLUSTERS` requires `clusters > 4096` while `clusters <= 4096` — a
contradiction. The maximal-cluster-count input (4096 single-byte-cluster
ASCII characters) landed exactly at, never over, the cap, confirming this
empirically. This is true for the current constant *values*, not a structural
guarantee — if `MAX_LAYOUT_CLUSTERS` is ever set below `MAX_LAYOUT_TEXT_BYTES`
independently, the check becomes live again.

### F2 — presentation-request concurrency: real for `UpdateSurface`, likely a non-issue for `CreateLayout` by ABI construction (not yet proven)

`crates/instar-kernel/wit/kernel.wit:97,153,177` (sync/async declarations),
`crates/instar-kernel/src/runtime.rs:228-247` (`submit_presentation`),
`crates/instar-kernel/src/presentation.rs:211-228` (`try_mark_surface`),
`crates/instar-host/src/bridge.rs:87-98` (queue capacity rationale).

`create-layout` is declared `func`, not `async func`:

```wit
// kernel.wit
commit: async func(batch: list<u8>) -> result<commit-result, commit-error>;         // :97
create-layout: func(text: string, style: layout-style) -> result<text-layout, layout-error>;  // :153
update-surface: async func(...) -> result<u64, surface-error>;                       // :177
```

A synchronous WIT import blocks the calling instance at the call site until it
returns; a generation is exactly one `Store`, one instance, one guest task
(`runtime.rs:1102`). Together these mean the guest very likely cannot have two
`create-layout` calls in flight from one generation regardless of any
host-side gate — the host's own async Rust implementation
(`async fn create_layout`, `runtime.rs:475`) is an internal convenience for
not blocking the host's own thread while marshaling to the main thread; it
does not by itself let the *guest* issue overlapping calls. This has **not
been proven with an actual hostile `wasm32-wasip2` guest component** — it is
a well-founded reading of the WIT and the generation invariant, not an
empirical result, and belongs at the top of the follow-up list.

`update-surface` genuinely is `async func`, and `try_mark_surface`
(`presentation.rs:216-228`) only dedups per distinct target `NodeKey`:

```rust
// presentation.rs:216
let PresentationOperation::UpdateSurface { target, .. } = &self.operation else {
    return true;   // non-UpdateSurface operations are never deduped here
};
```

So one generation can genuinely have one `update-surface` in flight per
distinct Surface, up to 4096 (`MAX_NODES`,
`crates/instar-ui-protocol/src/lib.rs:61`). This part of F2 stands as
originally reported.

The shared runtime->main queue's capacity was sized on an assumption:

```rust
// bridge.rs:87-98
/// ...Runtime->main is bounded by the same number for symmetry, but its
/// natural ceiling is far lower — a guest task has at most one commit
/// outstanding, because it is suspended on the reply.
pub const QUEUE_CAPACITY: usize = 256;
```

True for `UiCommit`. Very likely true for `CreateLayout` too, by the ABI
argument above (unproven). Not true for `UpdateSurface`, which shares this
exact queue (`MainThreadChannels.events: SyncSender<HostUserEvent>`,
`bridge.rs:182`, `223-238`) and genuinely can have up to 4096 concurrent
entries from one generation.

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
64` counts items, not cost. Since `on_window_event` (native
pointer/keyboard/resize, `bridge.rs:800`) and `pump` share the same winit
event-loop callback, a thread busy inside `serve_presentation` cannot deliver
a queued keystroke or paint a frame until that call returns. This is the
literal mechanism of main-thread starvation here — unmodified by the
corrections above, since it's a structural fact independent of which specific
inputs are expensive.

One thing worth measuring before treating this as urgent: `pump_bounded`
returns immediately after any item that pushed it past budget (it doesn't
keep draining), which hands control back to winit right after one expensive
operation, not after a whole backlog. Whether that alone keeps starvation
inside an acceptable bound, given F1's now-narrower set of confirmed-expensive
operations, is an open, measurable question rather than a foregone conclusion.

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
cost every time.

Qualifier found on rereading: `window.surface_scenes` is pruned whenever a
committed tree removes a Surface node (`lib.rs:1213`,
`surface_scenes.retain(...)` keyed to tree membership) and cleared entirely on
generation end (`lib.rs:844`). So the aggregate footprint here is bounded by
"currently tree-live Surfaces" (up to `MAX_NODES = 4096`), not by an
ever-growing history — a real mitigation, but it doesn't change the finding:
a guest legitimately keeping many Surfaces alive (which the tree cap
explicitly permits) still pays this cost on every update to any one of them.
Not directly measured; flagged as the next thing to measure, not guessed at.
This is the finding the reviewer independently flagged as the one to measure
first, and that assessment is agreed with here.

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
`Arc::clone` — reasoned cheap from the code, **not measured** — stated plainly
as a reasoned bound, not a stopwatch result, per the reviewer's objection to
overstating this. Decode/lowering are not where this system's DoS risk lives;
F1, F4, and F9 are.

### F6 — teardown cancellation works for queued work, not in-progress work (accepted as a V0 contract)

`crates/instar-host/src/bridge.rs:994-1008, 1043-1047`.

```rust
// bridge.rs:998
if request.generation() != self.generation {
    request.refuse(PresentationRefusal::StaleGeneration);
    return Vec::new();
}
```

Real and correct for anything still queued. `retire` (`1043-1047`) zeroes
`self.generation`, which is what this check reads; it does nothing for a
request already past this check and running inside `serve_presentation` —
that call has no cancellation token or yield point. Given today's worst
*confirmed* single operation is ~7 ms (not the ~13 ms originally claimed —
see corrections), "teardown latency may include completion of one
already-admitted bounded presentation operation" is accepted as a reasonable
V0 contract. Building cancellable Parley shaping to close this would be
disproportionate right now.

The minor secondary point (queue not proactively drained on retire) is
deprioritized: not worth rewriting the queue to remove at most 256 cheap
stale requests, especially since the existing channel doesn't obviously
support selective removal without breaking FIFO order for other generations.

### F7 — `LayoutRegistry::create`'s live-count check scans the whole process's slot history (minor)

`crates/instar-host/src/presentation_host.rs:47-63`.

```rust
let live = self.slots.iter().flatten()
    .filter(|slot| slot.generation == generation && slot.guest_lease)
    .count();
```

On rereading: released slots *do* become `None` and get reused
(`create()` looks for a `None` hole before appending, `presentation_host.rs:
98-110`), gated by `collect()` (`161-170`) freeing a slot once
`!guest_lease && Arc::strong_count(&slot.layout) == 1`. So `self.slots.len()`
tracks roughly a historical peak of simultaneously-retained slots, not the sum
of every generation that ever ran — the original framing ("grows across the
whole process's lifetime") overstated this. The O(n) linear scan on every
`create()` call is still real and still inelegant, but it's a minor cleanup
item, not a scaling concern. Downgraded accordingly.

### F8 — runtime-thread starvation: not found to be significant here, by design

`crates/instar-kernel/src/runtime.rs:474-528, 902-945`.

Unmodified by this review pass. Both `create_layout` and `update_surface` host
functions do only cheap, bounded work synchronously on the runtime thread (a
resource-table push, or an O(<=4096) resource-table lookup per borrowed
layout handle bounded by `MAX_RESOURCE_REFERENCES`), then `.await` a oneshot
reply — which suspends only that guest task, not the executor. The expensive
work has already been moved to the main thread by construction.

### F9 — `MAX_LIVE_LAYOUTS` only counts open guest handles, not Surface-retained layouts (new, most important finding)

`crates/instar-host/src/presentation_host.rs:47-63, 139-170`,
`crates/instar-host/src/presentation_host.rs:264-283` (`stage_scene`),
`crates/instar-kernel/src/runtime.rs:551-567` (`HostTextLayout::drop`).

The admission check counts only slots where `guest_lease == true`:

```rust
// presentation_host.rs:55-60
let live = self.slots.iter().flatten()
    .filter(|slot| slot.generation == generation && slot.guest_lease)
    .count();
```

A `text-layout` resource has no explicit WIT release method; dropping the
guest's handle triggers the Component Model's implicit resource destructor,
which calls `ReleaseLayout` and sets `guest_lease = false`
(`runtime.rs:551-567`, `presentation_host.rs:139-150`). `collect()` only frees
a slot (`*slot = None`) when `!guest_lease && Arc::strong_count(&slot.layout)
== 1` (`presentation_host.rs:161-170`). But `stage_scene` gives a retained
Surface its own `Arc::clone` of the layout for every `DrawTextLayout` command
(`presentation_host.rs:275`):

```rust
Command::DrawTextLayout { layout_slot, .. } => {
    Some(Arc::clone(&layouts[layout_slot as usize]))
}
```

So the sequence: create a layout (counts toward `live`), attach it to a
Surface via `update-surface` (`strong_count` becomes 2), drop the guest's
resource handle (`guest_lease = false`, but `collect()` cannot free it —
`strong_count` is still 2, not 1) — reduces `live` back toward zero *while the
layout remains fully resident*, retained by the Surface, indefinitely (until
that Surface's scene is replaced by an update that no longer references it,
or the Surface node itself is removed from the tree).

This is not a contrived attack pattern — it's what any ordinary
text-rendering guest does continuously: create a fresh layout for updated
visible content, attach it to a Surface, drop the old resource handle. Every
cycle of that pattern that changes which layout is attached leaves the
*previous* one uncounted by `MAX_LIVE_LAYOUTS` for as long as anything still
references it, and the guest can trivially construct sequences where nothing
ever stops referencing anything: create, attach to Surface A, drop handle,
create, attach to Surface B, drop handle... `live` never exceeds 1, while
`self.slots` accumulates a `SharedLayout` (shaped glyph data, not just 4 KiB
of source text — larger) per cycle, retained for as long as its owning
Surface's current scene names it. Bounded only by the indirect product of
existing per-request caps — up to `MAX_NODES = 4096` Surfaces, each able to
name up to `MAX_RESOURCE_REFERENCES = 4096` distinct layouts in one scene —
not by any direct aggregate cap.

`MAX_LIVE_LAYOUTS` as currently implemented means "layouts the guest currently
has an open handle to," not "layouts this generation is responsible for
keeping resident." Those are different guarantees, and only the second one is
the one the constant's name and the containment story imply.

## Worst-case measured operations (summary, corrected)

| Operation | Legal max | Measured warm cost | vs. p95 <= 5 ms |
|---|---|---:|---|
| `create_layout`, single 4096 B unbroken run | accepted | 4.75-7.29 ms | at/above budget alone |
| `create_layout`, 4096x `\n` (over line cap) | refused | ~0.7-1.1 ms | within budget |
| `create_layout`, 2048x `"a\n"` (at line/cluster cap, accepted) | accepted | 1.43 ms warm avg | within budget |
| `create_layout`, narrow wrap width variants | accepted | **unverified**, retracted single-sample reads of 12.0/12.7 ms | unknown |
| `Scene::decode`, 65,534 commands | accepted | 1.99 ms | within budget |
| `Scene::decode`, 49,931 `FillRect` (1 MiB) | accepted | 2.63 ms | within budget |
| `rebuild_scene` at scale | - | not measured | flagged |
| Aggregate retained-layout footprint (F9) | not directly bounded | not measured (this is a containment/memory finding, not a per-op latency one) | n/a |

The one solidly confirmed "expensive operation" is the single unbroken word at
the byte cap. Everything else that looked expensive in the first pass either
turned out cheap on a proper warm-repeated measurement, or is currently
unverified rather than confirmed.

## Recommended action order (revised, supersedes the original G1-G4 list)

The original G1-G4 list is not adopted as-is. F1's weakened cost claim and F2's
likely-wrong `CreateLayout` concurrency premise mean two of the four gates
were justified by numbers that didn't hold up, and F9 — found only during
review — is a more serious containment hole than anything the original list
addressed. Priority order:

1. **Fix F9's accounting.** Change the admission check
   (`presentation_host.rs:55-60`) to count every slot belonging to this
   generation that `collect()` hasn't actually freed, not just
   `guest_lease == true` ones — i.e., drop the `&& slot.guest_lease` term from
   the filter. This closes the loophole without changing `collect()`'s
   freeing condition (still correctly waits for `strong_count == 1`), so a
   Surface-retained layout keeps counting against the generation's budget for
   exactly as long as it's actually resident.
2. **Add an aggregate retained-Surface budget** (total bytes/commands/resource
   references across all of one window's `surface_scenes`, not just the
   1 MiB-per-request cap), admitted the same way an individual scene is:
   compute the prospective total before accepting a replacement, refuse and
   preserve the old scene if it would exceed the budget. Choose the number
   from measurement (item 4), not a guess.
3. **Prove or disprove F2's `create-layout` ABI-serialization claim** with an
   actual hostile `wasm32-wasip2` guest component: attempt N overlapping
   `create-layout` calls, record the actual maximum concurrently outstanding
   at the `PresentationSink`. If it reaches 1, no host-side gate is needed for
   `CreateLayout` and that part of the original G2 is dropped. If it reaches
   N, add the single-flight guard after all.
4. **Extend the latency benchmark**, not ad hoc timing: add a dimension
   varying retained-but-unrelated Surface count (1/8/32/128/256) while
   updating one small Surface, measuring admission, `stage_scene`, and
   `rebuild_scene` separately, plus varying content size within the unrelated
   Surfaces. This answers F4 and gives real numbers for item 2's budget.
5. **Do not add G1 (the hard-break pre-scan) on its original justification.**
   No demonstrated expensive-and-refused input currently exists for it to
   close. It remains a harmless, cheap, structurally-correct thing to add on
   general principle (never do avoidable work before a bound check that could
   reject cheaply), but it should not be described as closing a proven attack.
6. **Do not add G3 (rolling wall-clock presentation budget) yet.** Correct
   objections: it makes the same valid request's outcome depend on machine
   speed and current load, which is a real API-quality problem, not just a
   style preference; and it only prevents the *next* call, doing nothing about
   whichever single call is already 4.75-7.29 ms underway. Measure actual
   input starvation under a real expensive-layout-request loop plus real
   native input pressure first (this is what F3's "one thing worth measuring"
   note above is asking for). If that measurement shows unacceptable
   starvation even with `pump_bounded`'s existing return-immediately-after-one
   -expensive-item behavior, the more principled fix is moving shaping off the
   winit-owning thread entirely — a bigger change, and not one to make without
   this measurement in hand either.
7. **Do not add G4 (proactive queue drain on retire).** Not justified — at
   most 256 cheap stale refusals, and the channel type doesn't obviously
   support selective removal without breaking FIFO order for other
   generations.
8. Once 1-4 are done and measured, revisit whether `MAX_LAYOUT_TEXT_BYTES`
   itself should shrink — this is the existing "provisional... pending the
   documented 4/8/16 KiB reference benchmark" question from `docs/PHASE-3.md`,
   which this audit is a data point for, not a reason to invent a new
   mechanism ahead of it.

## Explicitly not worth adding

* No content-based or "does this look hostile" classification of text or
  scenes — every gate above is mechanical, and this would be the first place
  host code started interpreting guest data.
* No general CPU-time metering/billing system for guest operations.
* No preemptible/cancellable shaping (patching Parley, or a killable worker
  thread with a hard timeout) — disproportionate given the confirmed worst
  single operation is ~7 ms, and F6 accepts a bounded tail on teardown.
* No moving shaping/decode to a separate thread pool with async completion —
  not ruled out permanently (see action item 6), but not justified without
  the starvation measurement that would make the case for it.
* No cross-window/cross-generation fairness scheduler — no evidence of a
  cross-window contention problem.
* No adaptive/heuristic tuning of the size constants — keep bounds static and
  measurement-derived, per `docs/baselines/PERFORMANCE.md`: "do not optimize
  the constant before the model is known."
* **A separate, explicit distinction worth keeping in mind for all of the
  above**: this document is a service-containment gate ("can a hostile guest
  monopolize or exhaust the host with legal requests?"), not the product
  latency gate ("does normal typing hit p95 <= 5 ms?"). F1's confirmed 4.75-
  7.29 ms worst-case legal operation does not by itself mean ordinary typing
  latency fails — a real editor should be shaping small visible slices and
  reusing layouts, not repeatedly re-shaping 4 KiB unbroken runs. The real
  Scratchpad T0-to-rasterized-pixels benchmark is the one that answers the
  product question; this audit answers a different one.

## Mutant / test plan (revised)

| # | Mutant | Caught by |
|---|---|---|
| T1 | F9's fix is reverted (filter re-adds `&& slot.guest_lease`) | Create a layout, attach it to a Surface, drop the guest handle, then attempt `MAX_LIVE_LAYOUTS` more creates in the same generation; assert the count including the Surface-retained one is enforced (the extra creates are refused once the true resident total, not just the open-handle total, reaches the cap). |
| T2 | The aggregate retained-Surface budget (action item 2) is bypassed by spreading bytes across many Surfaces instead of one | Populate N Surfaces each under the per-request cap but summing over the aggregate budget; assert the update that crosses the aggregate threshold is refused and the previous scenes are preserved unchanged. |
| T3 | F2's `create-layout` ABI-serialization claim is wrong (a real hostile guest achieves >1 concurrent) | The hostile-guest test from action item 3, made permanent: assert max-observed-concurrent `CreateLayout` at the `PresentationSink` is exactly 1. If this test ever fails, action item 3's conclusion was wrong and the single-flight guard needs adding after all — this test is the tripwire for that. |
| T4 | `TooManyClusters` becomes reachable again after a future change to the constants | Assert `MAX_LAYOUT_CLUSTERS >= MAX_LAYOUT_TEXT_BYTES` as an invariant (or, if they're ever allowed to diverge, add a real reachability test at that point — don't leave the check silently dead). |
| T5 | `Scene::decode`'s command-count check moves after the loop | A malformed scene claiming a huge header count with a truncated body must fail fast, not attempt to decode as many commands as the truncated bytes allow. |
| T6 | `stage_scene`'s `layouts[layout_slot as usize]` reached with a slot not validated against the actual slice length at the call site | Fuzz/property test for `resource_count` vs. actual `layouts.len()` drift; assert no panic. Adjacent to CPU-DoS scope but a real host-crash vector worth its own test. |
| T7 | `pump_bounded`'s deadline check removed | Extend the existing `pump_respects_its_time_budget` (`bridge.rs:1255`) to exercise a presentation item, not only commits, if it doesn't already. |
| T8 | `pump_bounded`'s item-count check removed | Extend `pump_respects_its_item_budget_and_schedules_continuation` (`bridge.rs:1233`) the same way. |
| T9 | F4's cost characterization, once measured (action item 4) | Not written yet, deliberately — no gate is recommended for F4 until it's measured. The benchmark extension is the prerequisite; its own regression test is the right T9 once it exists. |

Scope note: no change proposed here touches `instar-editor-core` or the
Phase 3 guest-side proof. Every finding and every recommended action is
host-side and mechanical.
