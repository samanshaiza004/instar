# Gate R4 — renderer-neutral GlyphRun command, Vello CPU evaluation path

Date: 2026-08-05

Scope of R4 (on top of the uncommitted R1+R2+R3 worktree):

1. **A real, renderer-neutral `GlyphRun` command with font-resource
   ownership** in `youth-paint` — dependency-free, with no Parley/Swash/Vello/
   Blob type leaking into the scene contract.
2. **An opt-in Vello implementation of it**, only in `youth-render-vello-cpu`
   behind the new `glyph-run` crate feature, using the pinned
   `vello_cpu = "=0.1.0"` `RenderContext::glyph_run` / `GlyphRunBuilder`
   text API.
3. **A host-repeatable comparison fixture** rendering the same real Parley
   glyph positions through the existing Swash-to-AlphaMask producer and the
   Vello GlyphRun path, with bounded-region metrics.
4. **R4 tests** (run with `--features glyph-run`), documentation, and no
   behavior changes to the default feature set, `native.rs`, or the R3 opt-in
   path.

**R4 is evaluation work, not adoption.** Nothing here flips a default,
`native.rs` now selects Vello CPU by default after R5 adoption, while
`YOUTH_RENDER_BACKEND=legacy` remains the diagnostic oracle. Both production
paths remain AlphaMask-only, and **no producer emits `GlyphRun`** into any
scene this gate builds. R4 GlyphRun evaluation remains separate from that
renderer adoption.

## Font-resource data model (`youth-paint`)

New types, all backend-neutral:

- `FontId(pub u32)` — keys the scene's `fonts` collection, referenced by
  `GlyphRun::font`.
- `FontKey(pub u64)` — stable semantic identity that changes when the font
  bytes or collection face changes; it is not pointer identity.
- `FontResource { key: FontKey, data: Arc<[u8]>, index: u32 }` — owned,
  shareable raw font bytes plus the face's collection index. No
  Parley/Swash/Vello/Blob type appears; a backend converts these bytes to its
  own font handle.
- `GlyphPosition { id: u32, x: f32, y: f32 }` — one glyph; `id` is the
  **font-local** glyph index (not a Unicode code point), `x`/`y` the pen
  (baseline) position in the run's local space.
- `AffineTransform { xx, yx, xy, yy, dx, dy }` — renderer-neutral affine with
  `identity()`; `(x, y)` maps to `(xx*x + yx*y + dx, xy*x + yy*y + dy)`,
  matching kurbo's column-vector convention.
- `GlyphRun { font: FontId, font_size: f32, glyphs: Arc<[GlyphPosition]>,
  transform: AffineTransform, color: Color, hint: bool }`.
- `PaintScene.fonts: Vec<FontResource>` — the scene owns font bytes; backends
  cache backend-native conversions keyed by `(FontKey, index)`, never by the
  scene-local `FontId` alone.
- `PaintCommand::GlyphRun { run: GlyphRun }`.
- `PaintError` gains concrete, presentation- and Vello-free variants:
  `InvalidFont(FontId)`, `InvalidFontData { font, reason }`,
  `InvalidGlyphRun { font, reason }`, `UnsupportedGlyphRun`.

Every existing `PaintScene` constructor/tests and the legacy `raster.rs`
interpreter now include `fonts: vec![]`. The legacy `FrameBuffer` interpreter
explicitly **skips** `GlyphRun` with a comment documenting that the command is
*unsupported* there (no producer emits it and R4 is Vello-only), rather than
silently claiming parity. All R0 frame hashes are unchanged
(`raw_frame_fixtures_are_deterministic` still passes).

## Feature flag and command

`crates/youth-render-vello-cpu/Cargo.toml`:

```toml
[features]
glyph-run = ["vello_cpu/text", "dep:skrifa"]
```

- `glyph-run` is **not a default feature**; the default feature set is
  unchanged (AlphaMask-only).
- `vello_cpu = "=0.1.0"` stays exactly pinned, `default-features = false`,
  features `["std", "u8_pipeline"]`; `text` is enabled **only** when R4 tests
  or evaluation explicitly request `--features glyph-run`.
- `skrifa = "=0.44.0"` (the exact version glifo already uses) is added behind
  the feature purely to **validate font bytes before they reach Vello**, whose
  pinned text path panics on invalid data (`glifo` unwraps the skrifa parse
  and the `head` lookup).
- `youth-editor-engine` and `youth-text-render-cpu` are **dev-dependencies
  only** (for the comparison fixture); neither is a runtime dependency.

Run the R4 suite with:

```
cargo test -p youth-render-vello-cpu --features glyph-run
```

Run with the default feature set to prove nothing regressed without the
feature:

```
cargo test -p youth-render-vello-cpu
```

## Vello implementation

Behind `#[cfg(feature = "glyph-run")]`, `VelloCpuBackend`:

- Owns a font-conversion cache keyed by `(FontKey, collection index)`, converting a
  `FontResource`'s `Arc<[u8]>` into Vello `peniko::FontData`/`Blob` **once**
  per resource (`Blob::new(Arc::new(data.clone()))` — a refcount bump, never
  a byte copy; the caller-owned `PaintScene` remains the source of font
  ownership). Reuses the conversion across frames at a given size and clears
  it alongside `Resources` whenever the render context is recreated on a
  physical-size change.
- Validates before drawing: font id present, font bytes non-empty and
  parseable at the requested collection index with a usable `head` table
  (skrifa), glyph ids within the font's `maxp` glyph count, glyph positions
  finite, font size finite and positive — all as concrete `PaintError`
  variants, never panics and never silent skips.
- Renders `GlyphRun` via the real `RenderContext::glyph_run(&mut resources,
  &font)` API: `GlyphRunBuilder::font_size`, `hint`, the run's `AffineTransform`
  mapped onto kurbo's coefficient layout (`[xx, xy, yx, yy, dx, dy]`; the
  f32-to-f64 widening preserves every coefficient), `set_paint` color, and
  `fill_glyphs`. The scene transform is reset after each run so command
  ordering/state isolation is preserved
  (covered by a test: a `FillRect` after two transformed runs lands exactly).
- With the feature **disabled**, `GlyphRun` returns
  `PaintError::UnsupportedGlyphRun` (tested in the default feature set) —
  never a panic, never silently rendering nothing.
- `Resources`/`RenderContext` coupling and the existing timed rendering API
  (`render_into_timed`) are unchanged.

## Comparison fixture and metrics

`glyph_run_tests::glyph_run_is_structurally_comparable_to_swash_alphamask`
builds one host-repeatable fixture from a real `ParleyEditorEngine` presentation
(`"Hi"`, 16.0 px), then renders the exact same glyph positions through

- (a) the existing Swash `GlyphRasterizer` → `GlyphMask`/`AlphaMask` producer,
  and
- (b) the Vello `GlyphRun` path,

both through `VelloCpuBackend` into a white-cleared 128x64 premultiplied RGBA8
target, and compares bounded-region metrics. Measured on this host
(macOS, arm64, debug build):

| metric | value |
| --- | --- |
| swash painted pixels | 87 |
| vello painted pixels | 98 |
| swash non-zero bbox | (1, 0)–(14, 11) |
| vello non-zero bbox | (1, 0)–(14, 11) |
| painted-pixel overlap | 87/98 (0.89) |
| max channel delta (union) | 114 (antialiased edges) |
| mean channel delta (union) | 25.7 |
| max alpha delta | 0 (both composite opaque text) |

On this host, both paths bound **exactly the same 14×12 pixel region** and agree on 89% of
painted pixels; the per-pixel deltas are concentrated at antialiased edges
(independent rasterizers, independent hinting). This is a **structural
comparability claim, not pixel parity**: no byte-exact hashes are pinned for
antialiased text, and no claim is made that either path is "better".

## Known caveats

- **Hinting.** Vello's glifo path applies vertical-only hinting and only when
  the run transform is a uniform scale with no skew/rotation; Swash uses its
  own (FreeType-style) hinting. Edge geometry can differ by pixels at small
  sizes.
- **Emoji / color / bitmap / fallback fonts.** `GlyphRun` carries one font
  resource per run; there is no Unicode fallback chain, and color/bitmap
  glyph support is whatever the pinned glifo path does for a single face.
  Parley's fallback decisions are baked into glyph ids at presentation time
  and must be preserved by the producer.
- **Low DPI / scale.** The R4 fixture renders at 1×; hinting and AA behavior
  at fractional/retina scales is not measured.
- **Selection / IME.** `GlyphRun` paints text only. Selection/cursor geometry
  still ships as `AlphaMask`/rects; IME candidate placement is untouched.
- **Atlas cache experimental.** Vello's glyph-atlas cache
  (`GlyphRunBuilder::atlas_cache`) is explicitly experimental upstream and is
  **not** enabled; glyphs are rasterized directly.
- **Font data validation depth.** Validation is as deep as the pinned Vello
  path requires to avoid panics (parse + head + glyph-count bounds) and is
  keyed to the pinned `=0.1.0` behavior; a Vello upgrade may change what needs
  validating.

## What is measured vs not measured

**Measured:** the data-model/resource-ownership tests in `youth-paint`; the
Vello backend's validation, transform/state-restoration, bundled-font
rendering, repeated-render resource-reuse, and cache-recreation tests; the
single deterministic Swash-vs-Vello comparison fixture metrics above; and
regression runs of the existing R0/R1/R3 suites (no-feature backend tests,
youth-desktop tests including the R0 frame-hash pins, softbuffer bridge,
youth-text-render-cpu, youth-editor-engine).

**Not measured:** no end-to-end window presentation of `GlyphRun` (no producer
emits it), no performance/throughput numbers for either text path, no
hardware/retina/high-DPI text certification, no emoji/CJK/fallback coverage,
no selection/IME interaction, and no R5 default-flip decision. Those remain
future-gate work.

## Explicit adoption status

- `GlyphRun` is **not emitted by youth-desktop** in this gate; no producer in
  the workspace creates one.
- The native default renderer (`RenderBackend::VelloCpu`) and the explicit
  legacy oracle (`YOUTH_RENDER_BACKEND=legacy`) both remain **AlphaMask-only**;
  neither uses `GlyphRun`.
- The `glyph-run` feature must be explicitly requested; the default build is
  byte-for-byte the previous behavior.
- R5 has adopted the renderer default; GlyphRun remains unelected and is not
  part of the renderer adoption decision.
