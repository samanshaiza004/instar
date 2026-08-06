# Gate R3 — selectable Vello CPU presentation, direct softbuffer bridge

Date: 2026-08-05

R5 later adopted the Vello CPU path as the unset/default backend. The selector
and direct-buffer architecture described here remain valid; references below
to Vello being opt-in describe the R3 implementation state before that flip.

Scope of R3 (on top of the uncommitted R1+R2 worktree):

1. **The converter moved to the presentation boundary.** `rgba8_to_rgbx32`
   (and its error variants/tests) left `youth-paint` and now live in
   `crates/youth-desktop/src/softbuffer_bridge.rs` as
   `convert_rgba8_to_rgbx32` with a presentation-specific `SoftbufferError`.
   `youth-paint` now contains **no softbuffer/presentation-format
   knowledge**: its `PaintError` is focused on paint/backend errors only, and
   no softbuffer dependency was (or is) added to it. The bridge converts a
   caller-owned premultiplied RGBA8 `[r, g, b, a]` row-major buffer plus a
   `PhysicalSize` directly into `&mut [u32]` numeric `0x00RRGGBB` words with
   no allocations, exact source/destination length checks, host-endianness-
   independent shift packing, and explicit `NonOpaquePixel` rejection.
2. **The criterion benchmark moved to the presentation boundary**:
   `crates/youth-desktop/benches/softbuffer_conversion.rs` (criterion added
   as a dev-dependency of `youth-desktop` only; the old backend bench and
   criterion dev-dependency were removed from `youth-render-vello-cpu`).
3. **An opt-in real Vello path in `native.rs`** — not the default, and not a
   flip (that is Gate R5, not R3). `YOUTH_RENDER_BACKEND=vello_cpu` selects
   it; anything else stays on the legacy comparison path.
4. **Debug-level stage timing** for the Vello path using structured fields.
5. **Scene-opacity validation** at the paint/bridge seam.
6. This note.

The pinned `vello_cpu = "=0.1.0"` path validates physical dimensions before
narrowing them. Its internal u16/depth-bucket arithmetic makes `65_280` the
largest safe dimension for this pin; `65_535` and `65_536` are rejected with a
typed backend-limit error rather than reaching an upstream panic. Revisit this
guard only as part of a reviewed Vello version upgrade.

## Render-backend selector

- Env var: `YOUTH_RENDER_BACKEND`. A development/certification switch, not a
  permanent public production interface: its values are not stable, and an
  unrecognized value fails closed.
- Unset, or the value `legacy`: the legacy `FrameBuffer` comparison path
  (unchanged R0 behavior and hashes; still uses its existing
  `copy_from_slice` presentation copy).
- `vello_cpu`: the opt-in Vello CPU path (persistent `VelloCpuBackend` +
  reusable premultiplied RGBA8 `RenderTarget`, direct-buffer conversion, no
  `copy_from_slice`).
- Any other explicit value is a **startup/configuration error**
  (`DesktopError::Arguments`) naming the offending value and the variable --
  fail closed, never warn-and-fallback, so a typo cannot silently launch the
  wrong renderer. The selector is resolved **before the event loop is
  created**, so invalid configuration fails at `run`/`run_capsule_launch`
  time rather than presenting a window.
- The parse (`native::parse_render_backend`) is pure and unit-tested for
  unset/`legacy`/`vello_cpu`/invalid without touching the process
  environment; the env read happens exactly once, at app construction, and
  is stored on `NativeApp`.

## Direct-buffer flow (Vello path)

Per frame, `native.rs::present_vello`:

1. Builds the same `PaintScene` from the same tree/layout/state producer the
   legacy path uses (`raster::build_scene`, now `pub(crate)`), including the
   scale-factor finite/revision checks and the scene-opacity validation.
2. Validates the scene-opacity contract at the bridge seam
   (`softbuffer_bridge::validate_scene_opacity`).
3. Renders into the reusable premultiplied RGBA8 `RenderTarget` with the
   persistent `VelloCpuBackend` via `render_into_timed`, which validates the
   size against the renderer's u16 limit, recreates the backend's render
   context **and its coupled `Resources`** whenever the physical size
   changes, and resizes the caller's target in place.
4. Resizes the softbuffer surface **before** acquiring the buffer (so the
   acquired buffer is exactly `width * height` words), acquires it, and
   converts `target.pixels()` **directly into `&mut buffer[..]`** via the
   bridge — no intermediate `Vec<u32>`, no `copy_from_slice`.
5. Presents.

The target/backend are created lazily on the first Vello frame and resized
through the existing resize/scale-factor flow (scale-factor changes already
trigger relayout + redraw, which reach `present_vello` with the new physical
size).

## Allocation wording

The Vello path is **not** an allocation-free pipeline. The bridge conversion
(`softbuffer_bridge::convert_rgba8_to_rgbx32`) is allocation-free, and both
the Vello `RenderTarget` and the softbuffer buffer are reused across frames,
but platform internals (softbuffer's buffer re-acquisition and remap, the
windowing system, and Vello's context recreation on a size change) may
allocate or remap memory as they see fit. `backend_resize_us` exists so the
size-change recreation is separately attributable.

## Error / no-present behavior

- A bridge or render error (scene-opacity validation failure, render failure,
  length mismatch, or any `NonOpaquePixel` — including a late one after which
  the destination may already be partially modified) means the frame is
  **not presented**: `buffer.present()` is never reached, the previously
  displayed frame stays on screen, a `tracing::error!` names the failure, and
  the native fault path (`fault = "renderer_failure"`) is used just like a
  legacy render failure.
- The caller must never present after any bridge error; the bridge
  documents that its length checks are atomic but a late `NonOpaquePixel`
  failure may leave earlier destination words written.

## Opacity protections (exactly two, no redundant pass)

There are exactly two opacity protections, unchanged in shape from R3:

1. **One scene-contract check** at the paint/bridge seam: the scene's first
   command must be an opaque `Clear` (`validate_scene_opacity`). The
   intentional R0 fault-overlay *second* `Clear` is the documented exception
   to a hypothetical exactly-one-clear rule and is deliberately not reworked.
2. **Final per-pixel authority** in the bridge: `convert_rgba8_to_rgbx32`
   rejects any non-opaque pixel that survives compositing
   (`SoftbufferError::NonOpaquePixel`), so nothing translucent can reach the
   alpha-less `0x00RRGGBB` output.

No redundant opacity passes were added.

## Timing fields (Vello path, debug level)

`present_vello` emits one `tracing::debug!` event (target
`youth_desktop::render`) with structured fields, in microseconds:

- `scene_us` — PaintScene construction (including the opacity scene-contract
  check).
- `backend_resize_us` — backend preparation inside `render_into_timed`:
  only when the physical size changed — recreating the
  Vello render context and its coupled `Resources`, plus resizing the
  reusable `RenderTarget` in place. Zero on a steady-state frame at an
  unchanged size.
- `render_us` — Vello CPU scene recording and rasterization into the
  reusable target (reported by `render_into_timed` separately from
  `backend_resize_us`).
- `surface_resize_us` — the softbuffer `surface.resize` call, measured
  separately from acquisition so platform remap cost is attributable.
- `acquire_us` — softbuffer `surface.buffer_mut()` acquisition.
- `convert_us` — the allocation-free RGBA8 → 0x00RRGGBB conversion directly
  into the acquired buffer.
- `present_us` — softbuffer `present()`.
- `total_us` — total Vello present pipeline time.
- `unaccounted_us` — `total_us` minus the sum of the accounted stage fields.
  The stages are sequential sub-intervals of the total, so this is
  non-negative; it is clamped with `saturating_sub` against sub-microsecond
  rounding drift.

Debug level means idle normal operation does not print these by default. The
legacy path keeps its existing `info`-level `desktop.present` span and no
stage timing, so legacy timing behavior is unchanged.

## Legacy comparison status

- Legacy is still the default and still produces the exact R0 frame hashes
  (all `raw_frame_fixtures_are_deterministic` pins and every other existing
  R0/R1/R2 raster/paint test are untouched).
- The legacy comparison branch retains its existing `FrameBuffer`
  `copy_from_slice` presentation copy **while it remains available**;
  only the Vello path removes the extra copy. This is documented in
  `present_legacy`.
- The `convert_plus_copy` benchmark exists precisely to keep that avoidable
  copy measurable: conversion + `copy_from_slice` is reported separately from
  the direct-buffer conversion.

## Benchmark reporting (criterion 0.5.1)

Criterion 0.5.1 in this workspace ships only
`Bytes`/`BytesDecimal`/`Elements` throughput — no `ElementsAndBytes` — so
this benchmark uses truthful compatible groups instead of inventing a
combined metric:

- `convert/pixels` — `Throughput::Elements(pixels)`; variants `reused_vec`
  (reused destination `Vec<u32>`) and `acquired_slice` (acquired-like direct
  `&mut [u32]`, the exact bridge call the native Vello path makes).
- `convert/traffic` — `Throughput::BytesDecimal(pixels * 4)` (RGBA8 source
  traffic); the same two variants.
- `convert_plus_copy/traffic` — `Throughput::BytesDecimal(pixels * 8)` (RGBA
  source bytes read + packed words copied); the conversion plus the extra
  `copy_from_slice`, exposing the avoidable copy.
- `reject/pixels` — `Throughput::Elements(pixels)`; correctness-path
  rejection near the beginning (`reject_first_pixel`) and near the end
  (`reject_last_pixel`, the worst-case scan that partially modifies the
  destination).

All buffers are allocated once and reused; no benchmarked iteration
allocates. Exact sizes: 640x480, 1024x720, 1920x1080, 3840x2160 (rejection
cases at 640x480).

## R2 note

Gate R2's converter now lives at the presentation boundary (see item 1); its
`rgba8_to_rgbx32` name, alpha policy, 32-bit-safe oversized-size handling, and
unit-test coverage all moved with it, plus a new partial-destination-
modification test. The R2 benchmark target moved from
`youth-render-vello-cpu` to `youth-desktop` with the converter.

## Remaining R3.3–R3.5 certification work (not done here)

### Local macOS snapshot

The current optimized `youth` launcher is 30,075,392 bytes on this machine,
and `youth --help` took 1.15 seconds wall-clock in one cold process run. These
are raw single-host observations, not dependency deltas or first-window/first-
editable-frame measurements, so they must not be used as the R5 package or
startup decision.

- **R3.3 — real-hardware fixture certification.** No real hardware claims are
  made in R3. The Vello path must be certified on macOS (arm64), Windows, and
  Linux with canonical deterministic fixtures (the same bounded-region policy
  the existing live-editor tests use) before it can be considered more than a
  selectable spike. Nothing in this gate ran on hardware beyond the local
  build/test suite.
- **R3.4 — idle/typing/scroll measurements.** Full idle, typing, and scroll
  latency measurements for the Vello path are **not** run or claimed here.
  The debug timing fields above exist so such a measurement pass has data to
  collect, but no numbers from a full pass are recorded.
- **R3.5 — default-flip decision.** The Vello path remains opt-in; the
  decision to make it the default (and the corresponding removal of the
  legacy comparison branch's copy path) belongs to a later gate after R3.3
  and R3.4 produce evidence.
