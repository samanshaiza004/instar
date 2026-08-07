//! Characters to glyphs, and nothing else (WP7B2).
//!
//! This is deliberately not a text stack. It implements
//! [`instar_host::GlyphSource`], whose whole contract is "which glyph is this
//! character, in this face" — advances come from
//! [`instar_ui::TEXT_METRICS`], because the host measured its boxes with them
//! and a shaper that disagreed would push text out of the rectangles layout
//! computed for it.
//!
//! That constraint is why the font here is monospaced. A proportional face
//! would render each glyph at a fixed-pitch advance and look wrong in a way
//! that is nobody's bug in particular. When real shaping lands, measurement
//! and positioning come from one font context and this module disappears into
//! it; until then, being obviously provisional is better than being subtly
//! wrong.

use std::sync::Arc;

use instar_host::GlyphSource;
use instar_paint::{FontKey, FontResource};
use skrifa::prelude::*;
use skrifa::{FontRef, MetadataProvider};

/// The first and last code points cached up front.
const ASCII_FIRST: u32 = 0x20;
const ASCII_LAST: u32 = 0x7e;

/// A monospaced face, with its printable-ASCII glyph ids resolved once.
///
/// The bytes and the resolved table are kept, but not a `FontRef` — that
/// borrows the bytes, and a struct holding both would be self-referential for
/// no gain. Anything outside printable ASCII re-parses the face, which is the
/// rare path and stays correct.
pub struct MonoFont {
    data: Arc<[u8]>,
    index: u32,
    key: FontKey,
    /// Glyph ids for [`ASCII_FIRST`]..=[`ASCII_LAST`], `None` where the face
    /// has no glyph.
    ascii: Vec<Option<u32>>,
    /// Advance width of one character as a fraction of an em. Used only to
    /// answer [`GlyphSource::em_size`].
    advance_per_em: f32,
}

#[derive(Debug)]
pub enum FontError {
    Unparsable,
    /// A face with no advance to scale against cannot be sized to the layout's
    /// character width, which is the only thing this shell knows how to do.
    NoAdvance,
}

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparsable => write!(f, "the font could not be parsed"),
            Self::NoAdvance => write!(f, "the font reports no usable advance width"),
        }
    }
}

impl std::error::Error for FontError {}

impl MonoFont {
    /// Parses a face and resolves its ASCII glyphs.
    ///
    /// `key` must change whenever the bytes do: the renderer caches converted
    /// faces by it, and a stale entry would draw the old font's glyph ids.
    pub fn new(data: Arc<[u8]>, index: u32, key: FontKey) -> Result<Self, FontError> {
        let font = FontRef::from_index(&data, index).map_err(|_| FontError::Unparsable)?;
        let charmap = font.charmap();

        let ascii = (ASCII_FIRST..=ASCII_LAST)
            .map(|code| {
                char::from_u32(code)
                    .and_then(|ch| charmap.map(ch))
                    .map(|id| id.to_u32())
            })
            .collect();

        // Monospaced, so any glyph's advance is every glyph's advance; 'M' is
        // as good a witness as any and is present in every text face.
        let metrics = font.glyph_metrics(Size::unscaled(), LocationRef::default());
        let advance = charmap
            .map('M')
            .and_then(|id| metrics.advance_width(id))
            .filter(|advance| *advance > 0.0)
            .ok_or(FontError::NoAdvance)?;
        let upem = f32::from(
            font.metrics(Size::unscaled(), LocationRef::default())
                .units_per_em,
        );

        Ok(Self {
            data,
            index,
            key,
            ascii,
            advance_per_em: advance / upem,
        })
    }
}

impl GlyphSource for MonoFont {
    fn font(&self) -> FontResource {
        FontResource {
            key: self.key,
            // A refcount bump, not a copy of the font file — which matters,
            // because this is called once per scene and scenes are built per
            // click.
            data: Arc::clone(&self.data),
            index: self.index,
        }
    }

    fn glyph(&self, ch: char) -> Option<u32> {
        let code = ch as u32;
        if (ASCII_FIRST..=ASCII_LAST).contains(&code) {
            return self.ascii[(code - ASCII_FIRST) as usize];
        }
        // Outside the cached range. Re-parsing here is wasteful and correct,
        // and no Instar text is currently in this path.
        let font = FontRef::from_index(&self.data, self.index).ok()?;
        font.charmap().map(ch).map(|id| id.to_u32())
    }

    fn em_size(&self, char_width: f32) -> f32 {
        // Invert the face's own advance so one glyph occupies exactly the
        // advance layout measured with. The host's geometry is the authority;
        // the font is fitted to it, not the other way round.
        char_width / self.advance_per_em
    }
}

impl std::fmt::Debug for MonoFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonoFont")
            .field("key", &self.key)
            .field("index", &self.index)
            .field("bytes", &self.data.len())
            .field("advance_per_em", &self.advance_per_em)
            .finish()
    }
}

/// The face the shell ships.
///
/// Roboto Mono, under the SIL Open Font License; `assets/OFL.txt` is the
/// licence text. Inherited from the predecessor codebase's text renderer and
/// moved here in WP10, when that crate was removed — a shipped binary should
/// not reach into a deleted crate's directory for the font it draws with.
///
/// Instar has no font story of its own yet. When it grows one, this constant
/// is where it starts.
pub const ROBOTO_MONO: &[u8] = include_bytes!("../assets/RobotoMono.ttf");

/// The shipped face, ready to use.
pub fn default_font() -> Result<MonoFont, FontError> {
    MonoFont::new(Arc::from(ROBOTO_MONO), 0, FontKey(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_ui::TEXT_METRICS;

    #[test]
    fn the_shipped_face_parses_and_resolves_ascii() {
        let font = default_font().expect("the shipped face should parse");
        for ch in ' '..='~' {
            assert!(
                font.glyph(ch).is_some(),
                "the shipped face should have a glyph for {ch:?}"
            );
        }
    }

    /// The property that keeps text inside the boxes the host computed: at the
    /// size this font reports, one glyph advances exactly one layout column.
    #[test]
    fn the_em_size_makes_one_glyph_one_layout_column() {
        let font = default_font().expect("the shipped face should parse");
        let column = TEXT_METRICS.char_width;
        let em = font.em_size(column);

        let advance = em * font.advance_per_em;
        assert!(
            (advance - column).abs() < 0.01,
            "at {em}px/em one glyph advances {advance}px, but layout measured \
             its boxes at {column}px per character"
        );
    }

    #[test]
    fn scaling_the_column_scales_the_size_with_it() {
        let font = default_font().expect("the shipped face should parse");
        assert!(
            (font.em_size(16.0) - font.em_size(8.0) * 2.0).abs() < 0.01,
            "a 2x display doubles the physical column, so it must double the \
             em size -- otherwise text stops matching its boxes off 1x"
        );
    }

    #[test]
    fn a_face_that_does_not_parse_is_refused_rather_than_drawn() {
        assert!(matches!(
            MonoFont::new(Arc::from(&b"not a font"[..]), 0, FontKey(9)),
            Err(FontError::Unparsable)
        ));
    }
}
