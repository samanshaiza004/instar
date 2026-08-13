# Instar Guide

Instar is an experimental native host for applications compiled as WebAssembly
components. An application—called a **guest**—describes a semantic interface.
Instar owns the native window, layout, input, accessibility, rendering, and the
boundary around every guest turn.

The fastest path through this guide is:

1. [Check the requirements](getting-started/requirements.md).
2. [Install Instar](getting-started/install.md).
3. [Build and run the included counter](getting-started/first-run.md).
4. [Read the guest walkthrough](getting-started/build-a-guest.md).

> **Developer preview.** The CLI, WIT contract, protocol, and crate APIs can
> change without compatibility. Instar is usable for experiments and for
> learning the architecture; it is not yet a stable application platform.

## The contract in one picture

```text
guest component                         native Instar host
────────────────                       ─────────────────────────────
build semantic snapshot  ── commit ──▶ validate → diff → layout
                                          │
suspend at next-event    ◀── event ─── hit-test ← input ← native window
                                          │
                                      accessibility + pixels
```

The guest never owns a window or a rectangle. The host never owns application
state. That division is the organizing idea behind the entire project.

## What works today

- WebAssembly Component Model guests targeting `wasm32-wasip2`.
- Semantic retained UI snapshots with rows, columns, stacks, text, buttons,
  scrolling, visibility, clipping, alignment, and flex sizing.
- Host-local pointer, keyboard, focus, scroll, and accessibility behavior.
- Native CPU rendering on macOS, Linux, and Windows.
- Guest generation teardown and a host-owned crash surface.
- An experimental host-owned text resource subsystem in active development.

## What does not exist

There is no `instar new`, `instar build`, `instar dev`, `instar package`,
`instar inspect`, `instar validate`, or `instar doctor`. There is no Instar app
manifest or bundle format. The only command is `instar run`, deliberately.

For the decisions behind that narrow surface, see the
[project status](project/status.md) and the repository’s design ledger.
