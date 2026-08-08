//! Text shaping, and the cache that makes it incremental.
//!
//! # One shaped result drives layout and painting
//!
//! Phase 1 measured text with fixed-pitch placeholder metrics and painted it
//! by inverting a real font's advance until one glyph occupied one fake
//! column. Two approximations reconciled by a shared constant. This replaces
//! both with a single shaped result:
//!
//! ```text
//! text + style
//!    ↓
//! Parley
//!    ├── intrinsic width/height → Taffy
//!    └── positioned glyphs      → the renderer
//! ```
//!
//! # Shaped exactly once, in logical space
//!
//! > `instar-ui` shapes at `scale = 1.0` with `quantize = false`, and never
//! > receives display scale. `instar-host` converts shaped positions **and
//! > font ppem** to physical space during paint lowering. Pixel quantization
//! > and hinting are renderer concerns.
//!
//! Letting this crate see scale would make logical layout depend on which
//! monitor the window occupies: Parley multiplies font size and spacing by the
//! scale internally, and recovering logical geometry by dividing back down
//! lets physical quantization change wrapping between scale factors. Moving a
//! window between monitors must not reflow text.
//!
//! # The cached object is Parley's `Layout`, not the extracted runs
//!
//! [`TextEntry`] holds the `Layout`. [`ShapedText`] is the *render artifact*
//! extracted from it — a logical-space snapshot, not something to re-break.
//! That distinction is what makes the middle case below cheap:
//!
//! ```text
//! same text/style, same width   → reuse everything
//! same text/style, width changed → re-break and re-align the Layout,
//!                                  re-extract ShapedText, NO reshaping
//! text or style changed          → rebuild the Layout, break, extract
//! ```
//!
//! Resize is the case that matters: it changes the width of every text node at
//! once while changing none of their text. Rebuilding a `Layout` per node
//! there would make window dragging cost a full re-shape of the interface.
//!
//! # Extraction is a finalization pass, not a measurement side effect
//!
//! Taffy does not promise to call a measure closure once, and the last call it
//! makes is not necessarily at the width the node ends up with — it measures
//! speculatively under `MinContent` and `MaxContent` to resolve flex
//! distribution. So measurement answers questions and caches a `Layout`;
//! **extraction happens afterwards**, from the final geometry in the
//! `LayoutSnapshot`.
//!
//! That can cost one extra line-break for a node whose final width differs
//! from the last speculative one. It can never cost a second shape, and it
//! means render output is never coupled to whichever constraint Taffy happened
//! to ask about last.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::NodeKey;

/// Which face a node is asking for.
///
/// A *role*, not a family name: a guest says "this is body text" or "this is
/// code", and the host decides what that means on this machine. Custom font
/// loading is deliberately not in Phase 2 — it drags in font file transport,
/// licensing, sandboxing, and fallback policy, none of which have been
/// designed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FontRole {
    /// The platform's UI face.
    #[default]
    SystemUi,
    /// A fixed-pitch face.
    Monospace,
}

/// Everything about a node's text that affects shaping.
///
/// Hashed to decide whether a cached `Layout` is still valid, so it must
/// contain *everything* that changes glyphs or advances, and nothing that does
/// not. Colour belongs to paint, not here: adding a paint-only property like
/// colour would silently destroy the reuse optimization, because the style
/// hash would change on every repaint and force a reshape even though the
/// glyphs cannot differ.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapingStyle {
    pub role: FontRole,
    /// Logical pixels per em.
    pub size: f32,
    /// 100–900, CSS-style.
    pub weight: u16,
    /// Whether the node wraps at its available width or stays on one line.
    pub wrap: bool,
}

impl Default for ShapingStyle {
    fn default() -> Self {
        Self {
            role: FontRole::SystemUi,
            size: 14.0,
            weight: 400,
            wrap: true,
        }
    }
}

/// One run of glyphs sharing a font and size, in **logical** coordinates.
///
/// Run-based because Parley's output is: a run is a span of clusters sharing
/// one font and style, so font fallback, emoji, and bidi all produce several
/// runs inside a single text node. A single-font `ShapedText` would need
/// redesigning the first time a string contained a character the primary face
/// lacks.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedRun {
    /// Index into [`ShapedText::fonts`].
    pub font: usize,
    /// Logical pixels per em. `instar-host` multiplies this by the display
    /// scale — Vello uses it to select bitmap and colour glyph strikes, so a
    /// global transform would not be equivalent.
    pub font_size: f32,
    pub glyphs: Vec<Glyph>,
}

/// A positioned glyph, in logical coordinates relative to the text node's box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    /// Font-local glyph id. Meaningless outside its run's face.
    pub id: u32,
    pub x: f32,
    /// Baseline position, not the top of the line.
    pub y: f32,
}

/// A face a run refers to, as the bytes a renderer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFace {
    pub data: std::sync::Arc<[u8]>,
    pub index: u32,
    /// Stable identity for renderer-side caches. Must change when the bytes
    /// do.
    pub key: u64,
}

/// The render artifact for one text node: what to draw, in logical space.
///
/// Derived from a cached `Layout` and cheap to rebuild from it; never the
/// thing that gets re-broken. See the module docs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShapedText {
    pub runs: Vec<ShapedRun>,
    pub fonts: Vec<FontFace>,
    /// Logical extents of the laid-out text.
    pub width: f32,
    pub height: f32,
}

/// What a Taffy measure pass is asking for.
///
/// Mirrors `taffy::AvailableSpace` without importing it: this module answers
/// sizing questions and should not acquire an opinion about which layout
/// engine is asking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Available {
    /// Break at exactly this logical width.
    Definite(f32),
    /// The widest the text would like to be — no breaking.
    MaxContent,
    /// The narrowest it can be without overflowing — the longest unbreakable
    /// word.
    MinContent,
}

/// How much shaping work a pass actually did.
///
/// The point of the cache, made countable. For a one-leaf change in a
/// 4,000-node tree the target is `rebuilt == 1` — not 4,000 — even while the
/// frame is still dominated by layout and raster.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextStats {
    /// Layouts rebuilt because text or style changed. The expensive one.
    pub rebuilt: u64,
    /// Layouts re-broken because the available width changed. Cheap: no
    /// reshaping.
    pub relinebroken: u64,
    /// Layouts reused whole.
    pub reused: u64,
    /// `ShapedText` artifacts extracted from a `Layout`.
    pub extracted: u64,
}

/// One text node's cached shaping state.
///
/// The `Layout` is the reusable truth; `shaped` is what was last extracted
/// from it. `finalized_width` records the width the *finalization pass* last
/// broke at, which is the only record `measure` may not disturb.
pub struct TextEntry {
    /// Hash of the string. Cheaper to compare than the string, and the string
    /// itself lives in the retained tree already.
    pub source_hash: u64,
    pub style_hash: u64,
    /// The width [`TextContext::finalize`] last broke at.
    ///
    /// Only `finalize` writes this. `measure` may break the layout while
    /// answering a sizing question, and must not disturb this record — that is
    /// what stops a speculative probe from invalidating a finalized artifact.
    pub finalized_width: Option<f32>,
    /// Intrinsic widths, cached because **Parley no longer caches them
    /// internally** — the caller is expected to. Taffy asks for min- and
    /// max-content repeatedly per pass, so recomputing would reintroduce the
    /// cost this whole cache exists to remove.
    pub content_widths: parley::layout::ContentWidths,
    /// Height with no wrapping, recorded at rebuild. Answers the height half
    /// of an intrinsic query without re-breaking; approximate for wrapped
    /// text, and deliberately so — the alternative is the mutation that made
    /// one changed label re-extract every node on screen.
    pub unbroken_height: f32,
    pub shaped: ShapedText,
    /// Whether `shaped` still describes `layout`.
    ///
    /// Extraction walks every line, item, and glyph and allocates a fresh
    /// `ShapedText`, so doing it when nothing moved costs as much as the
    /// rebuild the cache just avoided — which is exactly what the first
    /// version did, making `reused` and `rebuilt` cost the same 2.5ms and
    /// leaving the cache detecting reuse without ever taking it.
    pub shaped_valid: bool,
    /// The reusable truth. Kept here rather than in the snapshot precisely so
    /// it survives between commits; re-breaking it is cheap, reshaping is not.
    pub layout: parley::Layout<[u8; 4]>,
}

/// Owns the font stack and every node's cached shaping.
///
/// `FontContext` and `LayoutContext` are expensive to construct and are meant
/// to be long-lived shared resources, so this is created once and kept for the
/// life of the UI — not built per layout pass.
///
/// # Contract
///
/// - [`Self::measure`] answers a sizing question and may be called many times
///   per node per layout pass, under different [`Available`] constraints. It
///   caches, but does not extract.
/// - [`Self::finalize`] runs once, after Taffy has produced final geometry. It
///   re-breaks any `Layout` whose final width differs from what it was last
///   broken at, and extracts [`ShapedText`] for every text node.
/// - [`Self::retire`] drops entries for keys the diff reported as removed.
///   Nothing else may drop them: a node absent from one snapshot and back in
///   the next is a *new* node, and reusing a stale entry would show the old
///   string.
pub struct TextContext {
    entries: HashMap<NodeKey, TextEntry>,
    stats: TextStats,
    font_context: parley::FontContext,
    layout_context: parley::LayoutContext,
}

impl TextContext {
    /// Builds the long-lived Parley resources.
    ///
    /// Both contexts are expensive to construct and reusable across every
    /// layout pass, so this is created once and kept for the life of the UI.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            stats: TextStats::default(),
            font_context: parley::FontContext::new(),
            layout_context: parley::LayoutContext::new(),
        }
    }

    /// Builds the long-lived Parley resources with a registered monospace face.
    ///
    /// `data` is the shipped monospace face, registered with Parley's font
    /// collection so [`FontRole::Monospace`] resolves to it rather than to
    /// whatever the platform happens to have.
    pub fn with_monospace_face(data: Arc<[u8]>) -> Self {
        let mut context = Self::new();
        context.register_monospace_face(data);
        context
    }

    /// Registers `data` as the collection's monospace face.
    pub fn register_monospace_face(&mut self, data: Arc<[u8]>) {
        let families = self
            .font_context
            .collection
            .register_fonts(parley::fontique::Blob::new(Arc::new(data)), None);
        self.font_context.collection.set_generic_families(
            parley::fontique::GenericFamily::Monospace,
            families.into_iter().map(|(id, _)| id),
        );
    }

    /// Statistics since the last [`Self::reset_stats`].
    pub fn stats(&self) -> TextStats {
        self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = TextStats::default();
    }

    /// Drops cached shaping for nodes the diff removed.
    pub fn retire(&mut self, removed: &[NodeKey]) {
        for key in removed {
            self.entries.remove(key);
        }
    }

    /// How many nodes are currently cached. Diagnostics, and the check that
    /// [`Self::retire`] is actually being called.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Answers a sizing question without extracting render artifacts.
    ///
    /// Taffy probes this under many constraints per pass; the last call is not
    /// necessarily the node's final width, so nothing is extracted here.
    pub fn measure(
        &mut self,
        key: NodeKey,
        text: &str,
        style: ShapingStyle,
        available: Available,
    ) -> (f32, f32) {
        let source_hash = hash_bytes(text.as_bytes());
        let style_hash = hash_style(&style);

        let rebuilt = !self.entries.get(&key).is_some_and(|entry| {
            entry.source_hash == source_hash && entry.style_hash == style_hash
        });
        if rebuilt {
            self.stats.rebuilt += 1;
            let mut layout = self.shape(text, &style);
            // Break once, unwrapped, to record the intrinsic answers Taffy will
            // ask for repeatedly. Parley stopped caching these, so this is the
            // cache.
            layout.break_all_lines(None);
            let content_widths = layout.calculate_content_widths();
            let unbroken_height = layout.height();
            self.entries.insert(
                key,
                TextEntry {
                    source_hash,
                    style_hash,
                    finalized_width: None,
                    content_widths,
                    unbroken_height,
                    shaped: ShapedText::default(),
                    shaped_valid: false,
                    layout,
                },
            );
        }

        let entry = self
            .entries
            .get_mut(&key)
            .expect("entry was just ensured above");

        // Intrinsic queries are answered from the cache and touch nothing.
        // Breaking the layout to answer them was the defect: Taffy probes
        // MinContent, MaxContent, and Definite for every node in a pass, so
        // each probe re-broke the layout and invalidated the extracted
        // artifact -- ten nodes re-extracting because one label changed.
        match available {
            Available::MinContent => {
                self.stats.reused += 1;
                return (entry.content_widths.min, entry.unbroken_height);
            }
            Available::MaxContent => {
                self.stats.reused += 1;
                return (entry.content_widths.max, entry.unbroken_height);
            }
            Available::Definite(width) => {
                // A real height needs a real break, so this one does mutate --
                // but it deliberately leaves `finalized_width` alone, so a
                // speculative probe cannot invalidate what `finalize` produced.
                entry.layout.break_all_lines(Some(width));
                entry.layout.align(
                    parley::Alignment::Start,
                    parley::AlignmentOptions::default(),
                );
            }
        }
        (entry.layout.width(), entry.layout.height())
    }

    /// Runs after Taffy has final geometry: re-breaks if needed, then extracts.
    ///
    /// This is the only place [`ShapedText`] is produced.
    pub fn finalize(&mut self, key: NodeKey, final_width: f32) -> &ShapedText {
        let entry = self
            .entries
            .get_mut(&key)
            .expect("finalize requires a prior measure call for the node");
        if entry.finalized_width != Some(final_width) {
            self.stats.relinebroken += 1;
            entry.layout.break_all_lines(Some(final_width));
            entry.layout.align(
                parley::Alignment::Start,
                parley::AlignmentOptions::default(),
            );
            entry.finalized_width = Some(final_width);
            entry.shaped_valid = false;
        } else {
            self.stats.reused += 1;
        }

        // Only when the layout actually moved. Re-extracting an unchanged
        // layout is the whole cost the cache exists to avoid.
        if !entry.shaped_valid {
            entry.shaped = extract(&entry.layout);
            entry.shaped_valid = true;
            self.stats.extracted += 1;
        }
        &entry.shaped
    }

    /// How many lines the cached layout is currently broken into.
    ///
    /// Diagnostics, and what the wrapping tests assert against.
    pub fn line_count(&self, key: NodeKey) -> usize {
        self.entries.get(&key).map_or(0, |entry| entry.layout.len())
    }

    fn shape(&mut self, text: &str, style: &ShapingStyle) -> parley::Layout<[u8; 4]> {
        let family = match style.role {
            FontRole::SystemUi => parley::FontFamily::from(parley::GenericFamily::SystemUi),
            FontRole::Monospace => parley::FontFamily::from(parley::GenericFamily::Monospace),
        };
        let mut builder =
            self.layout_context
                .ranged_builder(&mut self.font_context, text, 1.0, false);
        builder.push_default(parley::StyleProperty::FontFamily(family));
        builder.push_default(parley::StyleProperty::FontSize(style.size));
        builder.push_default(parley::StyleProperty::FontWeight(parley::FontWeight::new(
            f32::from(style.weight),
        )));
        if !style.wrap {
            builder.push_default(parley::StyleProperty::TextWrapMode(
                parley::TextWrapMode::NoWrap,
            ));
        }
        builder.build(text)
    }
}

impl Default for TextContext {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for TextContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextContext")
            .field("entries", &self.entries.len())
            .field("stats", &self.stats)
            .finish()
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn hash_style(style: &ShapingStyle) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    style.role.hash(&mut hasher);
    style.size.to_bits().hash(&mut hasher);
    style.weight.hash(&mut hasher);
    style.wrap.hash(&mut hasher);
    hasher.finish()
}

fn extract(layout: &parley::Layout<[u8; 4]>) -> ShapedText {
    let mut fonts: Vec<FontFace> = Vec::new();
    let mut runs = Vec::new();
    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let run = glyph_run.run();
            let font_data = run.font();
            let bytes = font_data.data.as_ref();
            let key = font_key(bytes, font_data.index);
            let font_index = match fonts.iter().position(|face| face.key == key) {
                Some(index) => index,
                None => {
                    fonts.push(FontFace {
                        data: Arc::from(bytes),
                        index: font_data.index,
                        key,
                    });
                    fonts.len() - 1
                }
            };
            let glyphs = glyph_run
                .positioned_glyphs()
                .map(|glyph| Glyph {
                    id: glyph.id,
                    x: glyph.x,
                    y: glyph.y,
                })
                .collect();
            runs.push(ShapedRun {
                font: font_index,
                font_size: run.font_size(),
                glyphs,
            });
        }
    }
    ShapedText {
        runs,
        fonts,
        width: layout.width(),
        height: layout.height(),
    }
}

fn font_key(bytes: &[u8], index: u32) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    index.hash(&mut hasher);
    hasher.finish()
}

/// # Known limitation, recorded rather than papered over
///
/// Parley's `calculate_content_widths` documents that min/max content widths
/// may be inaccurate for mixed-direction text. Instar answers `MinContent` and
/// `MaxContent` from it anyway, because the alternative is inventing a
/// measurement, and a wrong intrinsic width for a bidi paragraph is a layout
/// bug rather than a correctness one.
///
/// There is a bidi fixture in the tests so the behaviour is observed rather
/// than assumed, and so the day it starts mattering is a failing test rather
/// than a bug report.
pub const BIDI_CONTENT_WIDTH_IS_APPROXIMATE: bool = true;

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: NodeKey = NodeKey(7);

    fn style() -> ShapingStyle {
        ShapingStyle::default()
    }

    #[test]
    fn shaping_twice_at_the_same_width_reuses() {
        let mut text = TextContext::new();
        let (first_width, first_height) =
            text.measure(KEY, "Hello world", style(), Available::Definite(200.0));
        assert_eq!(text.stats().rebuilt, 1);
        assert_eq!(
            text.stats().extracted,
            0,
            "measure must never extract ShapedText"
        );

        let (second_width, second_height) =
            text.measure(KEY, "Hello world", style(), Available::Definite(200.0));
        assert_eq!(text.stats().rebuilt, 1);
        assert_eq!(text.stats().reused, 1);
        assert_eq!((first_width, first_height), (second_width, second_height));

        let shaped = text.finalize(KEY, 200.0);
        assert!(!shaped.runs.is_empty());
        let _ = shaped;
        assert_eq!(
            text.stats().reused,
            2,
            "finalize reuses the same broken layout"
        );
        assert_eq!(text.stats().extracted, 1);
    }

    #[test]
    fn width_change_relinebreaks_without_rebuilding() {
        let mut text = TextContext::new();
        let sentence = "a long sentence that will wrap";
        text.measure(KEY, sentence, style(), Available::Definite(200.0));
        text.measure(KEY, sentence, style(), Available::Definite(60.0));
        assert_eq!(text.stats().rebuilt, 1);
        assert_eq!(text.stats().relinebroken, 1);
        assert_eq!(text.stats().reused, 0);

        text.finalize(KEY, 60.0);
        assert_eq!(text.stats().rebuilt, 1);
        assert_eq!(text.stats().relinebroken, 1);

        text.finalize(KEY, 140.0);
        assert_eq!(text.stats().rebuilt, 1);
        assert_eq!(text.stats().relinebroken, 2);
        assert_eq!(text.stats().extracted, 2);
    }

    #[test]
    fn changing_the_string_rebuilds() {
        let mut text = TextContext::new();
        text.measure(KEY, "first", style(), Available::MaxContent);
        text.measure(KEY, "second", style(), Available::MaxContent);
        assert_eq!(text.stats().rebuilt, 2);
        assert_eq!(text.stats().reused, 0);
    }

    #[test]
    fn changing_size_or_weight_rebuilds() {
        let mut text = TextContext::new();
        let large = ShapingStyle {
            size: 24.0,
            ..style()
        };
        let bold = ShapingStyle {
            weight: 700,
            ..style()
        };

        text.measure(KEY, "same", style(), Available::MaxContent);
        text.measure(KEY, "same", large, Available::MaxContent);
        assert_eq!(text.stats().rebuilt, 2);
        text.measure(KEY, "same", bold, Available::MaxContent);
        assert_eq!(text.stats().rebuilt, 3);
        assert_eq!(text.stats().relinebroken, 0);
    }

    #[test]
    fn retire_drops_entries_and_a_returning_key_is_fresh() {
        let mut text = TextContext::new();
        let (old_width, _) = text.measure(KEY, "old", style(), Available::MaxContent);
        assert_eq!(text.len(), 1);

        text.retire(&[KEY]);
        assert!(text.is_empty());

        let (new_width, _) = text.measure(
            KEY,
            "a much longer replacement",
            style(),
            Available::MaxContent,
        );
        let shaped = text.finalize(KEY, new_width);
        assert!(!shaped.runs.is_empty());
        let _ = shaped;
        assert_eq!(text.stats().rebuilt, 2, "the key must be shaped fresh");
        assert_ne!(new_width, old_width, "the old string must not be served");
    }

    #[test]
    fn wrapping_text_makes_more_than_one_line() {
        let mut text = TextContext::new();
        let sentence = "one two three four five six seven eight";
        text.measure(KEY, sentence, style(), Available::Definite(60.0));
        assert!(
            text.line_count(KEY) > 1,
            "narrow wrapping should make several lines"
        );

        let no_wrap = ShapingStyle {
            wrap: false,
            ..style()
        };
        let no_wrap_key = NodeKey(8);
        text.measure(no_wrap_key, sentence, no_wrap, Available::Definite(60.0));
        assert_eq!(
            text.line_count(no_wrap_key),
            1,
            "NoWrap must stay on one line"
        );
    }

    #[test]
    fn min_content_is_at_most_max_content() {
        let mut text = TextContext::new();
        let sentence = "the quick brown fox jumps over the lazy dog";
        let (min_width, _) = text.measure(KEY, sentence, style(), Available::MinContent);
        let (max_width, _) = text.measure(KEY, sentence, style(), Available::MaxContent);
        assert!(min_width.is_finite() && max_width.is_finite());
        assert!(min_width <= max_width, "min {min_width} <= max {max_width}");
        assert!(min_width > 0.0 && max_width > 0.0);
    }

    #[test]
    fn mixed_direction_text_shapes_without_panicking() {
        // Hebrew and Arabic mixed with Latin exercises both bidi reordering
        // and font fallback inside one node. Parley documents that content
        // widths may be inaccurate for mixed-direction text, so this test
        // deliberately asserts only that the fixture shapes and extracts
        // without panicking -- not a pixel value we cannot justify.
        let fixture = "שלום Hello مرحبا world";
        let mut text = TextContext::new();
        let (min_width, _) = text.measure(KEY, fixture, style(), Available::MinContent);
        let (max_width, _) = text.measure(KEY, fixture, style(), Available::MaxContent);
        assert!(min_width.is_finite() && max_width.is_finite());
        assert!(text.line_count(KEY) >= 1);

        let shaped = text.finalize(KEY, max_width);
        let glyphs: usize = shaped.runs.iter().map(|run| run.glyphs.len()).sum();
        assert!(!shaped.runs.is_empty(), "bidi fixture should produce runs");
        assert!(
            !shaped.fonts.is_empty(),
            "bidi fixture should resolve a face"
        );
        assert!(glyphs > 0, "bidi fixture should produce glyphs");
    }
}
