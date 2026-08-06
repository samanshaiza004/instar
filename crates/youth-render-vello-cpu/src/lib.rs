//! Vello CPU paint backend for Youth's desktop presentation.
//!
//! Implements [`youth_paint::PaintBackend`] on top of `vello_cpu`, turning
//! a [`youth_paint::PaintScene`] (built by `youth-desktop::raster`) into
//! premultiplied RGBA8 pixels in a caller-owned, reusable
//! [`youth_paint::RenderTarget`]. This crate is the only place in the Youth
//! workspace that depends on Vello, and its public API exposes no Vello
//! types: callers only ever see [`VelloCpuBackend`], `youth_paint` types,
//! and `youth_paint::PaintError`.
//!
//! # Imaging model
//!
//! `Color` input is straight (non-premultiplied) sRGB; the target output is
//! premultiplied RGBA8. Every command composites via standard source-over.
//! `Clear` replaces the *current clip region* (the whole scene when no clip
//! is active), including mid-stream clears. Alpha masks are bridged into
//! tinted, premultiplied pixmaps and drawn as Vello image paints, so a
//! mask's coverage modulates the color's alpha before source-over.

#![forbid(unsafe_code)]

#[cfg(feature = "glyph-run")]
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use vello_cpu::color::{AlphaColor, PremulRgba8, Srgb};
use vello_cpu::kurbo::{Affine, Rect as KurboRect, RoundedRect, RoundedRectRadii, Shape};
use vello_cpu::peniko::{Extend, ImageBrush, ImageQuality, ImageSampler};
use vello_cpu::{ImageSource, Pixmap, PixmapMut, RenderContext, Resources};
#[cfg(feature = "glyph-run")]
use youth_paint::{AffineTransform, FontId, FontKey, FontResource, GlyphPosition, GlyphRun};
use youth_paint::{
    AlphaMask, Color, MaskId, PaintBackend, PaintCommand, PaintError, PaintScene, PhysicalSize,
    Point, Rect, RenderTarget,
};

#[cfg(feature = "glyph-run")]
use skrifa::FontRef;
#[cfg(feature = "glyph-run")]
use skrifa::raw::TableProvider;
#[cfg(feature = "glyph-run")]
use vello_cpu::Glyph;
#[cfg(feature = "glyph-run")]
use vello_cpu::peniko::{Blob, FontData};

// vello_cpu 0.1.0 stores the viewport in u16s and uses 128-pixel depth
// buckets. Keeping dimensions below the next bucket boundary avoids the
// upstream crate's u16 multiplication overflow near u16::MAX. Revisit this
// guard when the pinned Vello version changes.
const MAX_SAFE_DIMENSION: u32 = 65_280;

/// Microsecond timings for one [`VelloCpuBackend::render_into_timed`] call,
/// split so a presenter can attribute backend preparation (recreating the
/// render context, its coupled [`Resources`], and resizing the render
/// target) separately from actual scene recording and rasterization.
///
/// Backend-neutral: no Vello types appear here, so `youth-desktop` can
/// consume this without importing anything from `vello_cpu`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VelloRenderTimings {
    /// Time spent preparing and -- when the physical size changed --
    /// recreating the render context, its coupled [`Resources`], and
    /// resizing the caller's [`RenderTarget`] in place. Zero when the context
    /// already exists at `size` and the target needs no resize.
    pub backend_resize_us: u64,
    /// Time spent starting a fresh frame, recording the scene's commands,
    /// and rasterizing the result into the [`RenderTarget`].
    pub render_us: u64,
}

/// A [`PaintBackend`] that rasterizes scenes with Vello's CPU renderer.
///
/// Owns a Vello [`RenderContext`] and its coupled [`Resources`], plus the
/// scratch buffers the alpha-mask bridging needs. The lifecycle is explicit
/// and single-owner: the context is *recreated* (never `reset_and_resize`d)
/// alongside a fresh `Resources` whenever the physical render size changes
/// (Vello CPU's context size is wired into far more than the scene extent,
/// so a fresh context at the new size is the safe reading of the renderer's
/// contract), and the caller's [`RenderTarget`] is resized in place at the
/// same time. [`VelloCpuBackend::prepare`] is that single, testable seam:
/// both [`render_into`](PaintBackend::render_into) and
/// [`VelloCpuBackend::render_into_timed`] route their validation and
/// preparation through it.
#[derive(Debug)]
pub struct VelloCpuBackend {
    /// The active render context. `None` only before the first render;
    /// recreated when the physical size changes.
    context: Option<RenderContext>,
    /// Vello resources coupled to `context`: the image registry that
    /// alpha-mask pixmaps are registered with, plus Vello's internal
    /// caches. Recreated alongside the context.
    resources: Resources,
    /// The size `context` was created at; `(0, 0)` before the first render.
    size: PhysicalSize,
    /// Pixmaps registered with `resources` during the current render call;
    /// returned to `pool` once the call finishes.
    registered: Vec<Arc<Pixmap>>,
    /// Alpha-mask pixmaps from previous calls, reused as buffers so a
    /// steady-state frame allocates no new mask pixmaps (only grows when a
    /// larger mask than seen before arrives).
    pool: Vec<Arc<Pixmap>>,
    /// Backend-owned conversion of the scene's [`FontResource`]s into Vello
    /// font handles, keyed by stable semantic `(FontKey, collection index)`.
    /// Converted once per resource and reused across frames; cleared whenever
    /// the render context and its coupled [`Resources`] are recreated.
    #[cfg(feature = "glyph-run")]
    fonts: HashMap<(FontKey, u32), FontData>,
    /// Glyph count reported by each cached font's `maxp` table (when present),
    /// used to bound run glyph ids without re-parsing the font per run.
    #[cfg(feature = "glyph-run")]
    glyph_counts: HashMap<(FontKey, u32), Option<u32>>,
}

impl Default for VelloCpuBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl VelloCpuBackend {
    /// Creates an empty backend; the render context is created lazily on
    /// the first [`PaintBackend::render_into`] call.
    #[must_use]
    pub fn new() -> Self {
        Self {
            context: None,
            resources: Resources::new(),
            size: PhysicalSize {
                width: 0,
                height: 0,
            },
            registered: Vec::new(),
            pool: Vec::new(),
            #[cfg(feature = "glyph-run")]
            fonts: HashMap::new(),
            #[cfg(feature = "glyph-run")]
            glyph_counts: HashMap::new(),
        }
    }

    /// Pulls a pixmap from the reuse pool that has room for `area` pixels,
    /// resized to `width` x `height`; allocates a fresh one if no pooled
    /// buffer is big enough. `Pixmap::resize` keeps the existing
    /// allocation whenever the capacity already fits, so only new,
    /// larger-than-before masks allocate.
    fn take_pixmap(&mut self, width: u16, height: u16, area: usize) -> Arc<Pixmap> {
        if let Some(index) = self
            .pool
            .iter()
            .position(|candidate| candidate.capacity() >= area)
        {
            let mut pixmap = self.pool.swap_remove(index);
            if let Some(unique) = Arc::get_mut(&mut pixmap) {
                unique.resize(width, height);
                return pixmap;
            }
            // An aliased pixmap should never sit in the pool; drop it and
            // fall through to a fresh allocation.
        }
        Arc::new(Pixmap::new(width, height))
    }

    fn draw_command(
        &mut self,
        command: &PaintCommand,
        scene: &PaintScene,
    ) -> Result<(), PaintError> {
        match command {
            PaintCommand::Clear { color } => {
                // Replace the full current clip region: vello clips this
                // fill to whatever clip stack is active, which is exactly
                // the "current clip" semantics the scene contract requires.
                let (width, height) = (f64::from(self.size.width), f64::from(self.size.height));
                let context = self.context_mut()?;
                context.set_paint(alpha_color(*color));
                context.fill_rect(&KurboRect::new(0.0, 0.0, width, height));
            }
            PaintCommand::FillRect { rect, color } => {
                let context = self.context_mut()?;
                context.set_paint(alpha_color(*color));
                context.fill_rect(&kurbo_rect(*rect));
            }
            PaintCommand::StrokeRect { rect, width, color } => {
                // Only a 1-physical-pixel stroke is in the contract; render
                // it exactly like the legacy interpreter does -- four 1px
                // edge fills.
                if *width != 1.0 {
                    return Err(PaintError::UnsupportedStrokeWidth(*width));
                }
                if rect.width == 0 || rect.height == 0 {
                    return Ok(());
                }
                let context = self.context_mut()?;
                context.set_paint(alpha_color(*color));
                context.fill_rect(&kurbo_rect(Rect { height: 1, ..*rect }));
                context.fill_rect(&kurbo_rect(Rect {
                    y: rect.y + rect.height as i32 - 1,
                    height: 1,
                    ..*rect
                }));
                context.fill_rect(&kurbo_rect(Rect { width: 1, ..*rect }));
                context.fill_rect(&kurbo_rect(Rect {
                    x: rect.x + rect.width as i32 - 1,
                    width: 1,
                    ..*rect
                }));
            }
            PaintCommand::FillRoundedRect { rect, radii, color } => {
                let context = self.context_mut()?;
                context.set_paint(alpha_color(*color));
                let rounded = RoundedRect::from_rect(
                    kurbo_rect(*rect),
                    RoundedRectRadii::new(
                        f64::from(radii.top_left),
                        f64::from(radii.top_right),
                        f64::from(radii.bottom_right),
                        f64::from(radii.bottom_left),
                    ),
                );
                context.fill_path(&rounded.to_path(0.1));
            }
            PaintCommand::AlphaMask {
                mask,
                origin,
                color,
            } => {
                let mask_id = *mask;
                let alpha_mask = scene
                    .masks
                    .get(mask_id.0 as usize)
                    .ok_or(PaintError::InvalidMask(mask_id))?;
                self.draw_alpha_mask(mask_id, alpha_mask, *origin, *color)?;
            }
            PaintCommand::GlyphRun { run } => {
                #[cfg(feature = "glyph-run")]
                {
                    self.draw_glyph_run(run, scene)?;
                }
                #[cfg(not(feature = "glyph-run"))]
                {
                    let _ = run;
                    return Err(PaintError::UnsupportedGlyphRun);
                }
            }
            PaintCommand::PushClip { rect } => {
                let context = self.context_mut()?;
                context.push_clip_path(&kurbo_rect(*rect).to_path(0.1));
            }
            PaintCommand::PopClip => {
                let context = self.context_mut()?;
                context.pop_clip_path();
            }
        }
        Ok(())
    }

    /// Bridges one rasterized alpha mask into a premultiplied pixmap
    /// tinted by `color` (coverage modulating the color's alpha) and draws
    /// it as a Vello image paint at `origin`, so it composites with
    /// source-over and is clipped by the scene bounds and the active clip
    /// stack.
    fn draw_alpha_mask(
        &mut self,
        mask_id: MaskId,
        mask: &AlphaMask,
        origin: Point,
        color: Color,
    ) -> Result<(), PaintError> {
        let width = u16::try_from(mask.width).map_err(|_| PaintError::InvalidMaskData {
            mask: mask_id,
            reason: "mask width exceeds the renderer's limit",
        })?;
        let height = u16::try_from(mask.height).map_err(|_| PaintError::InvalidMaskData {
            mask: mask_id,
            reason: "mask height exceeds the renderer's limit",
        })?;
        let area = usize::from(width) * usize::from(height);
        if mask.alpha.len() != area {
            return Err(PaintError::InvalidMaskData {
                mask: mask_id,
                reason: "alpha buffer length does not match mask dimensions",
            });
        }

        let mut pixmap = self.take_pixmap(width, height, area);
        let mut may_have_transparency = false;
        {
            let pixels = Arc::get_mut(&mut pixmap)
                .ok_or(PaintError::BackendFailure(
                    "alpha-mask pixmap is unexpectedly shared",
                ))?
                .data_mut();
            for (pixel, &coverage) in pixels.iter_mut().zip(mask.alpha.iter()) {
                *pixel = premul_rgba8(color, coverage);
                may_have_transparency |= coverage != 255 || color.a != 255;
            }
        }
        if let Some(unique) = Arc::get_mut(&mut pixmap) {
            unique.set_may_have_transparency(may_have_transparency);
        }

        let image_id = self.resources.register_image(pixmap.clone());
        self.registered.push(pixmap);

        let brush = ImageBrush {
            image: ImageSource::opaque_id_with_transparency_hint(image_id, may_have_transparency),
            sampler: ImageSampler {
                x_extend: Extend::Pad,
                y_extend: Extend::Pad,
                // Integer-only translations (the only kind this backend
                // produces) are auto-lowered to nearest-neighbor by Vello;
                // asking for Low up front avoids the bilinear path entirely.
                quality: ImageQuality::Low,
                alpha: 1.0,
            },
        };
        let context = self.context_mut()?;
        context.set_paint(brush);
        // The image samples in its own [0, width) x [0, height) space; the
        // paint transform maps local image coordinates onto the scene, so
        // the fill geometry must be the *destination* rect at `origin` in
        // scene space (filling the local rect instead would misplace the
        // image and bleed a padded edge column/row at nonzero origins).
        context.set_paint_transform(Affine::translate((
            f64::from(origin.x),
            f64::from(origin.y),
        )));
        context.fill_rect(&KurboRect::new(
            f64::from(origin.x),
            f64::from(origin.y),
            f64::from(origin.x) + f64::from(mask.width),
            f64::from(origin.y) + f64::from(mask.height),
        ));
        context.reset_paint_transform();
        Ok(())
    }

    /// Rasterizes a [`GlyphRun`] directly with Vello's text pipeline, filling
    /// each glyph outline with the run's color.
    ///
    /// This is the R4 evaluation path, compiled only with the `glyph-run`
    /// feature. Font bytes are validated (parsed via skrifa) before they are
    /// handed to Vello, whose pinned 0.1.0 text path panics on invalid data
    /// rather than returning an error; the converted handle is cached per
    /// [`FontId`] and reused across frames.
    #[cfg(feature = "glyph-run")]
    fn draw_glyph_run(&mut self, run: &GlyphRun, scene: &PaintScene) -> Result<(), PaintError> {
        let font_id = run.font;
        let (font_data, glyph_count) = self.font_data(font_id, scene)?;
        if !run.font_size.is_finite() || run.font_size <= 0.0 {
            return Err(PaintError::InvalidGlyphRun {
                font: font_id,
                reason: "font size must be finite and positive",
            });
        }
        for glyph in &*run.glyphs {
            if !glyph.x.is_finite() || !glyph.y.is_finite() {
                return Err(PaintError::InvalidGlyphRun {
                    font: font_id,
                    reason: "glyph positions must be finite",
                });
            }
            // glifo silently skips glyph ids outside the font, so a producer
            // that mixed ids from a different face would misrender silently;
            // bound them to the face's glyph count when one is known.
            if glyph_count.is_some_and(|count| glyph.id >= count) {
                return Err(PaintError::InvalidGlyphRun {
                    font: font_id,
                    reason: "glyph id is outside the font's glyph count",
                });
            }
        }

        // Draw with disjoint field borrows: `glyph_run` takes the context
        // and `Resources` as separate mutable parameters, and the scene
        // transform must be applied before the builder snapshots it.
        let Self {
            context, resources, ..
        } = self;
        let context = context
            .as_mut()
            .ok_or(PaintError::BackendFailure("render context unavailable"))?;
        // Map the run's baseline positions into scene space exactly like the
        // rest of the command walker does (scene transform is identity at
        // command boundaries), then restore it afterwards so the next
        // command's state is unaffected.
        let saved_state = context.save_current_state();
        context.set_transform(affine_transform(run.transform));
        context.set_paint(alpha_color(run.color));
        context
            .glyph_run(resources, &font_data)
            .font_size(run.font_size)
            .hint(run.hint)
            .fill_glyphs(run.glyphs.iter().map(positioned_glyph));
        context.restore_state(saved_state);
        Ok(())
    }

    /// Validates the scene's [`FontResource`] for `font_id` and returns a
    /// cached Vello [`FontData`] handle, converting it the first time, plus
    /// the font's glyph count (when its `maxp` table is present) for glyph-id
    /// bounding.
    ///
    /// Validation is as deep as the pinned Vello text path requires: skrifa
    /// must be able to parse the bytes at the requested collection index and
    /// report a usable `head` table (Vello's glifo layer unwraps both, so
    /// invalid data would otherwise panic).
    #[cfg(feature = "glyph-run")]
    fn font_data(
        &mut self,
        font_id: FontId,
        scene: &PaintScene,
    ) -> Result<(FontData, Option<u32>), PaintError> {
        let resource = scene
            .fonts
            .get(font_id.0 as usize)
            .ok_or(PaintError::InvalidFont(font_id))?;
        let cache_key = (resource.key, resource.index);
        if let Some(font) = self.fonts.get(&cache_key) {
            return Ok((
                font.clone(),
                self.glyph_counts.get(&cache_key).copied().flatten(),
            ));
        }
        let glyph_count = validate_font_resource(font_id, resource)?;
        // The bytes stay where the scene owns them: `Arc<Arc<[u8]>>` points
        // at the same `Arc<[u8]>` the FontResource holds (Arc's own
        // `AsRef<T> for Arc<T>` makes the inner Arc the trait object), so
        // conversion is a refcount bump, never a copy of the font data.
        let font = FontData::new(Blob::new(Arc::new(resource.data.clone())), resource.index);
        self.fonts.insert(cache_key, font.clone());
        self.glyph_counts.insert(cache_key, glyph_count);
        Ok((font, glyph_count))
    }

    fn context_mut(&mut self) -> Result<&mut RenderContext, PaintError> {
        self.context
            .as_mut()
            .ok_or(PaintError::BackendFailure("render context unavailable"))
    }

    /// Validates `size` against the scene contract and the renderer's u16
    /// limit, recreates the render context and its coupled [`Resources`]
    /// whenever the physical size changed, and resizes the caller's
    /// [`RenderTarget`] in place when it does not already match.
    ///
    /// This is the backend's single lifecycle seam: every render call routes
    /// validation and preparation through it, so the "fresh context +
    /// fresh `Resources` + in-place target resize on size change" rule is
    /// explicit, tested, and shared between the plain
    /// [`render_into`](PaintBackend::render_into) path and the timed
    /// [`render_into_timed`](VelloCpuBackend::render_into_timed) path.
    /// Returns the microseconds this preparation took; no per-frame
    /// allocation happens beyond the recreation itself.
    fn prepare(
        &mut self,
        size: PhysicalSize,
        width: u16,
        height: u16,
        target: &mut RenderTarget,
    ) -> Result<u64, PaintError> {
        let started = Instant::now();
        if self.size != size {
            // Recreation, not `reset_and_resize`: the context's size is
            // baked into its dispatcher, tile and strip allocations, so
            // resizing in place would leave stale internals. The coupled
            // Resources (image registry, glyph atlas, caches) are recreated
            // alongside it, and the backend's font-handle cache is cleared
            // with them so nothing from the old context survives.
            self.context = Some(RenderContext::new(width, height));
            self.resources = Resources::new();
            self.registered.clear();
            #[cfg(feature = "glyph-run")]
            self.fonts.clear();
            #[cfg(feature = "glyph-run")]
            self.glyph_counts.clear();
            self.size = size;
        }
        if target.size() != size {
            target.resize(size)?;
        }
        Ok(started.elapsed().as_micros() as u64)
    }

    fn checked_dimensions(size: PhysicalSize) -> Result<(u16, u16), PaintError> {
        if size.width > MAX_SAFE_DIMENSION || size.height > MAX_SAFE_DIMENSION {
            return Err(PaintError::SizeExceedsBackendLimit(size));
        }
        let width =
            u16::try_from(size.width).map_err(|_| PaintError::SizeExceedsBackendLimit(size))?;
        let height =
            u16::try_from(size.height).map_err(|_| PaintError::SizeExceedsBackendLimit(size))?;
        Ok((width, height))
    }

    /// Renders `scene` into `target`, reporting how long backend
    /// preparation took separately from scene recording and rasterization.
    ///
    /// Behavior is identical to [`render_into`](PaintBackend::render_into):
    /// this is the single implementation, and the trait method delegates to
    /// it so the two paths can never diverge. The timings are measured with
    /// `Instant::now()` around the exact operations the plain path performs
    /// -- no extra allocation per frame, no extra passes.
    ///
    /// Returns [`PaintError::SizeExceedsBackendLimit`] for a size past the
    /// u16 renderer limit (before any narrowing cast), and the same errors
    /// as [`render_into`](PaintBackend::render_into) for scene-contract
    /// violations.
    pub fn render_into_timed(
        &mut self,
        size: PhysicalSize,
        scene: &PaintScene,
        target: &mut RenderTarget,
    ) -> Result<VelloRenderTimings, PaintError> {
        // Scene construction bugs must be reported, not silently rendered:
        // an imbalanced clip stack would pop a clip the backend never
        // pushed, and a scene size mismatch would rasterize commands built
        // for a different viewport.
        scene.validate().map_err(PaintError::ClipImbalance)?;
        if scene.size != size {
            return Err(PaintError::SizeMismatch {
                scene: scene.size,
                requested: size,
            });
        }
        // Vello CPU's context and pixmaps are u16-sized, and 0.1.0 also
        // requires a tile-safe upper bound. Reject anything larger rather
        // than truncating or reaching the upstream tile-rounding panic.
        let (width, height) = Self::checked_dimensions(size)?;

        let backend_resize_us = self.prepare(size, width, height, target)?;

        // Start a fresh frame on the (possibly just-recreated) context, then
        // record the scene's commands and rasterize them into the target.
        let render_started = Instant::now();
        if let Some(context) = self.context.as_mut() {
            context.reset();
        }

        for command in &scene.commands {
            self.draw_command(command, scene)?;
        }

        if let Some(context) = self.context.as_mut() {
            context.flush();
            let Some(target_view) = PixmapMut::new(width, height, target.pixels_mut()) else {
                return Err(PaintError::BackendFailure(
                    "render target buffer length does not match its size",
                ));
            };
            context.render(target_view, &mut self.resources);
        }
        let render_us = render_started.elapsed().as_micros() as u64;

        // Drop the image registry's references and hand every registered
        // mask pixmap back to the reuse pool for the next frame.
        self.resources.clear_images();
        self.pool.append(&mut self.registered);
        Ok(VelloRenderTimings {
            backend_resize_us,
            render_us,
        })
    }
}

impl PaintBackend for VelloCpuBackend {
    fn render_into(
        &mut self,
        size: PhysicalSize,
        scene: &PaintScene,
        target: &mut RenderTarget,
    ) -> Result<(), PaintError> {
        // The timed path is the single implementation; the trait method
        // delegates so plain rendering and timed rendering can never diverge.
        self.render_into_timed(size, scene, target).map(|_| ())
    }
}

/// Converts a straight sRGB [`Color`] into Vello's paint color type.
fn alpha_color(color: Color) -> AlphaColor<Srgb> {
    AlphaColor::from_rgba8(color.r, color.g, color.b, color.a)
}

/// Converts a scene [`Rect`] into a kurbo rect in the same coordinate
/// space (y-down physical pixels). Widths/heights are u32, so the far edge
/// is computed in f64 to avoid i32 overflow for very large rects.
fn kurbo_rect(rect: Rect) -> KurboRect {
    KurboRect::new(
        f64::from(rect.x),
        f64::from(rect.y),
        f64::from(rect.x) + f64::from(rect.width),
        f64::from(rect.y) + f64::from(rect.height),
    )
}

/// Maps a scene [`AffineTransform`] onto kurbo's coefficient layout. kurbo's
/// augmented matrix is `| a c e | / | b d f |` (a column-vector convention),
/// so the six f32 coefficients `{ xx, yx, xy, yy, dx, dy }` that satisfy
/// `x' = xx*x + yx*y + dx`, `y' = xy*x + yy*y + dy` become
/// `[xx, xy, yx, yy, dx, dy]`; widening the coefficients from f32 to f64
/// preserves every input value.
#[cfg(feature = "glyph-run")]
fn affine_transform(transform: AffineTransform) -> Affine {
    Affine::new([
        f64::from(transform.xx),
        f64::from(transform.xy),
        f64::from(transform.yx),
        f64::from(transform.yy),
        f64::from(transform.dx),
        f64::from(transform.dy),
    ])
}

/// Borrows a scene [`GlyphPosition`] as the Vello `Glyph` the text pipeline
/// consumes; the fields line up 1:1 (font-local id plus run-space x/y).
#[cfg(feature = "glyph-run")]
fn positioned_glyph(position: &GlyphPosition) -> Glyph {
    Glyph {
        id: position.id,
        x: position.x,
        y: position.y,
    }
}

/// Validates a scene [`FontResource`] as deeply as the pinned Vello text
/// path requires before the bytes reach Vello's glifo layer, which unwraps
/// both the skrifa parse and the `head` lookup and would otherwise panic on
/// invalid data. Returns the concrete [`PaintError::InvalidFontData`] error
/// instead of panicking, plus the font's glyph count when a `maxp` table is
/// present (used to bound run glyph ids).
#[cfg(feature = "glyph-run")]
fn validate_font_resource(
    font_id: FontId,
    resource: &FontResource,
) -> Result<Option<u32>, PaintError> {
    if resource.data.is_empty() {
        return Err(PaintError::InvalidFontData {
            font: font_id,
            reason: "font data is empty",
        });
    }
    let font_ref = FontRef::from_index(&resource.data, resource.index).map_err(|_| {
        PaintError::InvalidFontData {
            font: font_id,
            reason: "font data does not parse as a font at the requested index",
        }
    })?;
    let upem = font_ref
        .head()
        .map(|head| head.units_per_em())
        .map_err(|_| PaintError::InvalidFontData {
            font: font_id,
            reason: "font has no usable head table",
        })?;
    if upem == 0 {
        return Err(PaintError::InvalidFontData {
            font: font_id,
            reason: "font head table reports zero units per em",
        });
    }
    Ok(font_ref
        .maxp()
        .ok()
        .map(|maxp| u32::from(maxp.num_glyphs())))
}

/// Premultiplies a straight sRGB [`Color`] by an 8-bit `coverage` value:
/// `alpha = color.a * coverage / 255`, each channel scaled by that alpha.
/// A fully opaque color at full coverage is exactly the opaque RGB value,
/// and the result is ready for source-over compositing.
fn premul_rgba8(color: Color, coverage: u8) -> PremulRgba8 {
    let alpha = (u16::from(color.a) * u16::from(coverage)) / 255;
    PremulRgba8 {
        r: ((u16::from(color.r) * alpha) / 255) as u8,
        g: ((u16::from(color.g) * alpha) / 255) as u8,
        b: ((u16::from(color.b) * alpha) / 255) as u8,
        a: alpha as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use youth_paint::{CornerRadii, PhysicalSize};

    const SIZE: PhysicalSize = PhysicalSize {
        width: 16,
        height: 12,
    };

    /// Sample the premultiplied RGBA8 pixel at (x, y).
    fn sample(target: &RenderTarget, x: u32, y: u32) -> [u8; 4] {
        let index = (y * target.width() + x) as usize * 4;
        target.pixels()[index..index + 4].try_into().unwrap()
    }

    fn fill_rect(rect: Rect, color: Color) -> PaintCommand {
        PaintCommand::FillRect { rect, color }
    }

    fn clear(color: Color) -> PaintCommand {
        PaintCommand::Clear { color }
    }

    fn scene(commands: Vec<PaintCommand>, masks: Vec<AlphaMask>) -> PaintScene {
        PaintScene {
            size: SIZE,
            commands,
            masks,
            fonts: vec![],
            images: vec![],
        }
    }

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// Renders `commands`/`masks` at `SIZE` into a fresh target.
    fn render(
        commands: Vec<PaintCommand>,
        masks: Vec<AlphaMask>,
    ) -> Result<RenderTarget, PaintError> {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();
        backend.render_into(SIZE, &scene(commands, masks), &mut target)?;
        Ok(target)
    }

    #[test]
    fn opaque_fill_covers_exactly_its_rect() {
        let target = render(
            vec![
                clear(Color::opaque(255, 255, 255)),
                fill_rect(rect(2, 3, 4, 2), Color::opaque(255, 0, 0)),
            ],
            vec![],
        )
        .unwrap();
        for y in 0..SIZE.height {
            for x in 0..SIZE.width {
                let expected = if (2..6).contains(&x) && (3..5).contains(&y) {
                    [255, 0, 0, 255]
                } else {
                    [255, 255, 255, 255]
                };
                assert_eq!(sample(&target, x, y), expected, "pixel at ({x}, {y})");
            }
        }
    }

    #[test]
    fn translucent_fill_source_overs_over_the_backdrop() {
        // Opaque white backdrop, then 50%-alpha red over it: premultiplied
        // source (128, 0, 0, 128) source-overs onto (255, 255, 255, 255) to
        // give r = 128 + 255*(127/255) = 255 and g/b = 127. The u8 pipeline
        // rounds each channel to a byte, so assert with a one-channel
        // tolerance.
        let target = render(
            vec![
                clear(Color::opaque(255, 255, 255)),
                fill_rect(
                    rect(0, 0, 4, 4),
                    Color::with_alpha(Color::opaque(255, 0, 0), 128),
                ),
            ],
            vec![],
        )
        .unwrap();
        let [r, g, b, a] = sample(&target, 1, 1);
        assert_eq!(a, 255);
        assert!((i32::from(r) - 255).abs() <= 1, "r = {r}");
        assert!((i32::from(g) - 127).abs() <= 1, "g = {g}");
        assert!((i32::from(b) - 127).abs() <= 1, "b = {b}");
    }

    #[test]
    fn nested_clips_intersect_and_pop_restores() {
        // Clear white, clip [2,8)x[2,8), clip [5,11)x[5,11), fill the whole
        // scene red, pop, fill [0,4)x[0,4) blue. The red may only reach the
        // intersection [5,8)x[5,8); the blue only [0,4)x[0,4) once the
        // clips are gone.
        let target = render(
            vec![
                clear(Color::opaque(255, 255, 255)),
                PaintCommand::PushClip {
                    rect: rect(2, 2, 6, 6),
                },
                PaintCommand::PushClip {
                    rect: rect(5, 5, 6, 6),
                },
                fill_rect(rect(0, 0, 16, 12), Color::opaque(255, 0, 0)),
                PaintCommand::PopClip,
                PaintCommand::PopClip,
                fill_rect(rect(0, 0, 4, 4), Color::opaque(0, 0, 255)),
            ],
            vec![],
        )
        .unwrap();
        for y in 0..SIZE.height {
            for x in 0..SIZE.width {
                let expected = if (5..8).contains(&x) && (5..8).contains(&y) {
                    [255, 0, 0, 255]
                } else if x < 4 && y < 4 {
                    [0, 0, 255, 255]
                } else {
                    [255, 255, 255, 255]
                };
                assert_eq!(sample(&target, x, y), expected, "pixel at ({x}, {y})");
            }
        }
    }

    #[test]
    fn mid_stream_clear_respects_the_current_clip() {
        let target = render(
            vec![
                clear(Color::opaque(255, 255, 255)),
                PaintCommand::PushClip {
                    rect: rect(1, 1, 4, 4),
                },
                clear(Color::opaque(0, 255, 0)),
                PaintCommand::PopClip,
            ],
            vec![],
        )
        .unwrap();
        for y in 0..SIZE.height {
            for x in 0..SIZE.width {
                let expected = if (1..5).contains(&x) && (1..5).contains(&y) {
                    [0, 255, 0, 255]
                } else {
                    [255, 255, 255, 255]
                };
                assert_eq!(sample(&target, x, y), expected, "pixel at ({x}, {y})");
            }
        }
    }

    #[test]
    fn clip_imbalance_is_rejected_before_any_rendering() {
        let unbalanced = scene(
            vec![PaintCommand::PushClip {
                rect: rect(0, 0, 4, 4),
            }],
            vec![],
        );
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();
        assert_eq!(
            backend.render_into(SIZE, &unbalanced, &mut target),
            Err(PaintError::ClipImbalance(
                youth_paint::PaintSceneError::ClipImbalance(1)
            ))
        );

        let underflow = scene(vec![PaintCommand::PopClip], vec![]);
        assert_eq!(
            backend.render_into(SIZE, &underflow, &mut target),
            Err(PaintError::ClipImbalance(
                youth_paint::PaintSceneError::ClipUnderflow(0)
            ))
        );
    }

    #[test]
    fn unknown_mask_id_and_bad_alpha_buffer_are_rejected() {
        let unknown = scene(
            vec![PaintCommand::AlphaMask {
                mask: MaskId(7),
                origin: Point { x: 0, y: 0 },
                color: Color::opaque(255, 0, 0),
            }],
            vec![],
        );
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();
        assert_eq!(
            backend.render_into(SIZE, &unknown, &mut target),
            Err(PaintError::InvalidMask(MaskId(7)))
        );

        // A registered mask whose alpha buffer length does not match its
        // declared dimensions is a scene-construction bug, not a render bug.
        let malformed = scene(
            vec![PaintCommand::AlphaMask {
                mask: MaskId(0),
                origin: Point { x: 0, y: 0 },
                color: Color::opaque(255, 0, 0),
            }],
            vec![AlphaMask {
                left: 0,
                top: 0,
                width: 3,
                height: 3,
                alpha: Arc::from(vec![0u8; 8].as_slice()),
            }],
        );
        assert!(matches!(
            backend.render_into(SIZE, &malformed, &mut target),
            Err(PaintError::InvalidMaskData { .. })
        ));
    }

    #[test]
    fn stroke_rect_is_four_one_pixel_edge_fills() {
        let target = render(
            vec![
                clear(Color::opaque(255, 255, 255)),
                PaintCommand::StrokeRect {
                    rect: rect(3, 2, 6, 5),
                    width: 1.0,
                    color: Color::opaque(0, 0, 0),
                },
            ],
            vec![],
        )
        .unwrap();
        for y in 0..SIZE.height {
            for x in 0..SIZE.width {
                let on_edge = ((3..9).contains(&x) && (y == 2 || y == 6))
                    || ((2..7).contains(&y) && (x == 3 || x == 8));
                let expected = if on_edge {
                    [0, 0, 0, 255]
                } else {
                    [255, 255, 255, 255]
                };
                assert_eq!(sample(&target, x, y), expected, "pixel at ({x}, {y})");
            }
        }
    }

    #[test]
    fn non_one_pixel_stroke_width_is_rejected() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();
        let stroke = scene(
            vec![PaintCommand::StrokeRect {
                rect: rect(0, 0, 4, 4),
                width: 2.0,
                color: Color::opaque(0, 0, 0),
            }],
            vec![],
        );
        assert_eq!(
            backend.render_into(SIZE, &stroke, &mut target),
            Err(PaintError::UnsupportedStrokeWidth(2.0))
        );
    }

    #[test]
    fn rounded_rect_is_a_capability_beyond_the_legacy_interpreter() {
        // Rounded rects leave the corners transparent while filling the
        // interior -- something the legacy FrameBuffer interpreter (which
        // has no rounded capability) could never produce.
        let target = render(
            vec![
                clear(Color::opaque(255, 255, 255)),
                PaintCommand::FillRoundedRect {
                    rect: rect(2, 2, 8, 8),
                    radii: CornerRadii {
                        top_left: 3.0,
                        top_right: 3.0,
                        bottom_right: 3.0,
                        bottom_left: 3.0,
                    },
                    color: Color::opaque(0, 0, 255),
                },
            ],
            vec![],
        )
        .unwrap();
        // The exact corner pixel (2, 2) of an 8x8 rounded rect with radius 3
        // must be far from the blue fill (only a hair of anti-aliased
        // coverage, which shows up in the red channel: the blue fill would
        // leave it at 0), where a plain FillRect would put full blue -- the
        // non-parity capability this backend has and the legacy interpreter
        // does not.
        let [corner_r, _, _, _] = sample(&target, 2, 2);
        assert!(corner_r > 200, "corner stays white-ish (r = {corner_r})");
        assert_eq!(
            sample(&target, 5, 5),
            [0, 0, 255, 255],
            "the interior is fully filled"
        );
    }

    #[test]
    fn alpha_mask_tints_with_partial_alpha_and_source_overs() {
        // A 2x2 mask with coverage {255, 128, 0, 255} at origin (2, 2),
        // tinted 50%-alpha blue over an opaque red backdrop: coverage 255
        // halves the color alpha, coverage 128 quarters it, coverage 0
        // leaves the backdrop untouched.
        let target = render(
            vec![
                clear(Color::opaque(255, 0, 0)),
                PaintCommand::AlphaMask {
                    mask: MaskId(0),
                    origin: Point { x: 2, y: 2 },
                    color: Color::with_alpha(Color::opaque(0, 0, 255), 128),
                },
            ],
            vec![AlphaMask {
                left: 0,
                top: 0,
                width: 2,
                height: 2,
                alpha: Arc::from(vec![255, 128, 0, 255].as_slice()),
            }],
        )
        .unwrap();
        let [r, _, b, a] = sample(&target, 2, 2);
        assert_eq!(a, 255);
        assert!((i32::from(b) - 128).abs() <= 1, "b = {b}");
        assert!((i32::from(r) - 128).abs() <= 1, "r = {r}");

        let [r, _, b, a] = sample(&target, 3, 2);
        assert_eq!(a, 255);
        assert!((i32::from(b) - 64).abs() <= 1, "b = {b}");
        assert!((i32::from(r) - 191).abs() <= 1, "r = {r}");

        // Coverage 0 leaves the backdrop untouched...
        assert_eq!(sample(&target, 2, 3), [255, 0, 0, 255]);
        // ...while full coverage at (3, 3) tints blue like (2, 2) did.
        let [r, _, b, _] = sample(&target, 3, 3);
        assert!((i32::from(b) - 128).abs() <= 1, "b = {b}");
        assert!((i32::from(r) - 128).abs() <= 1, "r = {r}");
    }

    #[test]
    fn alpha_mask_with_negative_origin_is_clipped_to_the_scene() {
        // Mask 3x3 at origin (-1, -1): only the bottom-right 2x2 of the
        // mask lands inside the scene.
        let target = render(
            vec![
                clear(Color::opaque(255, 255, 255)),
                PaintCommand::AlphaMask {
                    mask: MaskId(0),
                    origin: Point { x: -1, y: -1 },
                    color: Color::opaque(0, 0, 0),
                },
            ],
            vec![AlphaMask {
                left: 0,
                top: 0,
                width: 3,
                height: 3,
                alpha: Arc::from(vec![255; 9].as_slice()),
            }],
        )
        .unwrap();
        assert_eq!(sample(&target, 0, 0), [0, 0, 0, 255]);
        assert_eq!(sample(&target, 1, 0), [0, 0, 0, 255]);
        assert_eq!(sample(&target, 0, 1), [0, 0, 0, 255]);
        assert_eq!(sample(&target, 1, 1), [0, 0, 0, 255]);
        assert_eq!(
            sample(&target, 2, 0),
            [255, 255, 255, 255],
            "the third mask column is fully off-scene"
        );
    }

    #[test]
    fn caller_owned_target_is_reused_and_recreated_on_resize() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();

        let first = scene(
            vec![
                clear(Color::opaque(255, 255, 255)),
                fill_rect(rect(0, 0, 2, 2), Color::opaque(255, 0, 0)),
            ],
            vec![],
        );
        backend.render_into(SIZE, &first, &mut target).unwrap();
        assert_eq!(sample(&target, 0, 0), [255, 0, 0, 255]);

        // A second frame at the same size reuses the backend context and
        // the caller's target; the buffer is fully replaced (Replace
        // composite mode clears everything not repainted).
        let second = scene(vec![clear(Color::opaque(0, 255, 0))], vec![]);
        backend.render_into(SIZE, &second, &mut target).unwrap();
        assert_eq!(sample(&target, 15, 11), [0, 255, 0, 255]);

        // A new physical size recreates the backend's context and resizes
        // the caller's target in place.
        let big = PhysicalSize {
            width: 20,
            height: 14,
        };
        let big_scene = PaintScene {
            size: big,
            commands: vec![clear(Color::opaque(0, 0, 255))],
            masks: vec![],
            fonts: vec![],
            images: vec![],
        };
        backend.render_into(big, &big_scene, &mut target).unwrap();
        assert_eq!(target.size(), big);
        assert_eq!(target.pixels().len(), 20 * 14 * 4);
        assert_eq!(sample(&target, 19, 13), [0, 0, 255, 255]);

        // And back down again.
        backend.render_into(SIZE, &second, &mut target).unwrap();
        assert_eq!(target.size(), SIZE);
        assert_eq!(sample(&target, 0, 0), [0, 255, 0, 255]);
    }

    #[test]
    fn context_recreates_across_scale_derived_physical_sizes() {
        // The desktop producer passes the window's physical inner size to
        // the backend -- the backend never receives a scale factor. This
        // test derives those physical sizes from a fixed logical fixture
        // with deterministic rounding (the same `logical * scale` shape a
        // producer would use), then drives the backend through every scale
        // in both directions so context recreation is exercised growing and
        // shrinking.
        const LOGICAL_WIDTH: u32 = 640;
        const LOGICAL_HEIGHT: u32 = 360;
        let physical = |scale: f64| PhysicalSize {
            width: (f64::from(LOGICAL_WIDTH) * scale).round() as u32,
            height: (f64::from(LOGICAL_HEIGHT) * scale).round() as u32,
        };
        // 640x360 logical is exact at every one of the required scales.
        assert_eq!(
            physical(1.0),
            PhysicalSize {
                width: 640,
                height: 360
            }
        );
        assert_eq!(
            physical(1.25),
            PhysicalSize {
                width: 800,
                height: 450
            }
        );
        assert_eq!(
            physical(1.5),
            PhysicalSize {
                width: 960,
                height: 540
            }
        );
        assert_eq!(
            physical(2.0),
            PhysicalSize {
                width: 1280,
                height: 720
            }
        );

        let scales = [1.0, 1.25, 1.5, 2.0];
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(PhysicalSize {
            width: 1,
            height: 1,
        })
        .unwrap();

        // Grow 1.0 -> 2.0, then come back down 2.0 -> 1.0: every
        // transition recreates the backend's context and resizes the
        // caller's target in place.
        for scale_index in [0, 1, 2, 3, 2, 1, 0] {
            let scale = scales[scale_index];
            let size = physical(scale);
            let scene = PaintScene {
                size,
                commands: vec![
                    clear(Color::opaque(255, 255, 255)),
                    fill_rect(rect(0, 0, 4, 4), Color::opaque(255, 0, 0)),
                ],
                masks: vec![],
                fonts: vec![],
                images: vec![],
            };
            backend.render_into(size, &scene, &mut target).unwrap();

            // The target follows the new physical size exactly...
            assert_eq!(target.size(), size, "target size at scale {scale}");
            assert_eq!(
                target.pixels().len(),
                size.width as usize * size.height as usize * 4,
                "target buffer length at scale {scale}"
            );

            // ...and the opaque scene is intact: the 4x4 red square at the
            // origin, a fully cleared white far corner, and one mid-edge
            // sample between them.
            assert_eq!(
                sample(&target, 0, 0),
                [255, 0, 0, 255],
                "pixel (0, 0) at scale {scale}"
            );
            assert_eq!(
                sample(&target, 3, 3),
                [255, 0, 0, 255],
                "pixel (3, 3) at scale {scale}"
            );
            assert_eq!(
                sample(&target, size.width - 1, size.height - 1),
                [255, 255, 255, 255],
                "far corner at scale {scale}"
            );
        }
    }

    #[test]
    fn scene_size_mismatch_and_oversized_scenes_are_rejected() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();

        let mismatch = scene(vec![clear(Color::opaque(0, 0, 0))], vec![]);
        let other_size = PhysicalSize {
            width: 8,
            height: 8,
        };
        assert_eq!(
            backend.render_into(other_size, &mismatch, &mut target),
            Err(PaintError::SizeMismatch {
                scene: SIZE,
                requested: other_size
            })
        );

        let oversized = PhysicalSize {
            width: u32::from(u16::MAX) + 1,
            height: 8,
        };
        let big_scene = PaintScene {
            size: oversized,
            commands: vec![],
            masks: vec![],
            fonts: vec![],
            images: vec![],
        };
        let mut big_target = RenderTarget::new(oversized).unwrap();
        assert_eq!(
            backend.render_into(oversized, &big_scene, &mut big_target),
            Err(PaintError::SizeExceedsBackendLimit(oversized))
        );
    }

    #[test]
    fn mask_dimensions_beyond_u16_are_rejected() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();
        let huge_mask = scene(
            vec![PaintCommand::AlphaMask {
                mask: MaskId(0),
                origin: Point { x: 0, y: 0 },
                color: Color::opaque(255, 0, 0),
            }],
            vec![AlphaMask {
                left: 0,
                top: 0,
                width: u32::from(u16::MAX) + 1,
                height: 1,
                alpha: Arc::from(Vec::new().as_slice()),
            }],
        );
        assert!(matches!(
            backend.render_into(SIZE, &huge_mask, &mut target),
            Err(PaintError::InvalidMaskData { .. })
        ));
    }

    #[test]
    fn u16_boundary_validation_accepts_tile_safe_max_and_rejects_larger_sizes() {
        // The pinned Vello release cannot safely round a u16::MAX viewport up
        // to its internal tile/depth boundaries. The largest safe dimension
        // is the depth-bucket-aligned 65_280; it renders without a panic. Both the exact
        // u16::MAX boundary and one past it are rejected before narrowing,
        // in both dimensions, with no giant allocation.
        for (width, height) in [(MAX_SAFE_DIMENSION, 1u32), (1u32, MAX_SAFE_DIMENSION)] {
            let size = PhysicalSize { width, height };
            let mut backend = VelloCpuBackend::new();
            let mut target = RenderTarget::new(size).unwrap();
            let scene = PaintScene {
                size,
                commands: vec![clear(Color::opaque(0, 0, 0))],
                masks: vec![],
                fonts: vec![],
                images: vec![],
            };
            backend
                .render_into(size, &scene, &mut target)
                .expect("tile-safe maximum dimension must render");
            assert_eq!(target.size(), size);
        }

        for (width, height) in [
            (u32::from(u16::MAX), 1u32),
            (1u32, u32::from(u16::MAX)),
            (u32::from(u16::MAX) + 1, 1u32),
            (1u32, u32::from(u16::MAX) + 1),
        ] {
            let size = PhysicalSize { width, height };
            let mut backend = VelloCpuBackend::new();
            let mut target = RenderTarget::new(PhysicalSize {
                width: 1,
                height: 1,
            })
            .unwrap();
            let scene = PaintScene {
                size,
                commands: vec![],
                masks: vec![],
                fonts: vec![],
                images: vec![],
            };
            assert_eq!(
                backend.render_into(size, &scene, &mut target),
                Err(PaintError::SizeExceedsBackendLimit(size))
            );
        }
    }

    #[test]
    fn render_into_timed_matches_plain_rendering_and_reports_both_stages() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();

        let first = scene(
            vec![
                clear(Color::opaque(255, 255, 255)),
                fill_rect(rect(0, 0, 2, 2), Color::opaque(255, 0, 0)),
            ],
            vec![],
        );
        // Both stage timings are returned as plain u64 microsecond counts;
        // their exact magnitudes are environment-dependent, so the test
        // asserts the pixels (the shared behavioral contract with the plain
        // trait path) rather than any timing value.
        let _first_timings = backend
            .render_into_timed(SIZE, &first, &mut target)
            .unwrap();
        assert_eq!(sample(&target, 0, 0), [255, 0, 0, 255]);
        assert_eq!(sample(&target, 15, 11), [255, 255, 255, 255]);

        // A steady-state frame at the same size: no context recreation, and
        // the target is not resized, so prepare is still reported as its own
        // field and the pixels are fully replaced.
        let second = scene(vec![clear(Color::opaque(0, 255, 0))], vec![]);
        let _second_timings = backend
            .render_into_timed(SIZE, &second, &mut target)
            .unwrap();
        assert_eq!(sample(&target, 0, 0), [0, 255, 0, 255]);
        assert_eq!(sample(&target, 15, 11), [0, 255, 0, 255]);

        // A size change is still handled through the timed path, including
        // an in-place target resize.
        let big = PhysicalSize {
            width: 20,
            height: 14,
        };
        let big_scene = PaintScene {
            size: big,
            commands: vec![clear(Color::opaque(0, 0, 255))],
            masks: vec![],
            fonts: vec![],
            images: vec![],
        };
        backend
            .render_into_timed(big, &big_scene, &mut target)
            .unwrap();
        assert_eq!(target.size(), big);
        assert_eq!(sample(&target, 19, 13), [0, 0, 255, 255]);

        // Validation failures surface through the timed path exactly as
        // they do through the trait method.
        let oversized = PhysicalSize {
            width: u32::from(u16::MAX) + 1,
            height: 1,
        };
        let oversized_scene = PaintScene {
            size: oversized,
            commands: vec![],
            masks: vec![],
            fonts: vec![],
            images: vec![],
        };
        assert_eq!(
            backend.render_into_timed(oversized, &oversized_scene, &mut target),
            Err(PaintError::SizeExceedsBackendLimit(oversized))
        );
    }

    #[test]
    #[cfg(not(feature = "glyph-run"))]
    fn glyph_run_without_the_feature_is_a_concrete_unsupported_error() {
        // The default feature set (no `glyph-run`) must reject GlyphRun with
        // the typed unsupported error rather than panic or silently render
        // nothing. Compiled only in builds without the feature; with it,
        // glyph_run_tests exercises the real path.
        let run = youth_paint::GlyphRun {
            font: youth_paint::FontId(0),
            font_size: 16.0,
            glyphs: std::sync::Arc::from(
                vec![youth_paint::GlyphPosition {
                    id: 0,
                    x: 0.0,
                    y: 0.0,
                }]
                .as_slice(),
            ),
            transform: youth_paint::AffineTransform::identity(),
            color: Color::opaque(0, 0, 0),
            hint: false,
        };
        let scene = PaintScene {
            size: SIZE,
            commands: vec![PaintCommand::GlyphRun { run }],
            masks: vec![],
            fonts: vec![],
            images: vec![],
        };
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(SIZE).unwrap();
        assert_eq!(
            backend.render_into(SIZE, &scene, &mut target),
            Err(PaintError::UnsupportedGlyphRun)
        );
    }
}

/// Gate R4 evaluation tests for the opt-in `glyph-run` feature: the Vello
/// GlyphRun path, its validation/state-isolation contract, and the
/// comparison against the existing Swash-to-AlphaMask producer.
#[cfg(all(test, feature = "glyph-run"))]
mod glyph_run_tests {
    use super::*;
    use skrifa::MetadataProvider;
    use youth_editor_engine::{EditorLayout, ParleyEditorEngine};
    use youth_text_render_cpu::GlyphRasterizer;

    /// Bundled Roboto Mono (OFL-1.1, see `assets/OFL.txt`), for deterministic
    /// tests that must not depend on which fonts the host has installed.
    const BUNDLED_TEST_FONT: &[u8] = include_bytes!("../assets/RobotoMono.ttf");

    const GLYPH_SIZE: PhysicalSize = PhysicalSize {
        width: 96,
        height: 48,
    };

    fn clear(color: Color) -> PaintCommand {
        PaintCommand::Clear { color }
    }

    /// Sample the premultiplied RGBA8 pixel at (x, y).
    fn sample(target: &RenderTarget, x: u32, y: u32) -> [u8; 4] {
        let index = (y * target.width() + x) as usize * 4;
        target.pixels()[index..index + 4].try_into().unwrap()
    }

    fn font_resource() -> FontResource {
        FontResource {
            key: FontKey(1),
            data: Arc::from(BUNDLED_TEST_FONT),
            index: 0,
        }
    }

    /// The bundled font's glyph id for `ch` (font-local, via its cmap).
    fn glyph_id_for(ch: char) -> u32 {
        let font = FontRef::from_index(BUNDLED_TEST_FONT, 0).expect("bundled font parses");
        font.charmap()
            .map(ch)
            .expect("bundled font maps the character")
            .to_u32()
    }

    fn run(font: FontId, font_size: f32, glyphs: &[GlyphPosition]) -> GlyphRun {
        GlyphRun {
            font,
            font_size,
            glyphs: Arc::from(glyphs),
            transform: AffineTransform::identity(),
            color: Color::opaque(0, 0, 0),
            hint: false,
        }
    }

    fn glyph_scene(run: GlyphRun, fonts: Vec<FontResource>) -> PaintScene {
        PaintScene {
            size: GLYPH_SIZE,
            commands: vec![
                clear(Color::opaque(255, 255, 255)),
                PaintCommand::GlyphRun { run },
            ],
            masks: vec![],
            fonts,
            images: vec![],
        }
    }

    /// Bounds and count of the pixels that differ from `backdrop`.
    fn painted_region(target: &RenderTarget, backdrop: [u8; 4]) -> (usize, i32, i32, i32, i32) {
        let mut count = 0usize;
        let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
        let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
        for y in 0..target.height() {
            for x in 0..target.width() {
                if sample(target, x, y) != backdrop {
                    count += 1;
                    min_x = min_x.min(x as i32);
                    min_y = min_y.min(y as i32);
                    max_x = max_x.max(x as i32);
                    max_y = max_y.max(y as i32);
                }
            }
        }
        (count, min_x, min_y, max_x, max_y)
    }

    #[test]
    fn glyph_run_renders_ink_on_the_bundled_font() {
        let scene = glyph_scene(
            run(
                FontId(0),
                24.0,
                &[GlyphPosition {
                    id: glyph_id_for('A'),
                    x: 12.0,
                    y: 30.0,
                }],
            ),
            vec![font_resource()],
        );
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(GLYPH_SIZE).unwrap();
        backend
            .render_into(GLYPH_SIZE, &scene, &mut target)
            .unwrap();

        // Bounded-region assertions only: 'A' at 24px on the bundled font
        // must paint ink near its baseline pen, well inside the scene. No
        // byte-exact hash -- the exact rasterization is antialiased and
        // renderer-specific.
        let (count, min_x, min_y, max_x, max_y) = painted_region(&target, [255, 255, 255, 255]);
        assert!(count > 0, "the glyph paints ink");
        assert!(
            (4..24).contains(&min_x),
            "ink starts near the pen (min_x {min_x})"
        );
        assert!(
            (0..24).contains(&min_y),
            "ink reaches up from the baseline (min_y {min_y})"
        );
        assert!(max_x < 40, "ink stays bounded right (max_x {max_x})");
        assert!(
            max_y <= 31,
            "ink stays at or above the baseline (max_y {max_y})"
        );
        assert_eq!(
            sample(&target, 2, 2),
            [255, 255, 255, 255],
            "far corner stays white"
        );
    }

    #[test]
    fn missing_font_id_is_rejected() {
        let scene = glyph_scene(
            run(
                FontId(7),
                16.0,
                &[GlyphPosition {
                    id: glyph_id_for('A'),
                    x: 0.0,
                    y: 0.0,
                }],
            ),
            vec![font_resource()],
        );
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(GLYPH_SIZE).unwrap();
        assert_eq!(
            backend.render_into(GLYPH_SIZE, &scene, &mut target),
            Err(PaintError::InvalidFont(FontId(7)))
        );
    }

    #[test]
    fn empty_and_unparseable_font_data_are_rejected() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(GLYPH_SIZE).unwrap();

        // Empty font data.
        let empty = glyph_scene(
            run(
                FontId(0),
                16.0,
                &[GlyphPosition {
                    id: glyph_id_for('A'),
                    x: 0.0,
                    y: 0.0,
                }],
            ),
            vec![FontResource {
                key: FontKey(2),
                data: Arc::from(Vec::<u8>::new().as_slice()),
                index: 0,
            }],
        );
        assert!(matches!(
            backend.render_into(GLYPH_SIZE, &empty, &mut target),
            Err(PaintError::InvalidFontData {
                font: FontId(0),
                ..
            })
        ));

        // Bytes that do not parse as a font.
        let garbage = glyph_scene(
            run(
                FontId(0),
                16.0,
                &[GlyphPosition {
                    id: 0,
                    x: 0.0,
                    y: 0.0,
                }],
            ),
            vec![FontResource {
                key: FontKey(3),
                data: Arc::from(vec![0xde, 0xad, 0xbe, 0xef].as_slice()),
                index: 0,
            }],
        );
        assert!(matches!(
            backend.render_into(GLYPH_SIZE, &garbage, &mut target),
            Err(PaintError::InvalidFontData {
                font: FontId(0),
                ..
            })
        ));

        // A valid single-face file but a collection index that does not exist.
        let bad_index = glyph_scene(
            run(
                FontId(0),
                16.0,
                &[GlyphPosition {
                    id: 0,
                    x: 0.0,
                    y: 0.0,
                }],
            ),
            vec![FontResource {
                key: FontKey(4),
                data: Arc::from(BUNDLED_TEST_FONT),
                index: 9,
            }],
        );
        assert!(matches!(
            backend.render_into(GLYPH_SIZE, &bad_index, &mut target),
            Err(PaintError::InvalidFontData {
                font: FontId(0),
                ..
            })
        ));
    }

    #[test]
    fn reusing_a_font_id_with_a_new_stable_key_uses_new_scene_bytes() {
        let valid = glyph_scene(
            run(
                FontId(0),
                16.0,
                &[GlyphPosition {
                    id: glyph_id_for('A'),
                    x: 0.0,
                    y: 0.0,
                }],
            ),
            vec![font_resource()],
        );
        let invalid = glyph_scene(
            run(
                FontId(0),
                16.0,
                &[GlyphPosition {
                    id: 0,
                    x: 0.0,
                    y: 0.0,
                }],
            ),
            vec![FontResource {
                key: FontKey(2),
                data: Arc::from(vec![0xde, 0xad, 0xbe, 0xef].as_slice()),
                index: 0,
            }],
        );
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(GLYPH_SIZE).unwrap();
        backend
            .render_into(GLYPH_SIZE, &valid, &mut target)
            .unwrap();
        assert!(matches!(
            backend.render_into(GLYPH_SIZE, &invalid, &mut target),
            Err(PaintError::InvalidFontData {
                font: FontId(0),
                ..
            })
        ));
    }

    #[test]
    fn non_positive_and_non_finite_font_sizes_are_rejected() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(GLYPH_SIZE).unwrap();
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let scene = glyph_scene(
                run(
                    FontId(0),
                    size,
                    &[GlyphPosition {
                        id: glyph_id_for('A'),
                        x: 0.0,
                        y: 0.0,
                    }],
                ),
                vec![font_resource()],
            );
            assert!(
                matches!(
                    backend.render_into(GLYPH_SIZE, &scene, &mut target),
                    Err(PaintError::InvalidGlyphRun {
                        font: FontId(0),
                        ..
                    })
                ),
                "font size {size} must be rejected"
            );
        }
    }

    #[test]
    fn out_of_range_glyph_ids_and_non_finite_positions_are_rejected() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(GLYPH_SIZE).unwrap();

        // Glyph id beyond the font's maxp glyph count (glifo would silently
        // skip it; the backend must reject it instead).
        let num_glyphs = FontRef::from_index(BUNDLED_TEST_FONT, 0)
            .unwrap()
            .maxp()
            .unwrap()
            .num_glyphs();
        let bad_id = glyph_scene(
            run(
                FontId(0),
                16.0,
                &[GlyphPosition {
                    id: u32::from(num_glyphs) + 5,
                    x: 0.0,
                    y: 0.0,
                }],
            ),
            vec![font_resource()],
        );
        assert!(matches!(
            backend.render_into(GLYPH_SIZE, &bad_id, &mut target),
            Err(PaintError::InvalidGlyphRun {
                font: FontId(0),
                ..
            })
        ));

        // A non-finite position would poison the transform math downstream.
        for (x, y) in [(f32::NAN, 0.0), (0.0, f32::INFINITY)] {
            let scene = glyph_scene(
                run(
                    FontId(0),
                    16.0,
                    &[GlyphPosition {
                        id: glyph_id_for('A'),
                        x,
                        y,
                    }],
                ),
                vec![font_resource()],
            );
            assert!(
                matches!(
                    backend.render_into(GLYPH_SIZE, &scene, &mut target),
                    Err(PaintError::InvalidGlyphRun {
                        font: FontId(0),
                        ..
                    })
                ),
                "position ({x}, {y}) must be rejected"
            );
        }
    }

    #[test]
    fn run_transform_places_glyphs_and_render_state_is_restored() {
        // Two runs of the same glyph translated to different pen positions,
        // followed by a FillRect that must land exactly (state isolation:
        // neither run's transform may leak into the next command).
        let glyphs = vec![GlyphPosition {
            id: glyph_id_for('A'),
            x: 0.0,
            y: 30.0,
        }];
        let left = GlyphRun {
            transform: AffineTransform {
                xx: 1.0,
                yx: 0.0,
                xy: 0.0,
                yy: 1.0,
                dx: 8.0,
                dy: 0.0,
            },
            ..run(FontId(0), 24.0, &glyphs)
        };
        let right = GlyphRun {
            transform: AffineTransform {
                xx: 1.0,
                yx: 0.0,
                xy: 0.0,
                yy: 1.0,
                dx: 40.0,
                dy: 0.0,
            },
            ..run(FontId(0), 24.0, &glyphs)
        };
        let scene = PaintScene {
            size: GLYPH_SIZE,
            commands: vec![
                clear(Color::opaque(255, 255, 255)),
                PaintCommand::GlyphRun { run: left },
                PaintCommand::GlyphRun { run: right },
                PaintCommand::FillRect {
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 4,
                        height: 4,
                    },
                    color: Color::opaque(255, 0, 0),
                },
            ],
            masks: vec![],
            fonts: vec![font_resource()],
            images: vec![],
        };
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(GLYPH_SIZE).unwrap();
        backend
            .render_into(GLYPH_SIZE, &scene, &mut target)
            .unwrap();

        // The trailing rect is exactly at its integer destination: the
        // glyph-run scene transform was reset after each run.
        assert_eq!(sample(&target, 1, 1), [255, 0, 0, 255]);
        assert_eq!(sample(&target, 3, 3), [255, 0, 0, 255]);

        // Both translated 'A's painted ink: the left one near x=8, the right
        // one near x=40, with a white gap between them (proving the
        // translation really moved the glyphs).
        let (count, min_x, min_y, max_x, _max_y) = painted_region(&target, [255, 255, 255, 255]);
        assert!(count > 0, "both glyphs painted ink");
        assert_eq!(
            min_x, 0,
            "leftmost ink is the trailing red rect's left edge (min_x {min_x})"
        );
        assert!(
            min_y <= 14,
            "glyph ink reaches up from its baseline (min_y {min_y})"
        );
        // The right run starts its ink somewhere after the left run's ink ends.
        assert!(
            max_x >= 40 && max_x < GLYPH_SIZE.width as i32,
            "right-translated glyph painted near x=40 (max_x {max_x})"
        );
        // A white gap between the two glyphs.
        assert_eq!(
            sample(&target, 30, 20),
            [255, 255, 255, 255],
            "the gap between the translated runs stays white"
        );
    }

    #[test]
    fn repeated_rendering_reuses_the_converted_font_and_recreation_clears_it() {
        let mut backend = VelloCpuBackend::new();
        let mut target = RenderTarget::new(GLYPH_SIZE).unwrap();

        let scene = glyph_scene(
            run(
                FontId(0),
                24.0,
                &[GlyphPosition {
                    id: glyph_id_for('A'),
                    x: 12.0,
                    y: 30.0,
                }],
            ),
            vec![font_resource()],
        );

        // First render converts the scene's font bytes once; the Vello Blob
        // retains a second reference to the scene-owned Arc<[u8]>.
        backend
            .render_into(GLYPH_SIZE, &scene, &mut target)
            .unwrap();
        assert_eq!(
            Arc::strong_count(&scene.fonts[0].data),
            2,
            "the backend cache holds the converted font"
        );

        // A second frame at the same size reuses the cached conversion: no
        // new reference is taken, so the count is unchanged.
        backend
            .render_into(GLYPH_SIZE, &scene, &mut target)
            .unwrap();
        assert_eq!(
            Arc::strong_count(&scene.fonts[0].data),
            2,
            "the converted font is reused, not re-converted"
        );

        // A physical-size change recreates the context and its coupled
        // Resources, which clears the font cache with them.
        let big = PhysicalSize {
            width: 128,
            height: 64,
        };
        let big_scene = PaintScene {
            size: big,
            commands: vec![clear(Color::opaque(255, 255, 255))],
            masks: vec![],
            fonts: vec![],
            images: vec![],
        };
        backend.render_into(big, &big_scene, &mut target).unwrap();
        assert_eq!(
            Arc::strong_count(&scene.fonts[0].data),
            1,
            "the font cache is cleared when the context is recreated"
        );
    }

    /// Rasterizes `presentation`'s first run through Swash into
    /// [`AlphaMask`] commands (the existing producer path) at the run's own
    /// absolute baseline positions.
    fn swash_scene(
        presentation: &youth_editor_engine::TextPresentation,
        size: PhysicalSize,
    ) -> PaintScene {
        let run = presentation.runs.first().expect("at least one run");
        let mut rasterizer = GlyphRasterizer::new();
        let color = Color::opaque(0, 0, 0);
        let mut masks = Vec::new();
        let mut commands = vec![PaintCommand::Clear {
            color: Color::opaque(255, 255, 255),
        }];
        for glyph in &run.glyphs {
            let mask = rasterizer
                .rasterize(&run.font, glyph.id, run.font_size)
                .expect("the presentation's glyph rasterizes through Swash");
            if mask.is_empty() {
                continue;
            }
            let mask_id = MaskId(u32::try_from(masks.len()).expect("too many masks"));
            let pen_x = glyph.x.round() as i32;
            let pen_y = glyph.y.round() as i32;
            masks.push(AlphaMask {
                left: mask.left,
                top: mask.top,
                width: mask.width,
                height: mask.height,
                alpha: Arc::from(mask.alpha.as_slice()),
            });
            commands.push(PaintCommand::AlphaMask {
                mask: mask_id,
                // The mask's left/top are offsets from the pen; the AlphaMask
                // command composites at its origin, so bake them in.
                origin: Point {
                    x: pen_x + mask.left,
                    y: pen_y - mask.top,
                },
                color,
            });
        }
        PaintScene {
            size,
            commands,
            masks,
            fonts: vec![],
            images: vec![],
        }
    }

    #[test]
    fn glyph_run_is_structurally_comparable_to_swash_alphamask() {
        // One host-repeatable fixture from a real Parley presentation: render
        // the exact same glyph positions through (a) the existing
        // Swash-to-AlphaMask producer and (b) the Vello GlyphRun path, and
        // compare bounded-region metrics. No byte-exact hashes -- text
        // rasterization is antialiased and renderer-specific.
        let mut engine = ParleyEditorEngine::with_text("Hi");
        let presentation = engine.presentation();
        let run = presentation
            .runs
            .first()
            .expect("visible text produces a glyph run");
        assert!(!run.glyphs.is_empty(), "visible text produces glyphs");

        // A generous scene so the baseline-anchored ink fits either way.
        let size = PhysicalSize {
            width: 128,
            height: 64,
        };
        let color = Color::opaque(0, 0, 0);

        let swash_scene = swash_scene(&presentation, size);
        let glyphs: Vec<GlyphPosition> = run
            .glyphs
            .iter()
            .map(|g| GlyphPosition {
                id: g.id,
                x: g.x,
                y: g.y,
            })
            .collect();
        let vello_scene = PaintScene {
            size,
            commands: vec![
                PaintCommand::Clear {
                    color: Color::opaque(255, 255, 255),
                },
                PaintCommand::GlyphRun {
                    run: GlyphRun {
                        font: FontId(0),
                        font_size: run.font_size,
                        glyphs: Arc::from(glyphs.as_slice()),
                        transform: AffineTransform::identity(),
                        color,
                        hint: true,
                    },
                },
            ],
            masks: vec![],
            fonts: vec![FontResource {
                key: FontKey(1),
                data: Arc::from(run.font.data.data()),
                index: run.font.index,
            }],
            images: vec![],
        };

        let mut backend = VelloCpuBackend::new();
        let mut swash_target = RenderTarget::new(size).unwrap();
        backend
            .render_into(size, &swash_scene, &mut swash_target)
            .unwrap();
        let mut vello_target = RenderTarget::new(size).unwrap();
        backend
            .render_into(size, &vello_scene, &mut vello_target)
            .unwrap();

        let white = [255, 255, 255, 255];
        let (swash_count, swash_min_x, swash_min_y, swash_max_x, swash_max_y) =
            painted_region(&swash_target, white);
        let (vello_count, vello_min_x, vello_min_y, vello_max_x, vello_max_y) =
            painted_region(&vello_target, white);
        assert!(swash_count > 0 && vello_count > 0, "both paths paint ink");

        // Bounded-region comparability: both must bound roughly the same area
        // (a few pixels of hinting/AA slop either way), and the painted-pixel
        // counts must be within a generous factor.
        let bounds_ok = |a: i32, b: i32| (a - b).abs() <= 6;
        assert!(
            bounds_ok(swash_min_x, vello_min_x) && bounds_ok(swash_min_y, vello_min_y),
            "min bounds comparable: swash ({swash_min_x},{swash_min_y}) vello ({vello_min_x},{vello_min_y})"
        );
        assert!(
            bounds_ok(swash_max_x, vello_max_x) && bounds_ok(swash_max_y, vello_max_y),
            "max bounds comparable: swash ({swash_max_x},{swash_max_y}) vello ({vello_max_x},{vello_max_y})"
        );
        let (bigger, smaller) = (swash_count.max(vello_count), swash_count.min(vello_count));
        assert!(
            smaller > 0 && bigger <= smaller * 3,
            "painted-pixel counts within 3x: swash {swash_count}, vello {vello_count}"
        );

        // Overlap and per-pixel channel difference over the union of painted
        // pixels (a pixel is "painted" when it differs from the white
        // backdrop), reported as the comparison metrics. Both paths composite
        // opaque text, so alpha is 255 everywhere and the interesting delta
        // is in the RGB channels at antialiased edges.
        let (mut overlap, mut union, mut max_alpha_delta) = (0usize, 0usize, 0u8);
        let mut max_channel_delta = 0u8;
        let mut sum_channel_delta = 0u64;
        for y in 0..size.height {
            for x in 0..size.width {
                let sw = sample(&swash_target, x, y);
                let ve = sample(&vello_target, x, y);
                let s_painted = sw != white;
                let v_painted = ve != white;
                max_alpha_delta = max_alpha_delta.max(sw[3].abs_diff(ve[3]));
                if s_painted || v_painted {
                    union += 1;
                    if s_painted && v_painted {
                        overlap += 1;
                    }
                    let delta = (0..3).map(|ch| sw[ch].abs_diff(ve[ch])).max().unwrap();
                    max_channel_delta = max_channel_delta.max(delta);
                    sum_channel_delta += u64::from(delta);
                }
            }
        }
        let overlap_ratio = overlap as f64 / union as f64;
        let mean_channel_delta = sum_channel_delta as f64 / union as f64;

        println!(
            "R4 fixture \"Hi\" @ {:.1}px: swash {{painted={swash_count}, bbox=({swash_min_x},{swash_min_y})-({swash_max_x},{swash_max_y})}}, \
             vello {{painted={vello_count}, bbox=({vello_min_x},{vello_min_y})-({vello_max_x},{vello_max_y})}}, \
             overlap={overlap}/{union} ({overlap_ratio:.2}), max_channel_delta={max_channel_delta}, \
             mean_channel_delta={mean_channel_delta:.2}, max_alpha_delta={max_alpha_delta}",
            run.font_size,
        );

        // Structural claim, not pixel parity: most painted pixels coincide
        // even when edge AA/hinting differ. The per-pixel channel deltas are
        // reported, not asserted, because antialiased edges can legitimately
        // differ by a lot between two independent rasterizers.
        assert!(
            overlap_ratio >= 0.5,
            "structurally comparable: overlap ratio {overlap_ratio:.2}"
        );
    }
}
