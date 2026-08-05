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

use std::sync::Arc;

use vello_cpu::color::{AlphaColor, PremulRgba8, Srgb};
use vello_cpu::kurbo::{Affine, Rect as KurboRect, RoundedRect, RoundedRectRadii, Shape};
use vello_cpu::peniko::{Extend, ImageBrush, ImageQuality, ImageSampler};
use vello_cpu::{ImageSource, Pixmap, PixmapMut, RenderContext, Resources};
use youth_paint::{
    AlphaMask, Color, MaskId, PaintBackend, PaintCommand, PaintError, PaintScene, PhysicalSize,
    Point, Rect, RenderTarget,
};

/// A [`PaintBackend`] that rasterizes scenes with Vello's CPU renderer.
///
/// Owns a Vello [`RenderContext`] and its coupled [`Resources`], plus the
/// scratch buffers the alpha-mask bridging needs. The render context is
/// recreated (never `reset_and_resize`d) whenever the physical render size
/// changes, since Vello CPU's context size is wired into far more than the
/// scene extent; a fresh context at the new size is the safe reading of the
/// renderer's contract.
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

    fn context_mut(&mut self) -> Result<&mut RenderContext, PaintError> {
        self.context
            .as_mut()
            .ok_or(PaintError::BackendFailure("render context unavailable"))
    }
}

impl PaintBackend for VelloCpuBackend {
    fn render_into(
        &mut self,
        size: PhysicalSize,
        scene: &PaintScene,
        target: &mut RenderTarget,
    ) -> Result<(), PaintError> {
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
        // Vello CPU's context and pixmaps are u16-sized; reject anything
        // larger rather than panicking on a truncated cast.
        let width =
            u16::try_from(size.width).map_err(|_| PaintError::SizeExceedsBackendLimit(size))?;
        let height =
            u16::try_from(size.height).map_err(|_| PaintError::SizeExceedsBackendLimit(size))?;

        if self.size != size {
            // Recreation, not `reset_and_resize`: the context's size is
            // baked into its dispatcher, tile and strip allocations, so
            // resizing in place would leave stale internals. The coupled
            // Resources (image registry, caches) are recreated too.
            self.context = Some(RenderContext::new(width, height));
            self.resources = Resources::new();
            self.registered.clear();
            self.size = size;
        }
        if target.size() != size {
            target.resize(size)?;
        }

        // Start a fresh frame on the (possibly just-recreated) context.
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

        // Drop the image registry's references and hand every registered
        // mask pixmap back to the reuse pool for the next frame.
        self.resources.clear_images();
        self.pool.append(&mut self.registered);
        Ok(())
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
}
