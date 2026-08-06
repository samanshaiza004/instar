# Gate R5 — Vello CPU adoption audit

Date: 2026-08-05

R5 is the adoption gate for the Vello CPU renderer with the existing
Swash-to-AlphaMask text path. It is intentionally separate from R4 GlyphRun
evaluation. R5 must not enable the `glyph-run` feature or require native Vello
text rendering.

## Current State

The production path now defaults to Vello CPU:

- unset `YOUTH_RENDER_BACKEND` selects Vello CPU;
- `YOUTH_RENDER_BACKEND=vello_cpu` selects the Vello CPU renderer explicitly;
- `YOUTH_RENDER_BACKEND=legacy` explicitly selects the legacy renderer;
- any other explicit value fails before the event loop starts.

The Vello path already uses the shared `PaintScene`, reusable RGBA8 target,
coupled `RenderContext`/`Resources`, direct softbuffer conversion, strict final
alpha validation, and structured stage timings. The legacy backend remains a
reference oracle and is not being removed during this audit.

## Evidence Collected

- `youth-desktop`: 63 focused tests pass, including bounded legacy/Vello
  fixture comparison and explicit legacy `UnsupportedGlyphRun` behavior.
- `youth-paint`: 9 tests pass.
- `youth-render-vello-cpu`: 18 default-feature tests and 26 `glyph-run`
  evaluation tests pass.
- Opt-in native Vello window smoke passes on the current macOS host.
- Current optimized launcher snapshot: 30,075,392 bytes.
- One local cold `youth --help` process: 1.15 seconds wall-clock.
- Conversion benchmarks cover all required resolutions and direct-buffer versus
  extra-copy paths.

These are local observations, not cross-platform certification or deltas from
the pre-Vello baseline.

## Adoption Bar

The code default has flipped, but R5 sign-off remains conditional on all of
these being evidenced:

- Calculator, Timer, Todo, Scratchpad, fault overlay, focused controls, editor
  selection/cursor, scroll clipping, nested clips, and all required scale
  factors pass visual comparison.
- Layout bounds, hit testing, accessibility bounds, editor viewport, semantic
  tree, and focus state remain unchanged.
- Linux, Windows, and macOS native smoke and real-hardware interaction pass.
- p50/p95/p99/max measurements exist for cold first frame, idle repaint,
  typing, selection drag, scrolling, resize, scale transition, fault overlay,
  and glyph-heavy editor workloads.
- Package-size, launcher startup, first window, first complete frame, and first
  editable frame deltas are measured against the pre-Vello baseline.
- No unexplained renderer faults or non-opaque final pixels occur, including
  repeated resize and DPI transitions.

## Flip Procedure

The adoption change is intentionally small and is now applied:

1. The unset selector resolves to `RenderBackend::VelloCpu`.
2. Keep explicit `legacy` as a diagnostic/reference override.
3. Run the complete workspace, visual, native, and cross-platform matrix.
4. Keep the legacy backend for one release cycle as a bounded oracle, then
   retire it according to the time-boxed R5 decision.

The default is now Vello on the current code path, but the required
Linux/Windows hardware and full workload measurements are not available on
the current host, so this document does not claim final cross-platform R5
sign-off.
