//! Renderer-neutral paint intent for Youth's desktop presentation.
//!
//! `PaintScene`/`PaintCommand` are the seam between two concerns that used
//! to live in one function: deciding what a semantic tree, its layout, and
//! its live editor/focus/hover/fault state *mean* visually (scene
//! construction, owned by `youth-desktop::raster`) and turning that intent
//! into pixels (a paint backend -- today the existing hand-rolled
//! `FrameBuffer`, later a real rasterizer such as
//! `youth-render-vello-cpu`). This crate holds only the shared vocabulary
//! between those two; it has no rendering logic and no dependency on any
//! specific backend (Vello, Swash, or otherwise).
//!
//! A [`PaintScene`] is three collections: `commands` (the ordered paint
//! intent), `masks` (reusable alpha-coverage bitmaps referenced by
//! [`AlphaMask`] commands via [`MaskId`]), and `images` (reserved for a
//! future full-color image command; no command consumes it yet). Backends
//! consume the scene through [`PaintBackend::render_into`], which renders
//! premultiplied RGBA8 into a caller-owned [`RenderTarget`].
//!
//! Deliberately narrow for this increment: no paths, gradients, filters,
//! layers, or transforms beyond the rectangular clip stack and the
//! 1-physical-pixel stroke policy. Add those only when a real backend
//! spike or a real control needs them, not speculatively.

#![forbid(unsafe_code)]

use std::sync::Arc;

use thiserror::Error;

/// A point in physical pixel space. May be negative -- an alpha mask's
/// origin can fall outside the visible area (e.g. scrolled off the top of
/// its clip rect) before a backend clips it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// A non-negative extent in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// A rectangle in physical pixel space (origin may be negative for the
/// same reason as [`Point`]; `width`/`height` never are).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Corner radii for a rounded rectangle, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CornerRadii {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

/// The physical pixel dimensions a [`PaintScene`] was built for (and a
/// [`RenderTarget`] was allocated at).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

/// Straight (non-premultiplied) sRGB color with an 8-bit alpha channel.
/// `a: 255` is fully opaque; backends composite via standard source-over
/// blending regardless of `a`, so an opaque color and a "hard" fill are
/// the same command, not two different concepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    #[must_use]
    pub const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    #[must_use]
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }
}

/// A reusable, backend-neutral alpha-coverage bitmap: the same shape the
/// glyph rasterizer (`youth-text-render-cpu`) already produces, held in
/// the scene instead of inline in each command so backends can cache or
/// bridge it once. `left`/`top` are the mask's offset relative to its
/// origin of reference (a glyph pen for rasterized text); a backend only
/// needs `width`/`height`/`alpha` plus the command's absolute `origin`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlphaMask {
    /// Horizontal offset from the mask's origin of reference to its left
    /// edge.
    pub left: i32,
    /// Vertical offset from the mask's origin of reference to its top
    /// edge.
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// Row-major 8-bit coverage, exactly `width * height` bytes. 255 is
    /// fully opaque, 0 fully transparent.
    pub alpha: Arc<[u8]>,
}

/// Indexes a [`PaintScene`]'s `masks` collection from an
/// [`PaintCommand::AlphaMask`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MaskId(pub u32);

/// A full-color raster image, reserved for a future image-paint command.
/// No [`PaintCommand`] references it in this increment; the collection
/// exists so the scene contract does not need a breaking change when the
/// first image-consuming control arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// Row-major premultiplied RGBA8 pixels, exactly
    /// `width * height * 4` bytes.
    pub pixels: Arc<[u8]>,
}

/// One unit of paint intent. A [`PaintScene`]'s `commands` are consumed in
/// order -- ordering is part of the contract (later commands paint over
/// earlier ones; `PushClip`/`PopClip` bracket the commands they apply to).
#[derive(Clone, Debug, PartialEq)]
pub enum PaintCommand {
    /// Replaces every pixel in the current clip region (the whole scene if
    /// no clip is active) with `color`. Appearing more than once in a
    /// scene is legal and meaningful (the fault overlay does this: paint
    /// the normal scene, then `Clear` again before the fault text).
    Clear {
        color: Color,
    },
    FillRect {
        rect: Rect,
        color: Color,
    },
    /// A stroke of the rectangle's border. Only `width == 1.0` is
    /// supported in this increment: every backend implements it as four
    /// 1-physical-pixel edge fills, matching the existing hand-rolled
    /// border exactly. Any other width is rejected at render time
    /// ([`PaintError::UnsupportedStrokeWidth`]).
    StrokeRect {
        rect: Rect,
        width: f32,
        color: Color,
    },
    /// Fills `rect` with the supplied corner radii. Producer code in
    /// this increment never emits one (no control needs rounded geometry
    /// yet); it exists so a backend's rounded capability is part of the
    /// contract and testable before a producer depends on it.
    FillRoundedRect {
        rect: Rect,
        radii: CornerRadii,
        color: Color,
    },
    /// Composites the coverage bitmap at `scene.masks[mask]` at absolute
    /// physical `origin`, tinted by `color`. Coverage modulates the color's
    /// alpha before source-over compositing; `origin` may be negative
    /// (partially clipped glyphs) and the backend clips to the scene.
    AlphaMask {
        mask: MaskId,
        origin: Point,
        color: Color,
    },
    /// Restricts every subsequent command (until the matching `PopClip`)
    /// to `rect`, intersected with any already-active clip.
    PushClip {
        rect: Rect,
    },
    PopClip,
}

/// A complete, ordered description of one frame's paint intent.
#[derive(Clone, Debug, PartialEq)]
pub struct PaintScene {
    pub size: PhysicalSize,
    pub commands: Vec<PaintCommand>,
    pub masks: Vec<AlphaMask>,
    pub images: Vec<Image>,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum PaintSceneError {
    #[error("PopClip at command index {0} has no matching PushClip")]
    ClipUnderflow(usize),
    #[error("{0} PushClip command(s) left unclosed at the end of the scene")]
    ClipImbalance(usize),
}

impl PaintScene {
    /// Checks that `PushClip`/`PopClip` are balanced: no `PopClip` without
    /// an active `PushClip`, and no unclosed `PushClip` left at the end.
    /// A scene builder should call this before handing a scene to any
    /// backend -- an imbalanced clip stack is a scene-construction bug,
    /// not something a backend should have to detect or silently tolerate.
    /// Backends validate this again in [`PaintBackend::render_into`], so
    /// the check is usable both by producers and by backends.
    pub fn validate(&self) -> Result<(), PaintSceneError> {
        let mut depth: usize = 0;
        for (index, command) in self.commands.iter().enumerate() {
            match command {
                PaintCommand::PushClip { .. } => depth += 1,
                PaintCommand::PopClip => {
                    depth = depth
                        .checked_sub(1)
                        .ok_or(PaintSceneError::ClipUnderflow(index))?;
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err(PaintSceneError::ClipImbalance(depth));
        }
        Ok(())
    }
}

/// Errors from the paint layer: backend failures from
/// [`PaintBackend::render_into`] and validation failures of the scene or
/// render target. All variants are backend-neutral; a specific backend maps
/// its own failure modes onto them so scene consumers never see a renderer
/// type. Presentation-format concerns (packing into softbuffer's numeric
/// 0x00RRGGBB output, alpha-less pixel rejection) deliberately live outside
/// this crate, in `youth-desktop`'s presentation bridge.
#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum PaintError {
    #[error("clip stack imbalance in scene: {0}")]
    ClipImbalance(PaintSceneError),
    #[error("scene was built for {scene:?}, but render_into was asked for {requested:?}")]
    SizeMismatch {
        scene: PhysicalSize,
        requested: PhysicalSize,
    },
    #[error("size {0:?} exceeds this renderer's supported limit")]
    SizeExceedsBackendLimit(PhysicalSize),
    #[error("mask {0:?} is not present in the scene's masks")]
    InvalidMask(MaskId),
    #[error("mask {mask:?} is invalid: {reason}")]
    InvalidMaskData { mask: MaskId, reason: &'static str },
    #[error("stroke width {0} is not supported (only 1.0 is)")]
    UnsupportedStrokeWidth(f32),
    #[error("backend failure: {0}")]
    BackendFailure(&'static str),
}

/// A caller-owned, reusable rendering destination: a physical size plus a
/// row-major premultiplied RGBA8 buffer (`[r, g, b, a]` per pixel,
/// `width * height * 4` bytes total). Callers allocate one target, hand it
/// to [`PaintBackend::render_into`] every frame, and the backend resizes
/// and refills it in place -- no per-frame allocation is required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderTarget {
    size: PhysicalSize,
    pixels: Vec<u8>,
}

impl RenderTarget {
    /// Allocates a zero-filled target at `size`.
    pub fn new(size: PhysicalSize) -> Result<Self, PaintError> {
        let byte_len = render_buffer_len(size)?;
        Ok(Self {
            size,
            pixels: vec![0; byte_len],
        })
    }

    /// Resizes the target in place to `size`, reusing the buffer's
    /// allocation. New pixels are zero-filled; the previous contents are
    /// discarded.
    pub fn resize(&mut self, size: PhysicalSize) -> Result<(), PaintError> {
        let byte_len = render_buffer_len(size)?;
        self.size = size;
        self.pixels.resize(byte_len, 0);
        Ok(())
    }

    /// The target's physical dimensions.
    #[must_use]
    pub const fn size(&self) -> PhysicalSize {
        self.size
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.size.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.size.height
    }

    /// The premultiplied RGBA8 buffer, `width * height * 4` bytes in
    /// row-major order.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    #[must_use]
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }
}

/// The byte length of a premultiplied RGBA8 buffer for `size`, rejecting
/// sizes whose buffer cannot be represented (overflowing either the
/// address space or a `usize`).
fn render_buffer_len(size: PhysicalSize) -> Result<usize, PaintError> {
    let pixels = u64::from(size.width)
        .checked_mul(u64::from(size.height))
        .ok_or(PaintError::SizeExceedsBackendLimit(size))?;
    let bytes = pixels
        .checked_mul(4)
        .ok_or(PaintError::SizeExceedsBackendLimit(size))?;
    usize::try_from(bytes).map_err(|_| PaintError::SizeExceedsBackendLimit(size))
}

/// A renderer-neutral paint backend: turns a [`PaintScene`] into
/// premultiplied RGBA8 pixels in a caller-owned [`RenderTarget`].
///
/// `size` is the authoritative output size: the scene must have been built
/// for it ([`PaintError::SizeMismatch`] otherwise), and the target is
/// resized to it if needed. A backend is expected to reuse all of its own
/// resources (render context, scratch buffers, cached images) across calls
/// for the same `size`, so steady-state frames do not allocate.
pub trait PaintBackend {
    fn render_into(
        &mut self,
        size: PhysicalSize,
        scene: &PaintScene,
        target: &mut RenderTarget,
    ) -> Result<(), PaintError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        }
    }

    fn scene(commands: Vec<PaintCommand>) -> PaintScene {
        PaintScene {
            size: PhysicalSize {
                width: 10,
                height: 10,
            },
            commands,
            masks: vec![],
            images: vec![],
        }
    }

    #[test]
    fn empty_scene_and_balanced_clips_validate() {
        let empty = scene(vec![]);
        assert_eq!(empty.validate(), Ok(()));

        let balanced = scene(vec![
            PaintCommand::PushClip { rect: rect() },
            PaintCommand::FillRect {
                rect: rect(),
                color: Color::opaque(0, 0, 0),
            },
            PaintCommand::PopClip,
        ]);
        assert_eq!(balanced.validate(), Ok(()));
    }

    #[test]
    fn nested_clips_validate() {
        let nested = scene(vec![
            PaintCommand::PushClip { rect: rect() },
            PaintCommand::PushClip { rect: rect() },
            PaintCommand::PopClip,
            PaintCommand::PopClip,
        ]);
        assert_eq!(nested.validate(), Ok(()));
    }

    #[test]
    fn pop_without_push_is_underflow() {
        let scene = scene(vec![PaintCommand::PopClip]);
        assert_eq!(scene.validate(), Err(PaintSceneError::ClipUnderflow(0)));
    }

    #[test]
    fn unclosed_push_is_imbalance() {
        let unclosed = scene(vec![
            PaintCommand::PushClip { rect: rect() },
            PaintCommand::PushClip { rect: rect() },
            PaintCommand::PopClip,
        ]);
        assert_eq!(unclosed.validate(), Err(PaintSceneError::ClipImbalance(1)));
    }

    #[test]
    fn render_target_invariants() {
        let size = PhysicalSize {
            width: 4,
            height: 3,
        };
        let target = RenderTarget::new(size).unwrap();
        assert_eq!(target.size(), size);
        assert_eq!(target.pixels().len(), 4 * 3 * 4);
        assert!(target.pixels().iter().all(|&byte| byte == 0));

        // Resize reuses the allocation and refills with zeros.
        let grown = PhysicalSize {
            width: 5,
            height: 3,
        };
        let mut target = target;
        target.resize(grown).unwrap();
        assert_eq!(target.size(), grown);
        assert_eq!(target.pixels().len(), 5 * 3 * 4);
        assert!(target.pixels().iter().all(|&byte| byte == 0));

        // Shrinking keeps the larger allocation but reports the new size.
        let shrunk = PhysicalSize {
            width: 2,
            height: 2,
        };
        target.resize(shrunk).unwrap();
        assert_eq!(target.size(), shrunk);
        assert_eq!(target.pixels().len(), 2 * 2 * 4);

        // Mutation through pixels_mut is observable through pixels.
        target.pixels_mut()[0] = 7;
        assert_eq!(target.pixels()[0], 7);
    }

    #[test]
    fn render_target_rejects_unrepresentable_sizes() {
        // The byte-length computation overflows well before allocation.
        let huge = PhysicalSize {
            width: u32::MAX,
            height: u32::MAX,
        };
        assert_eq!(
            RenderTarget::new(huge),
            Err(PaintError::SizeExceedsBackendLimit(huge))
        );

        let mut target = RenderTarget::new(PhysicalSize {
            width: 1,
            height: 1,
        })
        .unwrap();
        assert_eq!(
            target.resize(huge),
            Err(PaintError::SizeExceedsBackendLimit(huge))
        );
        // A failed resize leaves the target untouched.
        assert_eq!(target.width(), 1);
        assert_eq!(target.height(), 1);
    }

    #[test]
    fn mask_and_image_scenes_round_trip() {
        let mask = AlphaMask {
            left: 1,
            top: 2,
            width: 3,
            height: 4,
            alpha: Arc::from(vec![0; 12].as_slice()),
        };
        let image = Image {
            width: 3,
            height: 4,
            pixels: Arc::from(vec![0; 48].as_slice()),
        };
        let scene = PaintScene {
            size: PhysicalSize {
                width: 10,
                height: 10,
            },
            commands: vec![PaintCommand::AlphaMask {
                mask: MaskId(0),
                origin: Point { x: 1, y: 1 },
                color: Color::opaque(1, 2, 3),
            }],
            masks: vec![mask.clone()],
            images: vec![image.clone()],
        };
        assert_eq!(scene.validate(), Ok(()));
        assert_eq!(scene.masks, vec![mask]);
        assert_eq!(scene.images, vec![image]);
    }
}
