# Performance baseline

What Instar costs now, how it was measured, and what CI does and does not
enforce. Numbers here are the ones later work is compared against, so each says
where it came from — an unattributed number is a number nobody can reproduce or
retire.

## How to reproduce

```bash
INSTAR_LATENCY_GATE=1 cargo test --release -p instar-host --test bridge -- --nocapture
```

Release, and on a host doing nothing else. Both matter, and the second is not a
formality: the same suite on the machine these numbers came from reports p95
5.2 ms idle and 17.9 ms with a build running in another terminal.

## Warm click round trip

Click → guest event → guest commit → host diff → layout → lower → accepted.
1,000 cycles through a real `wasm32-wasip2` guest on a real second thread.

| | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|
| Phase 1, placeholder metrics | 206 µs | 215 µs | 225 µs | 475 µs |
| Parley, first cut | 46 ms | 82 ms | 134 ms | 301 ms |
| **Parley, after the cache fixes** | **4.94 ms** | **5.75 ms** | **11.5 ms** | **105 ms** |
| Stage 2 complete, idle machine | 4.86–4.98 ms | 5.15–5.34 ms | 5.31–5.55 ms | 5.5–8.8 ms |
| Package C complete, load avg 8.5 | 4.93–4.98 ms | 5.58–5.85 ms | 5.93–6.46 ms | 6.3–6.9 ms |

The Phase 1 row is not a regression target. It measured fake fixed-pitch
metrics against no real font; the honest comparison is the second row onward.

**Package C cost nothing measurable either**, which is the point of measuring
it: style should not touch the click path, and a style vocabulary that made
clicks slower would mean a paint-only change was reaching layout after all —
the same regression `TextStats` guards from the other side. The row was taken
at a load average of 8.5 rather than on an idle machine, so it is a
conservative reading rather than a best case.

**Stage 2 cost nothing measurable.** The last row was taken interleaved against
the commit before the generational-key work, alternating runs so drift hit both
equally:

```text
         p50                      p95                  p99
parent   4.983 / 4.954 / 4.953    5.34 / 5.18 / 5.30   5.67 / 5.31 / 5.55
stage 2  4.959 / 4.857 / 4.981    5.27 / 5.15 / --     5.48 / 5.32 / --
```

Indistinguishable. The first three readings taken *without* that control looked
like a regression — p50 6.66 ms — and were entirely a load average of 23 from
concurrent builds. Same lesson as the 386 ms that release measured at 5 ms:
instrument the thing, in the profile that matters, on a machine doing nothing
else.

## Gate thresholds

| | target | headroom over measured |
|---|---:|---:|
| p50 | 7 ms | 1.42× |
| p95 | 8 ms | 1.39× |
| p99 | 16 ms | 1.39× |
| max (`PROMPT`) | 250 ms | 2.4× |

`max` is a deadlock and outlier guard, not a performance target, which is why
it sits three orders of magnitude above p50.

`p50` was 5 ms — the measurement rounded up to the next integer, 1.01× headroom
where its neighbours had 1.39×. Not a stricter policy for p50, an oversight,
and one that went unnoticed because the release assertions had never run. A
ceiling a healthy machine grazes is a flake generator rather than a gate.

## What CI enforces

```text
cargo fmt --all --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
cargo test --workspace                          debug
guest workspace fmt + clippy                    wasm32-wasip2
cargo build --release -p instar-host --tests    compiles the release assertions
```

**CI does not arm the latency gate.** GitHub-hosted runners document resource
classes, not deterministic latency, and asserting millisecond percentiles on a
shared VM would be inferring more than the platform promises — the failures
would be about the runner, and a suite that reddens for reasons nobody can act
on trains people to ignore it.

What CI *does* do is build the release tests, so the assertions cannot rot the
way they did once before. Arming the gate belongs on a controlled performance
runner — self-hosted, or a deliberately chosen larger runner — as a separate
job whose failures mean something.

## Snapshot transport, at 4,000 nodes

One changed text leaf, Apple Silicon, release:

| layer | cost | scales with |
|---|---:|---|
| decode | 108 µs | tree, inherent |
| validate | 177 µs | tree, inherent |
| diff | 523 µs | tree, flat in change size |
| layout | 11,356 µs | **tree** |
| lower | 900 µs | **tree** |
| raster | 15,040 µs | **tree** |

Transport is ~0.8 ms of a ~27 ms frame — 3%. The expensive work is host-side
recomputation, which is why the architecture stays `full snapshot → host diff →
incremental host work` rather than pivoting to guest-sent deltas. One changed
leaf and four hundred changed leaves still cost the same; closing that is not
done.

The one target already met: an identical re-commit costs 0.8 ms instead of
27 ms, because an empty `ChangeSet` skips layout, lowering, and raster
entirely.

`MAX_NODES` stays 4096 for that reason. Raising it would expand supported input
faster than supported performance.

## Text shaping

One warm click, ten text-bearing nodes, one changed label:

```text
rebuilt       1
relinebroken  1
reused        9
extracted     1
```

`rebuilt == 1` is the property that matters, and it is asserted rather than
eyeballed. Two defects found by this instrument and not by any timing:

1. `finalize` re-extracted unconditionally, so a detected reuse still paid full
   extraction. Reuse: 2.53 ms → 83 ns.
2. `measure` line-broke for every constraint Taffy probed — 29 re-breaks for 10
   nodes — and each invalidated the extracted artifact, so one changed label
   re-extracted every node on screen. Round trip: 43.9 ms → 5.36 ms.

Shaping itself was never the problem: an explicitly-named monospace face
rebuilds in ~370 µs.

## Open

- **A 105 ms max** was recorded once in the Stage 1 ledger, 20× p50 and
  unexplained by any measured layer. Idle Stage 2 runs top out at 8.8 ms, so it
  may already be gone with the load that caused it. Needs an outlier-only trace
  including thread wake delay before anyone calls it noise or calls it fixed.
- **`system-ui` selection** costs ~7× an explicit face per shape, and
  **`TextContext::new()` costs ~342 ms** of system font enumeration. Neither is
  on the warm path, so both are startup debt to be re-baselined rather than
  fixed on the strength of old numbers.
- **The `MAX_NODE_IDS` scan** is O(observed ids) per commit — negligible for a
  guest holding tens of ids, ~65k entries for one that has burned the budget.
  Not measured, and not worth measuring until a guest churns ids at all.

## Debug builds of a software renderer are not indicative

Measured on the Gallery, same guest, same 960x640 window:

```text
debug     44-119 ms/frame
release     5-7  ms/frame
```

Roughly 10-20x, and it is entirely the rasterizer: `vello_cpu` in a debug
profile is unoptimized software rendering per pixel. Cost then scales with the
pixel count, so a maximized window on a 2x display — around 2.6 megapixels,
four times a 960x640 one — lands near 270 ms in debug and around 25 ms in
release.

Recorded because the first person to open the Gallery full-screen reasonably
described it as "unbelievably laggy", and the answer is the profile rather than
anything in the frame path. Anything judging how Instar *feels* has to be a
release build. The gates in this document already are.
