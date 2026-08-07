# Instar overhead: profiles A–D

What each layer of Instar costs, measured rather than estimated (WP9).

> **These are discovery metrics, not targets.** From `PHASE-1.md`'s measurement
> policy: *do not invent targets before measuring the actual baseline.* Nothing
> here is asserted by any test. The point is to know what the design costs, so
> that a later change which doubles it is visible instead of gradual.

## Method

`crates/instar-shell/src/bin/overhead.rs`, run as:

```bash
cargo run --release --bin overhead
```

Four stages in **one process**, each measured at a checkpoint:

| Stage | What is running |
|---|---|
| baseline | the process before any Instar exists in it |
| **A** | kernel + guest, settled — no layout, no scene, no renderer |
| **B** | A + host: layout, routing, scene lowering |
| **C** | B + font + Vello CPU, one frame rasterized |
| **D** | C driven through 100 click/commit/render cycles, then settled again |

The stages accumulate on purpose: B is A plus a host and cannot be had without
one. Each stage's cost is its delta from the previous checkpoint.

### Why one process rather than four

The first version ran each profile in its own process and compared resident
sizes. **The numbers were nonsense.** The same profile measured between 22 MB
and 51 MB across three consecutive runs, and profile C came out *smaller* than
profile A:

```text
        run 1     run 2     run 3
A       49104     50688     23888
B       22656     22240     36880
C       51120     35488     29744
```

Nothing was wrong with the program. Cross-process RSS on a loaded machine
mostly measures when the kernel last reclaimed pages. Measuring one process as
it grows removes the reclaim boundary between the numbers being compared — and
is also the figure the question was actually asking for.

Recorded here because the failure is more instructive than the fix: a
measurement can be stable, repeatable, cheap to collect, and still be of
something other than what you meant.

## Results

Apple Silicon, macOS 15 (Darwin 25.5.0), release profile, 2026-08-07.
Three runs; deltas below are representative and their spread is noted.

### Memory and threads

| Stage | RSS | Δ from previous | Threads | Spread of Δ across 3 runs |
|---|---:|---:|---:|---|
| baseline | 9.0 MB | — | 1 | identical every run |
| **A** kernel + guest | 50.4 MB | **+41.3 MB** | 11 | 40.9–41.3 MB (±1%) |
| **B** + host | 50.5 MB | **+96 KiB** | 11 | 96 KiB, *identical every run* |
| **C** + renderer + font | 52.0 MB | **+1.5 MB** | 11 | 1.2–2.4 MB |
| **D** after 100 cycles | 52.1 MB | **+48 KiB** | 11 | −96 KiB to +48 KiB |

### Time

| What | Measured |
|---|---|
| A — guest start to first commit | 51 ms |
| B — metrics to laid-out, lowered scene | 177 µs |
| C — cold start through first rasterized frame | 55 ms |
| C — first frame alone | 0.55 ms |
| D — slowest full cycle (click → commit → layout → scene → raster) | 0.45 ms |
| D — slowest frame alone | 0.25 ms |

### Idle

Every stage was observed for 3 seconds after settling:

| Stage | Wakes | Commits |
|---|---:|---:|
| A | 0 | 0 |
| B | 0 | 0 |
| C | 0 | 0 |
| D | 0 | 0 |

Idle is measured as *work*, not as CPU percentage: "0.0%" from a sampler
measures the sampler's resolution as much as the program. These count what the
runtime was actually asked to do. The predecessor's 10 ms epoch ticker would
have produced roughly 300 wakes per window.

### Sizes

| Artifact | Size |
|---|---|
| `instar` (release, macOS arm64) | 23.0 MB |
| `counter.wasm` (release) | 116 KB |
| `counter.wasm` (debug, as build scripts produce it) | 4.0 MB |
| `hostile.wasm` (debug) | 4.2 MB |

## What the numbers say

**Wasmtime is the cost.** 41 MB of the 43 MB Instar adds is stage A — a
component runtime with JIT-compiled code, its own memory pool, and 10 threads
appearing where there was 1. Everything Instar itself contributes is inside the
remaining ~1.6 MB.

**The orchestration layer is nearly free, and exactly reproducible.** 96 KiB
for Taffy's arena, the `LayoutSnapshot`, and a lowered `PaintScene`, to the
kilobyte, on every run. Layout plus lowering takes 177 µs.

**Presentation costs about 1.5 MB**, most of it the 614 KB frame buffer, the
180 KB font file, and the renderer's context and glyph atlas.

**Nothing accumulates.** After 100 complete cycles — 100 guest wake-ups, 100
commits, 100 layouts, 100 lowerings, 101 rasterized frames — the process is
within 48 KiB of where it started, and one run finished 96 KiB *below* it. This
is the number that mattered most: a runtime keeping a little of every
interaction is one that dies after an afternoon.

**A settled Instar does nothing at all.** Zero wakes and zero commits across
every stage, which is the premise the whole project was built to test.

## Against the predecessor

`docs/baselines/managed-youth-final/` holds the pre-rewrite baseline. It is
**partial** — build and size numbers were captured, runtime measurements were
not — so only the size comparison is like-for-like:

| | Youth (`managed-youth-final`) | Instar (Phase 1) |
|---|---|---|
| Desktop binary | `youth-desktop` 25.6 MB | `instar` 23.0 MB |
| CLI binary | `youth` 30.1 MB | — (`instar` is both) |
| Release build, full workspace | 42 min 17 s | 3 min 01 s |

The build-time difference is not a like-for-like measurement either — Youth's
workspace was larger and used fat LTO — but a 14× gap is worth recording.

**Youth's runtime numbers were never captured**, so there is no honest
comparison for memory, startup, or idle. What can be said is what the two
designs do while idle: Youth ran a 10 ms epoch ticker thread and a polling
loop, and Instar's idle counters above are zero.

## What is not measured here

- **Windows.** RSS and thread count are read from `/proc` on Linux and `ps` on
  macOS. Windows needs a Win32 call and therefore a dependency, and a tool that
  adds a dependency to the thing it measures has a conflict of interest. The
  design is identical across platforms; two are enough to learn from.
- **The compositor.** Everything up to `softbuffer`'s `present()` is measured;
  what the window server does with the buffer is not. See the manual smoke test
  in `PHASE-1.md`.
- **Sustained load over hours.** 100 cycles catches per-interaction leaks. The
  1,000-generation soak in `instar-kernel` covers lifecycle churn. Neither is a
  soak test in the sense of running overnight.
