//! Presentation-boundary bridge between Youth's render output and
//! softbuffer's numeric `0x00RRGGBB` buffer format.
//!
//! `youth-paint` deliberately stops at premultiplied RGBA8 pixels; it has no
//! knowledge of softbuffer or any presentation format. This module is where
//! that knowledge lives: packing a caller-owned premultiplied RGBA8 buffer
//! into softbuffer's packed u32 words, the alpha policy that makes such a
//! conversion always representable, and the scene-opacity contract that
//! guarantees that policy. Nothing here allocates.

use thiserror::Error;
use youth_paint::{PaintCommand, PaintScene, PhysicalSize};

/// Errors from converting premultiplied RGBA8 render output into softbuffer's
/// packed `0x00RRGGBB` u32 format, plus the scene-opacity policy that
/// guarantees such a conversion is representable.
///
/// These are presentation errors, deliberately separate from
/// `youth_paint::PaintError` (which stays focused on paint/backend concerns):
/// a native presenter turns any `SoftbufferError` into a "do not present"
/// decision.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SoftbufferError {
    /// The size's buffer cannot be represented (overflowing either the
    /// address space or a `usize`), so no source/destination could ever
    /// match it.
    #[error("size {0:?} exceeds what a softbuffer 0x00RRGGBB buffer can represent")]
    SizeExceedsLimit(PhysicalSize),
    #[error(
        "source RGBA8 buffer length {actual} does not match size {size:?} (expected {expected} bytes)"
    )]
    SourceBufferLength {
        size: PhysicalSize,
        expected: usize,
        actual: usize,
    },
    #[error(
        "destination buffer length {actual} does not match size {size:?} (expected {expected} words)"
    )]
    DestinationBufferLength {
        size: PhysicalSize,
        expected: usize,
        actual: usize,
    },
    #[error(
        "pixel {index} is translucent (premultiplied alpha {alpha}, expected 255); the 0x00RRGGBB output cannot carry alpha"
    )]
    NonOpaquePixel { index: usize, alpha: u8 },
    /// The scene's first command is not a `Clear` at all.
    #[error(
        "the scene's first command must be an opaque Clear; softbuffer's 0x00RRGGBB output cannot carry the frame's alpha"
    )]
    MissingOpaqueInitialClear,
    /// The scene's first command is a `Clear` whose alpha is not 255.
    #[error("the scene's first Clear has alpha {0}, expected 255")]
    InitialClearNotOpaque(u8),
}

/// Converts a caller-owned premultiplied RGBA8 buffer into softbuffer's
/// packed numeric `0x00RRGGBB` u32 output, writing each pixel's value in
/// scanline order. `src` is `[r, g, b, a]` per pixel, row-major, exactly
/// `size.width * size.height * 4` bytes; `dst` must hold exactly
/// `size.width * size.height` words. The packing is shift-based, so the
/// numeric value is independent of host endianness, and no allocation is
/// performed.
///
/// # Alpha policy
///
/// Softbuffer's numeric format carries no alpha channel and the Youth scene
/// contract clears every frame opaque ([`validate_scene_opacity`]), so a
/// premultiplied pixel with alpha != 255 cannot be represented and is
/// rejected with [`SoftbufferError::NonOpaquePixel`] instead of silently
/// dropping its coverage. Opaque premultiplied pixels (alpha 255) are
/// exactly their straight RGB value, so no un-premultiplication is needed.
///
/// # Failure atomicity
///
/// Only the length checks are atomic (they run before any pixel is
/// written). A late [`SoftbufferError::NonOpaquePixel`] failure may leave
/// earlier pixels already written to `dst` -- the destination is partially
/// modified, and the caller must therefore **never present after any
/// `SoftbufferError`**, since a rejected frame may be a torn mix of old and
/// new pixels.
pub fn convert_rgba8_to_rgbx32(
    src: &[u8],
    dst: &mut [u32],
    size: PhysicalSize,
) -> Result<(), SoftbufferError> {
    let byte_len = premultiplied_rgba8_buffer_len(size)?;
    if src.len() != byte_len {
        return Err(SoftbufferError::SourceBufferLength {
            size,
            expected: byte_len,
            actual: src.len(),
        });
    }
    let word_len = byte_len / 4;
    if dst.len() != word_len {
        return Err(SoftbufferError::DestinationBufferLength {
            size,
            expected: word_len,
            actual: dst.len(),
        });
    }
    for (index, pixel) in src.chunks_exact(4).enumerate() {
        if pixel[3] != 255 {
            return Err(SoftbufferError::NonOpaquePixel {
                index,
                alpha: pixel[3],
            });
        }
        dst[index] = u32::from(pixel[0]) << 16 | u32::from(pixel[1]) << 8 | u32::from(pixel[2]);
    }
    Ok(())
}

/// Validates the scene-opacity contract the softbuffer output depends on:
/// the first command of every scene rendered to the window must be an
/// opaque `Clear` (`alpha == 255`), so every final pixel composites over an
/// opaque backdrop and the alpha-less `0x00RRGGBB` output is always
/// representable.
///
/// Translucent fills/glyph masks/selection rects/clips may follow freely --
/// the initial clear guarantees their source-over results stay opaque. A
/// second `Clear` is also legal: the intentional R0 fault overlay repaints
/// the whole window with an opaque fault background before its text, and
/// that second clear is the deliberate, documented exception to a
/// hypothetical exactly-one-clear rule -- it is not deleted or reworked
/// here. The final per-pixel authority remains
/// [`convert_rgba8_to_rgbx32`], which rejects any non-opaque pixel that
/// survives compositing.
pub fn validate_scene_opacity(scene: &PaintScene) -> Result<(), SoftbufferError> {
    match scene.commands.first() {
        Some(PaintCommand::Clear { color }) if color.a == 255 => Ok(()),
        Some(PaintCommand::Clear { color }) => Err(SoftbufferError::InitialClearNotOpaque(color.a)),
        _ => Err(SoftbufferError::MissingOpaqueInitialClear),
    }
}

/// The byte length of a premultiplied RGBA8 buffer for `size`, rejecting
/// sizes whose buffer cannot be represented (overflowing either the address
/// space or a `usize`) -- 32-bit-safe the same way render-target sizing is:
/// the pixel count and byte count are computed in u64 and narrowed with
/// `usize::try_from`, so 32-bit platforms never get a lossy cast.
fn premultiplied_rgba8_buffer_len(size: PhysicalSize) -> Result<usize, SoftbufferError> {
    let pixels = u64::from(size.width)
        .checked_mul(u64::from(size.height))
        .ok_or(SoftbufferError::SizeExceedsLimit(size))?;
    let bytes = pixels
        .checked_mul(4)
        .ok_or(SoftbufferError::SizeExceedsLimit(size))?;
    usize::try_from(bytes).map_err(|_| SoftbufferError::SizeExceedsLimit(size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use youth_paint::{Color, MaskId, Point, Rect};

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

    fn opaque_clear() -> PaintCommand {
        PaintCommand::Clear {
            color: Color::opaque(0, 0, 0),
        }
    }

    #[test]
    fn conversion_packs_row_major_scanlines_into_rgb_order() {
        let size = PhysicalSize {
            width: 2,
            height: 2,
        };
        // Pixel (x, y): (0,0) red, (1,0) green, (0,1) blue, (1,1) white,
        // written as [r, g, b, a] row-major.
        let src = [
            255, 0, 0, 255, 0, 255, 0, 255, // row 0
            0, 0, 255, 255, 255, 255, 255, 255, // row 1
        ];
        let mut dst = [0u32; 4];
        convert_rgba8_to_rgbx32(&src, &mut dst, size).unwrap();
        assert_eq!(dst, [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0x00ff_ffff]);
    }

    #[test]
    fn conversion_requires_exact_source_and_destination_lengths() {
        let size = PhysicalSize {
            width: 2,
            height: 2,
        };
        let src = vec![255u8; 16];
        let mut dst = vec![0u32; 4];

        // Destination too short...
        let mut short = vec![0u32; 3];
        assert_eq!(
            convert_rgba8_to_rgbx32(&src, &mut short, size),
            Err(SoftbufferError::DestinationBufferLength {
                size,
                expected: 4,
                actual: 3,
            })
        );
        // ...and too long.
        let mut long = vec![0u32; 5];
        assert_eq!(
            convert_rgba8_to_rgbx32(&src, &mut long, size),
            Err(SoftbufferError::DestinationBufferLength {
                size,
                expected: 4,
                actual: 5,
            })
        );

        // Source too short...
        assert_eq!(
            convert_rgba8_to_rgbx32(&src[..15], &mut dst, size),
            Err(SoftbufferError::SourceBufferLength {
                size,
                expected: 16,
                actual: 15,
            })
        );
        // ...and too long.
        let long_src = vec![255u8; 17];
        assert_eq!(
            convert_rgba8_to_rgbx32(&long_src, &mut dst, size),
            Err(SoftbufferError::SourceBufferLength {
                size,
                expected: 16,
                actual: 17,
            })
        );

        // A size whose buffer cannot be represented is rejected before any
        // length check, on 32-bit and 64-bit alike.
        let huge = PhysicalSize {
            width: u32::MAX,
            height: u32::MAX,
        };
        assert_eq!(
            convert_rgba8_to_rgbx32(&[], &mut [], huge),
            Err(SoftbufferError::SizeExceedsLimit(huge))
        );
    }

    #[test]
    fn conversion_rejects_translucent_premultiplied_pixels() {
        let size = PhysicalSize {
            width: 2,
            height: 1,
        };
        // Fully transparent black (premultiplied alpha 0)...
        let src = [0, 0, 0, 0, 255, 255, 255, 255];
        let mut dst = [0u32; 2];
        assert_eq!(
            convert_rgba8_to_rgbx32(&src, &mut dst, size),
            Err(SoftbufferError::NonOpaquePixel { index: 0, alpha: 0 })
        );

        // ...and a half-alpha premultiplied red (partial/translucent
        // premultiplied bytes) are both rejected rather than silently
        // losing alpha; a failure at the first pixel leaves the
        // destination untouched.
        let translucent = [128, 0, 0, 128, 255, 255, 255, 255];
        let mut dst = [0u32; 2];
        assert_eq!(
            convert_rgba8_to_rgbx32(&translucent, &mut dst, size),
            Err(SoftbufferError::NonOpaquePixel {
                index: 0,
                alpha: 128
            })
        );
        assert_eq!(dst, [0u32; 2]);

        // A translucent pixel past an opaque one reports its own index.
        let mixed = [255, 255, 255, 255, 128, 0, 0, 128];
        let mut dst = [0u32; 2];
        assert_eq!(
            convert_rgba8_to_rgbx32(&mixed, &mut dst, size),
            Err(SoftbufferError::NonOpaquePixel {
                index: 1,
                alpha: 128
            })
        );

        // Fully opaque premultiplied pixels convert exactly.
        let opaque = [1, 2, 3, 255, 4, 5, 6, 255];
        let mut dst = [0u32; 2];
        convert_rgba8_to_rgbx32(&opaque, &mut dst, size).unwrap();
        assert_eq!(dst, [0x0001_0203, 0x0004_0506]);
    }

    #[test]
    fn a_late_translucent_pixel_partially_modifies_the_destination() {
        let size = PhysicalSize {
            width: 3,
            height: 1,
        };
        // Two opaque pixels followed by a translucent one: the conversion
        // must have written the first two destination words before the
        // third pixel's rejection, so the destination is left partially
        // modified. The caller must never present after any bridge error --
        // exactly why this is documented and tested rather than hidden.
        let src = [255, 255, 255, 255, 1, 2, 3, 255, 0, 0, 0, 0];
        let mut dst = [9u32; 3];
        assert_eq!(
            convert_rgba8_to_rgbx32(&src, &mut dst, size),
            Err(SoftbufferError::NonOpaquePixel { index: 2, alpha: 0 })
        );
        assert_eq!(
            dst,
            [0x00ff_ffff, 0x0001_0203, 9],
            "pixels before the failure are converted; the failed pixel and beyond are untouched"
        );
    }

    #[test]
    fn initial_opaque_clear_is_required_and_later_transparency_is_allowed() {
        // The normal opaque window: an opaque Clear first, then translucent
        // fills/masks/clips may follow freely.
        let normal = scene(vec![
            opaque_clear(),
            PaintCommand::FillRect {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                color: Color::with_alpha(Color::opaque(255, 0, 0), 128),
            },
            PaintCommand::PushClip {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                },
            },
            PaintCommand::PopClip,
        ]);
        assert_eq!(validate_scene_opacity(&normal), Ok(()));

        // The intentional R0 fault overlay appends a second opaque Clear
        // after the normal scene; that second clear is the documented
        // exception to an exactly-one-clear rule and must still validate.
        let fault = scene(vec![
            opaque_clear(),
            PaintCommand::Clear {
                color: Color::opaque(72, 24, 32),
            },
            PaintCommand::AlphaMask {
                mask: MaskId(0),
                origin: Point { x: 16, y: 16 },
                color: Color::opaque(255, 0, 0),
            },
        ]);
        assert_eq!(validate_scene_opacity(&fault), Ok(()));
    }

    #[test]
    fn a_missing_or_translucent_initial_clear_is_rejected() {
        assert_eq!(
            validate_scene_opacity(&scene(vec![])),
            Err(SoftbufferError::MissingOpaqueInitialClear)
        );
        assert_eq!(
            validate_scene_opacity(&scene(vec![PaintCommand::FillRect {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                color: Color::opaque(255, 0, 0),
            }])),
            Err(SoftbufferError::MissingOpaqueInitialClear)
        );
        assert_eq!(
            validate_scene_opacity(&scene(vec![PaintCommand::Clear {
                color: Color::with_alpha(Color::opaque(0, 0, 0), 128),
            }])),
            Err(SoftbufferError::InitialClearNotOpaque(128))
        );
    }
}
