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

## I0: the distributable is 13.4 MiB, not 144

Measured at `55996b2`, rustc 1.97.1, cargo 1.97.1, macOS 26.5.2 (25F84),
arm64.

```text
                        dev        release    release + strip
instar                124.9 MiB    16.8 MiB       13.4 MiB
overhead              123.9 MiB
uibench                28.7 MiB

counter.wasm            4.14 MiB                   0.14 MiB
gallery.wasm            4.28 MiB                   0.18 MiB
calculator.wasm         4.45 MiB                   0.17 MiB
```

```text
clean release build     9m 39s
release + strip         4m 27s   (relink after a profile change)
no-op rebuild           0.66s
target/ after both      29 GiB
```

**This is the number that answers the question, and it is a factor of ten
from the one that prompted it.** A 144 MiB debug binary was never evidence
about what Instar ships; `lto = true` and an optimizing pass remove almost all
of it, and `strip = "symbols"` takes another 3.4 MiB. The guests go from ~4.3
MiB to under 200 KiB — a factor of twenty-five.

So the accurate account of the original 144 MiB is:

```text
~108 MiB  unoptimized, un-LTO'd code the release profile removes
~ 19 MiB  Wasmtime capability Instar never asked for (package I2)
~  3 MiB  debug symbols
~ 13 MiB  the runtime
```

Only the second of those was a defect. The first is the dev profile working as
designed, and it is why "Rust makes Instar 144 MB" was the wrong sentence:
Rust's dev profile generates a lot of debug and incremental material, and
Instar was linking a general-purpose Wasmtime including capability it never
configured.

### What this settles, and what it defers

At 13.4 MiB with a JIT compiler linked in, the case for Winch or an AOT
compiler/runner split is now weak on size grounds alone. Both remain
interesting for other reasons — Winch for guest startup latency, AOT for
attack surface — but neither is justified by this table, and the AOT split
would cost Instar the property that `instar run foo.wasm` accepts an
architecture-independent component.

The remaining number worth attention is the 29 GiB `target/`, which is
development ergonomics rather than product. `target/debug/build` is down to
296 MiB after the guest-build fix, so what is left is `deps` — and the
untested lever there is `[profile.dev] debug = "line-tables-only"` with
`[profile.dev.package."*"] debug = false`.

## I1: the dev profile is worth 1.5x, and the 8x was a measurement error

Measured at `62df5bf`, rustc 1.97.1, cargo 1.97.1, macOS 26.5.2, arm64.

An earlier reading suggested this lever was worth roughly 8x — 31 GiB down to
4 GiB. **That number was not real.** The two sides had not built the same
thing: the large figure was a `target/` that had accumulated dev, release,
`dist` and doc artifacts across a day's work, and the small one was a single
dev-profile build. Comparing them measured how much had been built, not what
debug information costs.

Controlled: same revision, same toolchain, same machine, same command, a fresh
`CARGO_TARGET_DIR` for each side, run back to back.

```bash
CARGO_TARGET_DIR=<fresh> cargo test --workspace --no-run
```

That builds 26 test binaries. It is not the 37 suites `cargo test --workspace`
reports, and the difference is not a discrepancy: the other 11 are doc-test
runs, one per library crate, which `--no-run` does not compile. Both sides
build the same 26.

```text
                        A: debug = true      B: line-tables-only
                                                 + deps debug = false
target/                      5.64 GB              3.79 GB    -33%
target/debug/deps            4.50 GB              2.75 GB    -39%
target/debug/incremental     1.34 GB              0.81 GB    -39%

clean build                   260 s                270 s
no-op rebuild                   1 s                  1 s
one-line edit, rebuilt         21 s            18 / 24 / 18 s
panic file:line               yes                  yes
```

The rebuild row is why it has three numbers on the B side. B's first reading
was 37 s, which would have been a 76% regression and a reason to revert. Three
further samples put it at 18-24 s, straddling A's 21 s: the 37 s was an
outlier, and a single unreplicated timing was about to become a decision.

**Keep the profile, and stop looking at dev profiles.** 1.8 GB for no cost is
worth having, backtraces still name the file and line that panicked, and build
times are indistinguishable. But it is a third, not the order of magnitude the
uncontrolled reading claimed, and nothing else here is worth another day.

The honest form of the original observation is that a `target/` directory
grows to tens of gigabytes because it holds every profile ever built, not
because dependency debug information is enormous.

## Phase 3, package A: local text editing

```bash
cargo run --release --example textbench
```

Measured at `e6d8fff`, macOS 26.5.2, arm64. Host-local only: a synthetic edit
into a `TextBuffer`, and the transform of every attached `TextView`. No OS
input, no IME, no shaping, no pixels — package A has no native text input path,
so a number for one would describe machinery that does not exist.

### Latency, p50 / p95 / p99

```text
operation                     1 MiB              10 MiB     5 MiB single line
insert start           0.2/0.3/0.3 µs      0.2/0.3/0.4 µs      0.2/0.3/0.4 µs
insert middle          0.2/0.3/0.3 µs      0.3/0.3/0.4 µs      0.3/0.4/0.4 µs
insert end             0.3/0.3/0.4 µs      0.3/0.8/1.5 µs      0.3/0.3/0.4 µs
delete middle          0.2/0.2/0.3 µs      0.2/0.3/0.3 µs      0.4/0.5/1.1 µs
replace selection      0.2/0.3/1.3 µs      0.3/0.4/0.5 µs      0.3/0.4/0.5 µs
paste 100 KiB           30/68/215  µs       29/44/80   µs       16/34/67   µs
undo                   0.1/0.2/0.2 µs      0.1/0.2/0.2 µs      0.1/0.2/0.2 µs
redo                   0.2/0.2/0.2 µs      0.2/0.2/0.4 µs      0.2/0.3/0.7 µs
```

A tenfold document costs nothing measurable. The 5 MiB single line — the case
that finds a line index quietly assuming short lines — is not distinguishable
from the others either.

### Copying, for exactly one operation

```text
                       payload  undo-kept  materlz  bytes  whole-buffer   alloc
1 MiB  insert middle         1          1        1      0             0  2859 B
10 MiB insert middle         1          1        1      0             0  3179 B
10 MiB replace selection    10        110        1    100             0   386 B
10 MiB paste 100 KiB    102400     102400        1      0             0 409 KiB
```

**`whole-buffer materializations: 0`, across all three documents and all eight
operations.** That is the invariant `instar-text` states, and it is the number
the package exists to produce. `materlz` counts one per edit because an edit
reads back the material it overwrites, for undo — 100 bytes for a 100-byte
replacement, zero for an insertion.

### Memory

```text
document             text     buffer live   journal after 1,000 edits   per view
1 MiB            1024 KiB        1065 KiB                      67 KiB    32-108 B
10 MiB           10.0 MiB        10.4 MiB                      67 KiB
5 MiB one line   5120 KiB        5313 KiB                      67 KiB
```

The journal column is a thousand one-byte insertions into each document and is
the same 67 KiB in all three: undo costs what was typed, not what it was typed
into. A view costs tens of bytes and does not vary with the document; the range
is the registry's hash map growing, not the view.

Adding views does not slow editing: p50 is 0.3 µs at one, two, and eight views
of the same buffer.

### Why these numbers are believable

A benchmark is evidence only if the wrong implementation looks wrong. Running
it against `TextStorage::replace` rewritten as rope → contiguous `String` →
edit → rebuild:

```text
                              healthy      rope -> String -> rope
insert middle, 1 MiB           0.2 µs                    264 µs
insert middle, 10 MiB          0.3 µs                  8,189 µs
1 MiB -> 10 MiB                  flat                       31x
allocated per edit             2,859 B                  26.4 MiB
whole-buffer materializations        0            1 per operation
```

The healthy implementation is flat as the document grows by 10x; the faulted
one tracks document size. That is the discrimination the table has to have
before its zeros mean anything.

Two instruments, because neither is sufficient alone. `instar_text::instrument`
counts contiguous copies made through the crate's API, so it names *who* asked
for the document; it cannot see a copy assembled inside `storage.rs` from
`crop`'s chunks directly. The counting allocator in `textbench` sees any
allocation however it was built, but cannot attribute it — and cannot see bytes
a B-tree moves inside memory it already owns, so it is a lower bound on copying
rather than a total. It is reported as bytes *allocated*, which is what it is.

### What package A has and has not answered

Answered: small local edits show no O(document-size) latency or copying,
nothing on the editing path materializes the document, undo scales with changed
material, and each extra view costs view-sized memory.

Not answered, and not attempted: whether this survives a real keyboard, an IME
preedit, shaping, and a guest that disagrees about the document. Those are
packages B and C.

### B1: the shaping window

```text
document              position     first row   rows   bytes shaped   truncated   p50
1 MiB                 top                  0     23         1426 B          no  0.9 µs
1 MiB                 80% down         13528     25         1550 B          no  1.4 µs
10 MiB                top                  0     23         1426 B          no  0.9 µs
10 MiB                80% down        135298     25         1550 B          no  1.3 µs
5 MiB single line     top                  0      1         64 KiB         yes  0.0 µs
5 MiB single line     80% down             0      1         64 KiB         yes  0.0 µs
```

A tenfold document costs the same window, and row 135,298 costs what row
13,528 costs — `O(rows × log n)`, with no walk of anything above the viewport.
The single line is capped at `MAX_SHAPED_PARAGRAPH_BYTES` and reports
`truncated`, because five megabytes in one paragraph is the case that would
look correct on every other fixture.

This is the window, not the shaping: B1 has no font stack wired to a
`TextView` yet, so glyphs lowered and frame time are not here. Which bytes get
shaped is the architectural claim, and a window that tracked the document
could not be rescued by a fast shaper.

Four injections, all caught: no paragraph cap, a window that always starts at
row 0, a cut that ignores character boundaries, and a `paragraph_at` that falls
back to the nearest paragraph instead of answering `None`. The second is worth
noting — it also showed up as the unit suite going from 0.03 s to 0.83 s, which
is what a document scan looks like from the outside.

### B1b: the window, shaped

```text
document              position   bytes shaped   glyphs   presentation   shape p50
1 MiB                 top              1403 B     1403        285 KiB      194 µs
1 MiB                 80% down         1525 B     1525        115 KiB      212 µs
10 MiB                top              1403 B     1403        106 KiB      202 µs
10 MiB                80% down         1525 B     1525        115 KiB      221 µs
5 MiB single line     top              64 KiB    65536       2306 KiB     6691 µs
5 MiB single line     80% down         64 KiB    65536       2306 KiB     6689 µs
```

Glyphs and presentation memory follow the window rather than the document, and
a deeply scrolled row is proved to reach pixels by a rasterizing test rather
than by a scene assertion — the distinction Phase 2 paid for with the focus
ring, which had a correct scene and an invisible ring for two packages.

Two findings worth carrying into B2.

**Per-window re-shaping is ~200 µs and nothing is cached across frames.** At
8.4 µs per row that is 1.2% of a 60 Hz budget, so it is not urgent, but a
scrolling view currently reshapes every visible row every frame. A per-row
cache keyed by content is the obvious lever if it ever matters.

**The 64 KiB paragraph cap costs 6.7 ms to shape.** The cap does its job — the
number stops growing with the line — but 6.7 ms is a visible hitch, so
"bounded" and "affordable" are not the same claim. Only a viewport's width of
an unwrapped line is ever visible, so a smaller cap is likely right; it is left
at 64 KiB because choosing the number properly needs the horizontal-scroll work
B2 has not done yet.

### The font extraction defect this found

`textbench` was built to catch a `to_string` on the editing path. It found
something else, in code shipped since Phase 1 and on the path of every text
node in the UI:

```text
extract one 61-glyph line        before              after
  shipped monospace face       62 µs / 184,776 B    1.3 µs / 1,056 B
  a system face             2,530 µs / 7,910,720 B  1.2 µs / 1,056 B
```

Two causes, both in `instar-ui::text::extract`. The cache key was a hash of the
entire font file, so producing it read megabytes. And `FontFace::data` is an
`Arc<[u8]>` built with `Arc::from(bytes)` from Parley's `Blob<u8>` — a
different `Arc` type, so every extraction copied the whole font.

The first fix keyed on `Blob`'s own unique id, which makes both costs vanish.
`instar-ui`'s `layout_is_deterministic` rejected it within the hour: two
`TextContext`s each load the face into their own blob, so the same font
produced two different keys and two identical layouts compared unequal.

So the key stays content-derived and the *lookup* became free — the blob id
indexes a cache holding both the hash and the single copy of the bytes, and
the expensive read happens once per face per thread instead of once per
extraction.

Worth recording as a method note: this was invisible to every existing test and
to the warm-click gate, because the keyed path re-extracts only when a string
changes and the gate's guest changes one short label. It took a benchmark that
shapes twenty-three rows at once to make a per-extraction cost visible at all.
