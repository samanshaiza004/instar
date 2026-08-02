//! CPU glyph rasterization for Youth's Editor rendering, built on Swash.
//!
//! This crate is the only place in the Youth workspace that depends on
//! `swash`. It consumes [`youth_editor_engine::TextPresentation`] (glyph
//! runs plus selection/cursor geometry) and produces alpha-coverage
//! [`GlyphMask`]s a caller composites into its own pixel buffer -- this
//! crate never touches `youth-desktop`'s `FrameBuffer` type directly, so
//! the dependency runs one way (`youth-desktop` depends on this crate, not
//! the reverse).
//!
//! `#![forbid(unsafe_code)]` covers Youth-authored code in this crate only;
//! it cannot and does not forbid unsafe code inside `swash` or its own
//! dependencies. If a future increment demonstrates Youth-owned unsafe is
//! genuinely unavoidable here, loosening this attribute is a separate,
//! explicitly reviewed decision -- not something to pre-empt now.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use swash::FontRef;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::Format;
use youth_editor_engine::{FontHandle, TextPresentation};

/// An owned alpha-coverage bitmap for one rasterized glyph, positioned
/// relative to the glyph's pen (baseline) origin.
#[derive(Clone, Debug, Default)]
pub struct GlyphMask {
    /// Horizontal offset from the rounded pen position to the mask's
    /// left edge.
    pub left: i32,
    /// Vertical offset from the rounded pen (baseline) position to the
    /// mask's top edge, measured upward (subtract to get the top edge in
    /// a downward-increasing pixel coordinate system).
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// Row-major 8-bit alpha coverage, `width * height` bytes.
    pub alpha: Vec<u8>,
}

impl GlyphMask {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Reads the coverage at `(x, y)` within the mask, `0` outside bounds.
    #[must_use]
    pub fn coverage_at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.alpha[(y * self.width + x) as usize]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct GlyphCacheKey {
    font_id: u64,
    glyph_id: u32,
    /// `f32::to_bits()` of the font size, for a hashable exact key. Two
    /// sizes that are bit-identical share a cache entry; anything else
    /// rasterizes separately, which is correct (just not maximally
    /// cache-efficient across float rounding noise).
    size_bits: u32,
}

/// Rasterizes glyphs from a [`TextPresentation`] into cached, owned
/// [`GlyphMask`]s. One rasterizer is cheap to keep per live Editor
/// presentation; it owns Swash's scratch scaling context and a glyph mask
/// cache keyed by (font identity, glyph id, size).
pub struct GlyphRasterizer {
    context: ScaleContext,
    cache: HashMap<GlyphCacheKey, GlyphMask>,
}

impl std::fmt::Debug for GlyphRasterizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlyphRasterizer")
            .field("cached_glyph_count", &self.cache.len())
            .finish_non_exhaustive()
    }
}

impl Default for GlyphRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

impl GlyphRasterizer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            context: ScaleContext::new(),
            cache: HashMap::new(),
        }
    }

    /// Rasterizes (or returns the cached rasterization of) one glyph.
    /// Returns `None` only if the font data itself cannot be parsed by
    /// Swash; a glyph with no visible ink (e.g. space) is `Some` with an
    /// empty mask, not `None`, so callers can distinguish "nothing to
    /// paint" from "font failed to load."
    pub fn rasterize(
        &mut self,
        font: &FontHandle,
        glyph_id: u32,
        font_size: f32,
    ) -> Option<&GlyphMask> {
        let key = GlyphCacheKey {
            font_id: font.data.id(),
            glyph_id,
            size_bits: font_size.to_bits(),
        };
        if !self.cache.contains_key(&key) {
            let mask = rasterize_uncached(&mut self.context, font, glyph_id, font_size)?;
            self.cache.insert(key, mask);
        }
        self.cache.get(&key)
    }

    /// Drops all cached masks. Useful if memory pressure ever matters more
    /// than avoiding re-rasterization; not required for correctness.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    #[must_use]
    pub fn cached_glyph_count(&self) -> usize {
        self.cache.len()
    }
}

fn rasterize_uncached(
    context: &mut ScaleContext,
    font: &FontHandle,
    glyph_id: u32,
    font_size: f32,
) -> Option<GlyphMask> {
    let glyph_id = u16::try_from(glyph_id).ok()?;
    let font_ref = FontRef::from_index(font.data.data(), font.index as usize)?;
    let mut scaler = context.builder(font_ref).size(font_size).hint(true).build();
    let image = Render::new(&[Source::Bitmap(StrikeWith::BestFit), Source::Outline])
        .format(Format::Alpha)
        .render(&mut scaler, glyph_id)?;
    Some(GlyphMask {
        left: image.placement.left,
        top: image.placement.top,
        width: image.placement.width,
        height: image.placement.height,
        alpha: image.data,
    })
}

/// Visits every glyph in `presentation`, positioned relative to
/// `(origin_x, origin_y)`, invoking `plot(x, y, coverage)` for each
/// non-zero-coverage pixel in absolute pixel coordinates (Y increasing
/// downward, matching [`youth_editor_engine`]'s layout coordinate space).
///
/// This performs no compositing itself -- the caller decides how coverage
/// maps to color and blends into its own pixel buffer, which is why this
/// crate has no dependency on any specific framebuffer type.
pub fn paint_presentation(
    rasterizer: &mut GlyphRasterizer,
    presentation: &TextPresentation,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    mut plot: impl FnMut(i32, i32, u8),
) {
    for run in &presentation.runs {
        for glyph in &run.glyphs {
            let Some(mask) = rasterizer.rasterize(&run.font, glyph.id, run.font_size * scale)
            else {
                continue;
            };
            if mask.is_empty() {
                continue;
            }
            let pen_x = (origin_x + glyph.x * scale).round() as i32;
            let pen_y = (origin_y + glyph.y * scale).round() as i32;
            let base_x = pen_x + mask.left;
            let base_y = pen_y - mask.top;
            for row in 0..mask.height {
                for col in 0..mask.width {
                    let coverage = mask.coverage_at(col, row);
                    if coverage > 0 {
                        plot(base_x + col as i32, base_y + row as i32, coverage);
                    }
                }
            }
        }
    }
}

/// Bundled Roboto Mono (OFL-1.1, see `assets/OFL.txt`), for deterministic
/// tests that must not depend on which fonts happen to be installed on the
/// host running them. Only glyph-count/geometry/bounded-region assertions
/// against this font are made today; promoting any of this to an exact
/// pixel-hash snapshot is deferred to when CI actually provisions the
/// canonical Linux runner that determinism claim depends on (see the
/// crate-level testing policy notes in the Gate A design doc) -- asserting
/// it now against an unverified environment would be the same mistake the
/// design doc explicitly warns against repeating.
#[cfg(test)]
const BUNDLED_TEST_FONT: &[u8] = include_bytes!("../assets/RobotoMono.ttf");

#[cfg(test)]
mod tests {
    use super::*;
    use youth_editor_engine::{EditorLayout, ParleyEditorEngine};

    #[test]
    fn bundled_test_font_is_valid_and_rasterizable() {
        let font_ref = FontRef::from_index(BUNDLED_TEST_FONT, 0)
            .expect("the bundled font file parses as a valid font");
        let mut context = ScaleContext::new();
        let mut scaler = context.builder(font_ref).size(16.0).hint(true).build();
        assert!(
            scaler.has_outlines(),
            "the bundled font provides scalable outlines"
        );
        // 'A' in Roboto Mono's default cmap -- a stable, well-known glyph
        // to sanity-check rasterization end-to-end against the exact
        // bundled bytes, independent of whatever fonts the host has
        // installed.
        let glyph_id = font_ref.charmap().map('A');
        assert_ne!(glyph_id, 0, "the bundled font maps 'A' to a real glyph");
        let image = Render::new(&[Source::Outline])
            .format(Format::Alpha)
            .render(&mut scaler, glyph_id)
            .expect("rasterizing a real glyph from the bundled font succeeds");
        assert!(
            image.placement.width > 0 && image.placement.height > 0,
            "the bundled font's 'A' glyph produces a non-empty raster"
        );
    }

    #[test]
    fn rasterizing_a_visible_glyph_produces_a_non_empty_mask() {
        let mut engine = ParleyEditorEngine::with_text("A");
        let presentation = engine.presentation();
        let run = presentation
            .runs
            .first()
            .expect("at least one glyph run for a visible character");
        let glyph = run.glyphs.first().expect("at least one glyph");

        let mut rasterizer = GlyphRasterizer::new();
        let mask = rasterizer
            .rasterize(&run.font, glyph.id, run.font_size)
            .expect("rasterization succeeds for a real system font");
        assert!(!mask.is_empty(), "a visible letter must produce ink");
        assert!(
            mask.alpha.iter().any(|&byte| byte > 0),
            "some pixel has coverage"
        );
    }

    #[test]
    fn rasterization_is_cached_across_repeated_calls() {
        let mut engine = ParleyEditorEngine::with_text("AAA");
        let presentation = engine.presentation();
        let run = presentation.runs.first().expect("a glyph run");

        let mut rasterizer = GlyphRasterizer::new();
        for glyph in &run.glyphs {
            rasterizer
                .rasterize(&run.font, glyph.id, run.font_size)
                .expect("rasterization succeeds");
        }
        // Three identical 'A' glyphs at the same size share one cache
        // entry rather than three.
        assert_eq!(rasterizer.cached_glyph_count(), 1);
    }

    #[test]
    fn paint_presentation_visits_a_bounded_nonzero_pixel_region() {
        let mut engine = ParleyEditorEngine::with_text("Hi");
        let presentation = engine.presentation();

        let mut rasterizer = GlyphRasterizer::new();
        let mut painted = Vec::new();
        paint_presentation(
            &mut rasterizer,
            &presentation,
            0.0,
            0.0,
            1.0,
            |x, y, coverage| {
                painted.push((x, y, coverage));
            },
        );

        assert!(!painted.is_empty(), "painting visible text plots pixels");
        let max_x = painted.iter().map(|(x, ..)| *x).max().unwrap();
        let max_y = painted.iter().map(|(_, y, _)| *y).max().unwrap();
        // A bounded region check: two short characters should not paint
        // pixels far outside the presentation's own reported content size.
        assert!(
            f64::from(max_x) <= f64::from(presentation.content_width) + 32.0,
            "painted x extent ({max_x}) should stay near content_width ({})",
            presentation.content_width
        );
        assert!(
            f64::from(max_y) <= f64::from(presentation.content_height) + 32.0,
            "painted y extent ({max_y}) should stay near content_height ({})",
            presentation.content_height
        );
    }

    #[test]
    fn an_empty_buffer_paints_nothing() {
        let mut engine = ParleyEditorEngine::with_text("");
        let presentation = engine.presentation();
        let mut rasterizer = GlyphRasterizer::new();
        let mut painted = Vec::new();
        paint_presentation(
            &mut rasterizer,
            &presentation,
            0.0,
            0.0,
            1.0,
            |x, y, coverage| {
                painted.push((x, y, coverage));
            },
        );
        assert!(painted.is_empty());
    }
}
