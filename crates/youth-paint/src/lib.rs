//! Renderer-neutral paint intent for Youth's desktop presentation.
//!
//! `PaintScene`/`PaintCommand` are the seam between two concerns that used
//! to live in one function: deciding what a semantic tree, its layout, and
//! its live editor/focus/hover/fault state *mean* visually (scene
//! construction, owned by `youth-desktop::raster`) and turning that intent
//! into pixels (a paint backend -- today the existing hand-rolled
//! `FrameBuffer`, later a real rasterizer). This crate holds only the
//! shared vocabulary between those two; it has no rendering logic and no
//! dependency on any specific backend (Swash, Vello, or otherwise).
//!
//! Deliberately narrow for this increment: no rounded rects, paths,
//! gradients, images, transforms, filters, or layers. Add those only when
//! a real backend spike or a real control needs them, not speculatively.

#![forbid(unsafe_code)]

use std::sync::Arc;

use thiserror::Error;

/// A point in physical pixel space. May be negative -- a glyph mask's
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

/// The physical pixel dimensions a [`PaintScene`] was built for.
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
    /// Always a 1-physical-pixel stroke today (`width` is carried for a
    /// future real stroke algorithm, but every producer in this increment
    /// sets it to `1.0`, and every backend implements it as four 1px edge
    /// fills, matching the existing hand-rolled border exactly).
    StrokeRect {
        rect: Rect,
        width: f32,
        color: Color,
    },
    /// One rasterized glyph (or, for the bitmap-font fallback path, one
    /// whole drawn text run treated as a single synthetic glyph): `alpha`
    /// is a row-major, `size.width * size.height`-byte coverage buffer,
    /// composited at `color` starting at `origin`. `Arc` because the
    /// underlying rasterizer already caches and owns these buffers --
    /// sharing avoids a per-frame copy.
    GlyphMask {
        origin: Point,
        size: Size,
        alpha: Arc<[u8]>,
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

    #[test]
    fn empty_scene_and_balanced_clips_validate() {
        let empty = PaintScene {
            size: PhysicalSize {
                width: 10,
                height: 10,
            },
            commands: vec![],
        };
        assert_eq!(empty.validate(), Ok(()));

        let balanced = PaintScene {
            size: empty.size,
            commands: vec![
                PaintCommand::PushClip { rect: rect() },
                PaintCommand::FillRect {
                    rect: rect(),
                    color: Color::opaque(0, 0, 0),
                },
                PaintCommand::PopClip,
            ],
        };
        assert_eq!(balanced.validate(), Ok(()));
    }

    #[test]
    fn nested_clips_validate() {
        let scene = PaintScene {
            size: PhysicalSize {
                width: 10,
                height: 10,
            },
            commands: vec![
                PaintCommand::PushClip { rect: rect() },
                PaintCommand::PushClip { rect: rect() },
                PaintCommand::PopClip,
                PaintCommand::PopClip,
            ],
        };
        assert_eq!(scene.validate(), Ok(()));
    }

    #[test]
    fn pop_without_push_is_underflow() {
        let scene = PaintScene {
            size: PhysicalSize {
                width: 10,
                height: 10,
            },
            commands: vec![PaintCommand::PopClip],
        };
        assert_eq!(scene.validate(), Err(PaintSceneError::ClipUnderflow(0)));
    }

    #[test]
    fn unclosed_push_is_imbalance() {
        let scene = PaintScene {
            size: PhysicalSize {
                width: 10,
                height: 10,
            },
            commands: vec![
                PaintCommand::PushClip { rect: rect() },
                PaintCommand::PushClip { rect: rect() },
                PaintCommand::PopClip,
            ],
        };
        assert_eq!(scene.validate(), Err(PaintSceneError::ClipImbalance(1)));
    }
}
