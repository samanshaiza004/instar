//! The presentation boundary: rendered pixels into a window (WP7B2).
//!
//! `instar-paint` stops at premultiplied RGBA8 in a caller-owned buffer and
//! knows nothing about softbuffer. This is where that knowledge lives —
//! packing those bytes into softbuffer's numeric `0x00RRGGBB` words, and the
//! opacity policy that makes the conversion always representable.
//!
//! Salvaged from `youth-desktop`'s bridge, which had the same job. The
//! reasoning is unchanged and worth keeping: the destination format carries no
//! alpha, so a translucent pixel has no representation, and silently dropping
//! its coverage would be a rendering bug that looks like a design choice.

use instar_paint::{PaintCommand, PaintScene, PhysicalSize};

/// Why a frame cannot be presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    SizeExceedsLimit(PhysicalSize),
    SourceBufferLength {
        expected: usize,
        actual: usize,
    },
    DestinationBufferLength {
        expected: usize,
        actual: usize,
    },
    /// A pixel survived compositing without full alpha, so it cannot be
    /// written to an alpha-less buffer without inventing a backdrop for it.
    NonOpaquePixel {
        index: usize,
        alpha: u8,
    },
    /// The scene does not start by painting every pixel opaque, so the frame
    /// has no guarantee of being representable at all.
    MissingOpaqueClear,
}

impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeExceedsLimit(size) => {
                write!(f, "size {size:?} is larger than a buffer can represent")
            }
            Self::SourceBufferLength { expected, actual } => {
                write!(f, "rendered {actual} bytes, expected {expected}")
            }
            Self::DestinationBufferLength { expected, actual } => {
                write!(f, "window buffer holds {actual} words, expected {expected}")
            }
            Self::NonOpaquePixel { index, alpha } => write!(
                f,
                "pixel {index} composited to alpha {alpha}; an 0x00RRGGBB buffer carries none"
            ),
            Self::MissingOpaqueClear => {
                write!(f, "the scene does not open with an opaque Clear")
            }
        }
    }
}

impl std::error::Error for SurfaceError {}

/// The byte length of a premultiplied RGBA8 buffer for `size`.
pub fn buffer_len(size: PhysicalSize) -> Result<usize, SurfaceError> {
    (size.width as usize)
        .checked_mul(size.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(SurfaceError::SizeExceedsLimit(size))
}

/// Checks the invariant [`pack`] depends on: every pixel composites over an
/// opaque backdrop, because the frame begins by painting one.
///
/// A second `Clear` later in the scene is legal and expected — the crash
/// screen repaints the whole window before its text. What matters is only that
/// the *first* command leaves no pixel untouched.
pub fn check_opacity(scene: &PaintScene) -> Result<(), SurfaceError> {
    match scene.commands.first() {
        Some(PaintCommand::Clear { color }) if color.a == 255 => Ok(()),
        _ => Err(SurfaceError::MissingOpaqueClear),
    }
}

/// Packs premultiplied RGBA8 into softbuffer's numeric `0x00RRGGBB` words.
///
/// Shift-based, so the value does not depend on host endianness, and
/// allocation-free.
///
/// # Failure atomicity
///
/// The length checks run before any pixel is written, but a
/// [`SurfaceError::NonOpaquePixel`] can be discovered part-way through. The
/// caller must therefore **never present after any error from this function**:
/// the destination may hold a torn mix of this frame and the last one.
pub fn pack(src: &[u8], dst: &mut [u32], size: PhysicalSize) -> Result<(), SurfaceError> {
    let bytes = buffer_len(size)?;
    if src.len() != bytes {
        return Err(SurfaceError::SourceBufferLength {
            expected: bytes,
            actual: src.len(),
        });
    }
    if dst.len() != bytes / 4 {
        return Err(SurfaceError::DestinationBufferLength {
            expected: bytes / 4,
            actual: dst.len(),
        });
    }

    for (index, pixel) in src.chunks_exact(4).enumerate() {
        if pixel[3] != 255 {
            return Err(SurfaceError::NonOpaquePixel {
                index,
                alpha: pixel[3],
            });
        }
        // Opaque premultiplied is exactly straight RGB, so there is nothing to
        // un-premultiply.
        dst[index] = u32::from(pixel[0]) << 16 | u32::from(pixel[1]) << 8 | u32::from(pixel[2]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_paint::Color;

    fn size(width: u32, height: u32) -> PhysicalSize {
        PhysicalSize { width, height }
    }

    fn scene(commands: Vec<PaintCommand>) -> PaintScene {
        PaintScene {
            size: size(1, 1),
            commands,
            masks: Vec::new(),
            fonts: Vec::new(),
            images: Vec::new(),
        }
    }

    #[test]
    fn an_opaque_pixel_packs_to_its_straight_rgb() {
        let mut dst = [0u32; 2];
        pack(&[1, 2, 3, 255, 4, 5, 6, 255], &mut dst, size(2, 1)).expect("opaque packs");
        assert_eq!(dst, [0x00_01_02_03, 0x00_04_05_06]);
    }

    #[test]
    fn a_translucent_pixel_is_refused_rather_than_flattened() {
        let mut dst = [0u32; 1];
        assert_eq!(
            pack(&[10, 20, 30, 128], &mut dst, size(1, 1)),
            Err(SurfaceError::NonOpaquePixel {
                index: 0,
                alpha: 128
            }),
            "dropping the coverage would be a rendering bug dressed as a policy"
        );
    }

    #[test]
    fn a_mismatched_buffer_is_caught_before_anything_is_written() {
        let mut dst = [0xdead_beef_u32; 4];
        assert!(matches!(
            pack(&[0; 8], &mut dst, size(2, 1)),
            Err(SurfaceError::DestinationBufferLength { .. })
        ));
        assert_eq!(
            dst, [0xdead_beef; 4],
            "nothing may be written on a size error"
        );
    }

    #[test]
    fn a_scene_without_an_opaque_opening_clear_is_not_presentable() {
        assert_eq!(
            check_opacity(&scene(vec![])),
            Err(SurfaceError::MissingOpaqueClear)
        );
        assert_eq!(
            check_opacity(&scene(vec![PaintCommand::Clear {
                color: Color::opaque(0, 0, 0).with_alpha(200)
            }])),
            Err(SurfaceError::MissingOpaqueClear)
        );
        assert_eq!(
            check_opacity(&scene(vec![PaintCommand::Clear {
                color: Color::opaque(0, 0, 0)
            }])),
            Ok(())
        );
    }

    #[test]
    fn an_unrepresentable_size_is_rejected_rather_than_wrapped() {
        assert!(matches!(
            buffer_len(size(u32::MAX, u32::MAX)),
            Err(SurfaceError::SizeExceedsLimit(_))
        ));
    }
}
