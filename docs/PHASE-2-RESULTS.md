# Phase 2 results

The frozen state at `instar-phase-2`. `docs/PHASE-2.md` keeps the archaeology —
how each decision was reached, what was wrong first, and why. This is the
answer, with numbers, so a later change has something to be measured against.

## The claim

> A Wasm guest can describe a normal desktop interface declaratively while
> Instar provides retained layout, rendering, scrolling, transient interaction,
> keyboard and focus behaviour, and accessibility — without polling and without
> the guest owning a single pixel.

Held, with one gate outstanding: native accessibility behaviour is verified on
no platform yet. See *What is not closed* below.

## Frozen numbers

Measured at the tagged commit, rustc 1.97.1, cargo 1.97.1, macOS 26.5.2
(25F84), arm64.

### Gate

```text
496 tests passing across the workspace
0 failing, 0 ignored
clippy clean under RUSTFLAGS="-D warnings" --all-targets
rustfmt clean
6 guests build for wasm32-wasip2
```

### Interaction latency

Warm click, guest round trip included. The gate is opt-in
(`INSTAR_LATENCY_GATE=1`) because a judgement needs a host doing nothing else.

```text
        measured        gate     headroom
p50     4.86–4.98 ms    7 ms     1.42x
p95     5.15–5.34 ms    8 ms     1.39x
p99     5.31–5.55 ms   16 ms     1.39x
```

### Distribution size

```text
                     dev        release    release + strip
instar             124.9 MiB    16.8 MiB       13.4 MiB
counter.wasm         4.14 MiB                   0.14 MiB
gallery.wasm         4.28 MiB                   0.18 MiB
calculator.wasm      4.45 MiB                   0.17 MiB
```

### Runtime memory

Every guest, measured against `ResourcePolicy::instar_default()`:

```text
guest                instances  memories  tables   peak bytes  peak table
counter                      3         1       2      1114112         107
gallery                      3         1       2      1114112         121
hostile                      3         1       2      1114112         111
kernel-guest                 3         1       2      1114112         107
kernel-spike-guest           3         1       2      1114112         105

policy ceiling              16         4       8     67108864       10000
```

Roughly 60x headroom on memory and 80x on tables. The table is printed by its
own test and fails if a guest drifts toward the policy, so the limits stay
evidence rather than a guess.

### Protocol

```text
PROTOCOL_VERSION = 8
```

## The supported UI vocabulary

Everything a guest can say. Anything absent is absent on purpose.

```text
nodes       Root  Column  Row  Stack  Scroll  Text  Button

sizing      width/height   Content | Fixed(u16)
            min/max        width, height
            basis          Auto | Fixed(u16)
            grow, shrink   validated f32
            align_self, align_items, justify_content
            padding, gap

presence    display     Normal | None
            visibility  Visible | Hidden
            overflow    Visible | Clip

text        role        SystemUi | Monospace
            size, weight
            align       Start | Center | End

paint       foreground, background
            border (width, colour), corner_radius
            cursor

flags       enabled
```

Host-owned and never on the wire: scroll offsets, focus, pressed state,
hover, scrollbar geometry and style, every rectangle, and the accessibility
tree.

## Open ledger entries

`docs/DESIGN-LEDGER.md` carries four, closed three ways.

```text
1  parent-relative percentage sizing     DECLINED
2  a default control for Enter           DECLINED
3  text alignment                        IMPLEMENTED (H2)
4  nested scrollbar policy               RESOLVED (ScrollbarStyle)
```

Both declines are decisions, not omissions: one application asked for each and
the second independently did not. Recorded so that a third request is
recognisable as new evidence rather than as a rediscovery.

## What is not closed

```text
F4 macOS      formal checklist   PENDING
F4 Windows    Narrator           PENDING
F4 Linux      Orca               PENDING
```

Three defects were found by pointing VoiceOver at the Gallery and all three are
fixed with regressions, and VoiceOver was then observed working — but the
acceptance procedure in `docs/F4-SMOKE.md` has not been run deliberately
against a named build on any platform. `PENDING` means what it says.

Development-side, and explicitly *not* product: `target/` reaches 29 GiB in
ordinary use. `target/debug/build` is 296 MiB after the guest-build fix, so
what remains is `deps`, and the untested lever is dependency debuginfo in the
dev profile.

## The freeze

> No ordinary UI feature enters Instar unless Phase 3, or a real application,
> forces it.

The Gallery and the Calculator have both had their say. The next evidence has
to come from somewhere neither of them could reach — which is what Phase 3 is
for.
