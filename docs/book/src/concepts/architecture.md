# Architecture

The workspace is split so only the smallest protocol surface can reach a guest.

```text
instar-shell ── native event loop, presentation, shipped binary
      │
instar-host  ── routing, commit barrier, scene lowering
   ┌──┼──────────────┐
window  ui         paint ── render-vello-cpu
          │
     ui-protocol ◀──────── guest component

instar-kernel ── Wasmtime generation and async host operations
instar-sdk    ── optional guest-side snapshot and route helper
instar-text   ── host text buffers/views (under active integration)
```

## Crates

| Crate | Responsibility |
|---|---|
| `instar-ui-protocol` | Versioned byte encoding, bounds, node/event types; zero dependencies. |
| `instar-sdk` | Optional guest snapshot builder and semantic event router. |
| `instar-kernel` | Wasmtime engine, generations, async imports, event delivery. |
| `instar-ui` | Retained tree, layout, hit-testing, focus, scroll, accessibility state. |
| `instar-window` | Native event translation and physical/logical conversion. |
| `instar-paint` | Renderer-independent scene and paint commands. |
| `instar-render-vello-cpu` | CPU raster backend. |
| `instar-host` | Main/runtime bridge, policy, validation, presentation lowering. |
| `instar-shell` | Native event loop, surface, font, accessibility adapter, executable. |
| `instar-guest-build` | Reproducible component builds from crate build scripts. |
| `instar-text` | Buffers, views, revisions, editing and selection; experimental. |

## Important dependency rules

- A guest links `instar-ui-protocol` and optionally `instar-sdk`; nothing native.
- `instar-window` never sees semantic node identity.
- `instar-ui` never sees DPI or a native window.
- `instar-host` may lower to paint intent but does not choose a rasterizer.
- Only `instar-shell` combines the renderer and a window surface.

The repository enforces these as dependency-set tests. For the complete
normative explanation, read `docs/ARCHITECTURE.md` in the source tree.

## Commit path

The order is deliberate:

```text
screen generation → decode → validate → apply atomically
→ layout → lower scene → request redraw → reply to guest
```

Screening stale generations first prevents dead guests from consuming parser
and allocation work. Replying after layout means a successful commit is usable
presentation state, not merely accepted bytes.
