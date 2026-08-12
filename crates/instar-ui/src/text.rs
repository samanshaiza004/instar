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
//!
//! # What `measure` may and may not do
//!
//! > `measure` may perform temporary work needed to answer the current sizing
//! > query, including line-breaking for `Definite(width)`, but it must not
//! > mutate finalized presentation state or invalidate reusable artifacts
//! > based on speculative constraints.
//!
//! ```text
//! MinContent / MaxContent  intrinsic query only; cached ContentWidths;
//!                          no line-break mutation
//! Definite(width)          may line-break temporarily to compute height;
//!                          must not update finalized_width;
//!                          must not invalidate the shaped artifact
//! finalize(actual_width)   owns finalized_width, persistent line-break
//!                          state, and ShapedText extraction
//! ```
//!
//! The invariant is not "measure never mutates" — a real height needs a real
//! break — but that **speculative probes cannot poison the finalized cache**.

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
/// Where a line sits within the width it was broken to.
///
/// Mirrors [`instar_ui_protocol::WireTextAlign`] the way [`ShapingStyle`]
/// mirrors the wire's shaping group: the wire is the wire, and this layer
/// keeps its own vocabulary so the two can move independently.
///
/// Deliberately **not** part of `ShapingStyle`. Parley applies alignment after
/// line breaking and can re-apply it to an existing layout, so changing it
/// costs a realign and a re-extract -- no reshape, no new break, no layout
/// pass. Grouping it with role, size and weight would have made every
/// alignment change pay for all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    #[default]
    Start,
    Center,
    End,
}

impl From<instar_ui_protocol::WireTextAlign> for Alignment {
    fn from(wire: instar_ui_protocol::WireTextAlign) -> Self {
        match wire {
            instar_ui_protocol::WireTextAlign::Start => Self::Start,
            instar_ui_protocol::WireTextAlign::Center => Self::Center,
            instar_ui_protocol::WireTextAlign::End => Self::End,
        }
    }
}

impl From<Alignment> for parley::Alignment {
    fn from(alignment: Alignment) -> Self {
        match alignment {
            // Direction-aware, both of them: Parley resolves Start and End
            // against the text's own direction, which is why the vocabulary
            // says these rather than Left and Right.
            Alignment::Start => Self::Start,
            Alignment::Center => Self::Center,
            Alignment::End => Self::End,
        }
    }
}

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
#[derive(Clone, PartialEq)]
pub struct FontFace {
    pub data: std::sync::Arc<[u8]>,
    pub index: u32,
    /// Stable identity for renderer-side caches.
    ///
    /// Derived from the font blob's own id, so it changes when the bytes do —
    /// and, unlike a content hash, may also differ between two blobs holding
    /// identical bytes. That is the right trade: the same face loaded twice is
    /// a rare and harmless cache miss, while hashing the file to rule it out
    /// cost 2.5 ms per extraction with a system face.
    pub key: u64,
}

/// Hand-written so a failing assertion is readable.
///
/// The derived `Debug` prints `data` byte by byte. A `ShapedText` appears
/// inside `LayoutSnapshot`, so one failed layout comparison produced a 182 MB
/// dump of two font files rendered as decimal integers — which is not a
/// diagnostic, it is a denial of one.
impl std::fmt::Debug for FontFace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontFace")
            .field("key", &self.key)
            .field("index", &self.index)
            .field("bytes", &self.data.len())
            .finish()
    }
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

/// A shaped layout whose lifetime the caller owns.
///
/// # Why this exists rather than returning `ShapedText`
///
/// `ShapedText` is a *render artifact* — glyph ids and positions, derived from
/// a layout and cheap to rebuild from it. The `Layout` is the reusable truth:
/// it is what can be re-broken at a new width, re-aligned, and hit-tested.
/// That split is the Stage 1 fix this module is built around, and handing a
/// caller only the derived half would push them into reshaping to answer a
/// question the layout could have answered.
///
/// Concretely, `parley::editing::Cursor` operates against a `Layout`. A
/// `TextView` that received only glyph positions would discover at the first
/// mouse click that it cannot hit-test, and would either shape a second time
/// or grow a parallel layout path.
///
/// # Why it is opaque
///
/// The other way to avoid that is to return `parley::Layout` directly, and
/// then `instar-host` depends on Parley's types and "`instar-ui` owns text
/// shaping" stops being true. The wrapper is the middle: callers get the
/// reusable truth, Parley stays inside this crate, and the editing geometry
/// operations B2 needs can be added here without moving the seam.
///
/// # What this is, and what it must never become
///
/// > A `TextLayout` is **presentation state, not buffer state**. It may be
/// > reused while the revision, byte range, style and layout constraints it
/// > was built from remain valid. It never becomes canonical text truth.
///
/// Written down before caching exists, because caching is when it gets
/// violated. A layout that outlives the range it was shaped from is a second
/// answer to what the document says, and the first symptom is a caret placed
/// from stale geometry — the same shape of defect as two answers to where a
/// node is on screen, which Phase 2 already paid for once.
///
/// The corollary for B2: hit-testing and caret geometry must use the *same
/// instance* that produced the presented glyphs. Re-shaping the same string to
/// answer a click is how a view acquires two layouts that disagree.
pub struct TextLayout {
    layout: parley::Layout<[u8; 4]>,
    shaped: ShapedText,
    shaped_valid: bool,
}

/// Which side of a boundary a caret is attached to.
///
/// Not decoration. The same byte offset sits in two visually different places
/// at a bidi boundary and at a soft line break, and Parley resolves that with
/// affinity — so a position reduced to a bare byte offset is a position that
/// cannot be drawn correctly. It travels with the offset everywhere, including
/// across the window-origin translation.
///
/// Instar's own enum rather than Parley's, for the same reason `TextLayout`
/// exists: `instar-host` should not name Parley types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Affinity {
    /// Attached to the character logically after the offset.
    #[default]
    Downstream,
    /// Attached to the character logically before it.
    Upstream,
}

impl From<Affinity> for parley::Affinity {
    fn from(affinity: Affinity) -> Self {
        match affinity {
            Affinity::Downstream => Self::Downstream,
            Affinity::Upstream => Self::Upstream,
        }
    }
}

impl From<parley::Affinity> for Affinity {
    fn from(affinity: parley::Affinity) -> Self {
        match affinity {
            parley::Affinity::Downstream => Self::Downstream,
            parley::Affinity::Upstream => Self::Upstream,
        }
    }
}

/// A caret position within one layout's text.
///
/// The byte offset is **layout-local**: it indexes the string the layout was
/// built from, not a document. Turning it into a document position is
/// `instar-host`'s job and the reason `PresentedSegment` remembers where its
/// text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextCursor {
    pub index: usize,
    pub affinity: Affinity,
}

/// Where a caret should be drawn, in the layout's own logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaretGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl TextLayout {
    /// The caret position a point falls at.
    ///
    /// Coordinates are relative to this layout's own origin, so the caller
    /// subtracts the segment's origin before asking. Parley resolves the
    /// cluster, the line, and the writing direction; Instar does not
    /// reimplement any of that.
    pub fn cursor_from_point(&self, x: f32, y: f32) -> TextCursor {
        let cursor = parley::Cursor::from_point(&self.layout, x, y);
        TextCursor {
            index: cursor.index(),
            affinity: cursor.affinity().into(),
        }
    }

    /// The caret at a layout-local byte offset.
    pub fn cursor_from_byte_index(&self, index: usize, affinity: Affinity) -> TextCursor {
        let cursor = parley::Cursor::from_byte_index(&self.layout, index, affinity.into());
        TextCursor {
            index: cursor.index(),
            affinity: cursor.affinity().into(),
        }
    }

    /// Where to draw a caret, in this layout's logical coordinates.
    pub fn caret_geometry(&self, cursor: TextCursor, width: f32) -> CaretGeometry {
        let box_ =
            parley::Cursor::from_byte_index(&self.layout, cursor.index, cursor.affinity.into())
                .geometry(&self.layout, width);
        CaretGeometry {
            x: box_.x0 as f32,
            y: box_.y0 as f32,
            width: (box_.x1 - box_.x0) as f32,
            height: (box_.y1 - box_.y0) as f32,
        }
    }

    /// The rectangles a selection between two layout-local cursors covers.
    ///
    /// A callback rather than a returned `Vec`: Parley offers both, and the
    /// allocating form builds a temporary vector per segment per frame. With
    /// twenty-three segments visible that is twenty-three allocations to paint
    /// one highlight.
    ///
    /// Both cursors are **layout-local**. Projecting a document-wide selection
    /// onto one segment is `instar-host`'s job, because a Parley `Selection`
    /// lives inside a single layout and this editor shapes one per row — a
    /// selection stored as a Parley `Selection` would break the moment a drag
    /// crossed a row boundary.
    pub fn selection_geometry_with(
        &self,
        anchor: TextCursor,
        focus: TextCursor,
        mut f: impl FnMut(CaretGeometry),
    ) {
        let selection = parley::Selection::new(
            parley::Cursor::from_byte_index(&self.layout, anchor.index, anchor.affinity.into()),
            parley::Cursor::from_byte_index(&self.layout, focus.index, focus.affinity.into()),
        );
        selection.geometry_with(&self.layout, |box_, _line| {
            f(CaretGeometry {
                x: box_.x0 as f32,
                y: box_.y0 as f32,
                width: (box_.x1 - box_.x0) as f32,
                height: (box_.y1 - box_.y0) as f32,
            });
        });
    }

    /// Breaks the text into lines at `width`, or unbroken when `None`.
    pub fn break_lines(&mut self, width: Option<f32>) {
        self.layout.break_all_lines(width);
        self.shaped_valid = false;
    }

    /// Positions the broken lines. Must follow a break; Parley aligns the
    /// lines that exist.
    pub fn align(&mut self, alignment: Alignment) {
        self.layout
            .align(alignment.into(), parley::AlignmentOptions::default());
        self.shaped_valid = false;
    }

    /// The render artifact, extracted once per layout change.
    ///
    /// `&mut self` for a getter is deliberate: extraction is real work, and
    /// this module's whole cache exists because re-extracting an unchanged
    /// layout was the cost worth removing. A `&self` version returning by
    /// value would reintroduce it once per visible paragraph per frame.
    pub fn shaped(&mut self) -> &ShapedText {
        if !self.shaped_valid {
            self.shaped = extract(&self.layout);
            self.shaped_valid = true;
        }
        &self.shaped
    }

    pub fn width(&self) -> f32 {
        self.layout.width()
    }

    pub fn height(&self) -> f32 {
        self.layout.height()
    }

    pub fn line_count(&self) -> usize {
        self.layout.len()
    }
}

impl std::fmt::Debug for TextLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextLayout")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("lines", &self.line_count())
            .finish_non_exhaustive()
    }
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
/// Each counter names the work of *one call*, so which call does the counting
/// matters and is stated here — an earlier reading of these drifted out of
/// step with the code and took three tests with it.
pub struct TextStats {
    /// Layouts rebuilt because text or style changed. The expensive one, and
    /// the only one that reshapes.
    pub rebuilt: u64,
    /// **Finalization** re-broke a layout because the final width moved.
    /// Cheap: no reshaping.
    ///
    /// A speculative break inside [`TextContext::measure`] is deliberately not
    /// counted. It is not finalization — it must leave `finalized_width` and
    /// the shaped artifact alone — and counting it here would make the two
    /// indistinguishable in a trace, which is the confusion this whole split
    /// exists to remove.
    pub relinebroken: u64,
    /// `Layout::align` was invoked because alignment needed applying.
    ///
    /// Deliberately "work done", not "the alignment property changed". A width
    /// change re-breaks the lines and therefore *also* has to re-apply
    /// alignment, and a counter that only tracked the wire diff would report
    /// zero for real work — which would make it a description of the protocol
    /// rather than of the machine.
    pub realigned: u64,
    /// A query answered without touching the layout: an intrinsic
    /// (`MinContent`/`MaxContent`) measure served from the per-entry cache, or
    /// a finalize at a width the layout is already broken at.
    ///
    /// Counted per call, not per node, so a rebuild followed by a cached
    /// intrinsic answer in the same `measure` records both.
    pub reused: u64,
    /// `ShapedText` artifacts extracted from a `Layout`. Finalization only.
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
    /// The alignment the finalized layout was last positioned under.
    ///
    /// Separate from the shaping identity above, because it is not part of it:
    /// Parley applies alignment after line breaking and can re-apply it to an
    /// existing layout, so a change here needs neither a reshape nor a new
    /// break.
    pub finalized_alignment: Alignment,
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

/// Rounds a measured width up to a whole logical pixel.
///
/// Taffy rounds computed layout to integers. A node sized to its own text
/// therefore lands a fraction of a pixel *narrower* than the text it was
/// measured from -- and `finalize` then re-breaks the text to that rounded
/// width, wrapping a label that fits. Whether it happened depended on which
/// way the fraction fell, so the same string wrapped or did not according to
/// the font, the size, and the letters in it.
///
/// Reporting the ceiling closes it at the source: a box sized from a
/// measurement is never smaller than what was measured. A node that is
/// genuinely constrained still wraps, because that width comes from its
/// parent and not from here.
fn ceil(width: f32) -> f32 {
    width.ceil()
}

thread_local! {
    /// Font stacks built on this thread.
    ///
    /// Parley intends a `FontContext` to be roughly one per application, and a
    /// fresh one enumerates the system's faces. "Long-lived" is the kind of
    /// intention that decays into a per-frame construction nobody notices,
    /// because the only symptom is that everything is slow — so it is counted,
    /// and a test asserts the count does not move across a frame.
    ///
    /// Thread-local rather than global because the test harness runs tests in
    /// parallel on separate threads, and a process-wide counter would report
    /// another test's context.
    static CONSTRUCTED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl TextContext {
    /// Builds the long-lived Parley resources.
    ///
    /// Both contexts are expensive to construct and reusable across every
    /// layout pass, so this is created once and kept for the life of the UI.
    pub fn new() -> Self {
        CONSTRUCTED.with(|count| count.set(count.get() + 1));
        Self {
            entries: HashMap::new(),
            stats: TextStats::default(),
            font_context: parley::FontContext::new(),
            layout_context: parley::LayoutContext::new(),
        }
    }

    /// How many font stacks this thread has built.
    ///
    /// Diagnostic, in the same spirit as [`TextStats`]: a number that makes an
    /// intention checkable instead of aspirational.
    pub fn constructed_on_this_thread() -> u64 {
        CONSTRUCTED.with(|count| count.get())
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
                    finalized_alignment: Alignment::default(),
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
                return (ceil(entry.content_widths.min), entry.unbroken_height.ceil());
            }
            Available::MaxContent => {
                self.stats.reused += 1;
                return (ceil(entry.content_widths.max), entry.unbroken_height.ceil());
            }
            Available::Definite(width) => {
                // A real height needs a real break, so this one does mutate --
                // but it deliberately leaves `finalized_width` alone, so a
                // speculative probe cannot invalidate what `finalize` produced.
                // Broken, never aligned. This probe is speculative -- Taffy
                // asks repeatedly and the last answer is not necessarily the
                // winning one -- and alignment is finalization's job. Applying
                // it here would recreate exactly the class of mutation bug
                // that cost 9x the latency in stage 1.
                entry.layout.break_all_lines(Some(width));
            }
        }
        (ceil(entry.layout.width()), entry.layout.height().ceil())
    }

    /// Runs after Taffy has final geometry: re-breaks if needed, aligns if
    /// needed, then extracts.
    ///
    /// This is the only place [`ShapedText`] is produced, and the only place
    /// alignment is ever applied.
    ///
    /// The three reasons to do work are separate, and so are their costs:
    ///
    /// ```text
    /// width moved       re-break, then re-align, then extract
    /// alignment moved   re-align, then extract
    /// neither           reuse
    /// ```
    ///
    /// A re-break always forces a re-align: the lines it produced have not
    /// been positioned yet, and Parley's alignment is applied to the lines
    /// that exist.
    pub fn finalize(
        &mut self,
        key: NodeKey,
        final_width: f32,
        alignment: Alignment,
    ) -> &ShapedText {
        let entry = self
            .entries
            .get_mut(&key)
            .expect("finalize requires a prior measure call for the node");

        let rebroke = entry.finalized_width != Some(final_width);
        if rebroke {
            self.stats.relinebroken += 1;
            entry.layout.break_all_lines(Some(final_width));
            entry.finalized_width = Some(final_width);
        }

        // Decided before anything is mutated. Testing `finalized_alignment`
        // after assigning it reports reuse for work that was just done, which
        // is a counter describing its own side effect.
        let realign = rebroke || entry.finalized_alignment != alignment;
        if realign {
            self.stats.realigned += 1;
            // `align_when_overflowing` stays at Parley's default, under which
            // an overflowing End or Center line falls back to Start. Whether
            // Instar wants that is a text-overflow question -- it sits beside
            // clipping, ellipsis and scrolling -- and answering it with an
            // alignment flag would be answering the wrong question. Recorded
            // as unresolved rather than guessed at.
            entry
                .layout
                .align(alignment.into(), parley::AlignmentOptions::default());
            entry.finalized_alignment = alignment;
            entry.shaped_valid = false;
        }

        if !realign {
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

    /// Shapes text this context will not cache, for a caller that owns the
    /// result.
    ///
    /// The keyed path exists because the semantic tree gives every text node a
    /// stable `NodeKey` to cache against. A `TextView` has no such key: it
    /// shapes a *window* that moves as the view scrolls, so the caller decides
    /// what to keep and for how long.
    ///
    /// What is shared is the part that must be: the `FontContext` and
    /// `LayoutContext`. Parley intends those to be long-lived, roughly one per
    /// application, and a second set would load every face twice — and could
    /// resolve the same family to a different face for a `Text` node than for
    /// a `TextView` showing the same font.
    ///
    /// This is the whole of `instar-ui`'s knowledge of editors: it shapes a
    /// string. It does not know a `TextBuffer` exists.
    pub fn shape_keyless(&mut self, text: &str, style: ShapingStyle) -> TextLayout {
        TextLayout {
            layout: self.shape(text, &style),
            shaped: ShapedText::default(),
            shaped_valid: false,
        }
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
            let face = cached_face(font_data);
            let font_index = match fonts.iter().position(|held| held.key == face.key) {
                Some(index) => index,
                None => {
                    fonts.push(FontFace {
                        data: face.data,
                        index: font_data.index,
                        key: face.key,
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

/// A face's key and bytes, computed once per face instead of once per
/// extraction.
///
/// # What was wrong
///
/// Extraction did two things per call that both read the whole font file: it
/// hashed the bytes to produce a cache key, and it built `FontFace::data` with
/// `Arc::from(bytes)` — a copy, because Parley hands out a `Blob<u8>` whose
/// inner `Arc` is a different type. `textbench` measured the result:
///
/// ```text
/// one 61-glyph line     shipped monospace      a system face
/// extract              62 µs / 184,776 B    2,530 µs / 7,910,720 B
/// ```
///
/// # Why memoized rather than replaced
///
/// `Blob` carries a unique id, and keying on that alone would make both costs
/// vanish — but it would also make `FontFace::key` non-deterministic, because
/// two `TextContext`s each load the face into their own blob. `instar-ui`
/// asserts that layout is deterministic, and it caught exactly that.
///
/// So the key stays content-derived and the *lookup* becomes free: the blob id
/// indexes a cache holding the hash and the one copy of the bytes. The
/// expensive read happens once per face per thread.
///
/// The bytes are not eliminated entirely because the alternative is putting a
/// `Blob` into `FontFace` and therefore into `instar-paint::FontResource`,
/// which would push a font-library type into the renderer-neutral paint crate
/// to save a copy that now happens once.
#[derive(Clone)]
struct CachedFace {
    key: u64,
    data: Arc<[u8]>,
}

thread_local! {
    static FACES: std::cell::RefCell<HashMap<(u64, u32), CachedFace>> =
        std::cell::RefCell::new(HashMap::new());
}

fn cached_face(font: &parley::FontData) -> CachedFace {
    FACES.with(|cache| {
        cache
            .borrow_mut()
            .entry((font.data.id(), font.index))
            .or_insert_with(|| {
                let bytes = font.data.data();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut hasher);
                font.index.hash(&mut hasher);
                CachedFace {
                    key: hasher.finish(),
                    data: Arc::from(bytes),
                }
            })
            .clone()
    })
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

    const KEY: NodeKey = NodeKey::first(7);

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
        assert_eq!(
            text.stats().rebuilt,
            1,
            "the same string and style must not reshape"
        );
        assert_eq!((first_width, first_height), (second_width, second_height));

        let shaped = text.finalize(KEY, 200.0, Alignment::Start);
        assert!(!shaped.runs.is_empty());
        assert_eq!(
            text.stats().relinebroken,
            1,
            "the first finalize owns the persistent break, whatever measure \
             did speculatively"
        );
        assert_eq!(text.stats().extracted, 1);

        text.finalize(KEY, 200.0, Alignment::Start);
        assert_eq!(
            (text.stats().reused, text.stats().relinebroken),
            (1, 1),
            "finalizing again at the same width reuses the broken layout"
        );
        assert_eq!(
            text.stats().extracted,
            1,
            "and a reused layout must not be re-extracted -- doing so cost as \
             much as the rebuild the cache just avoided"
        );
    }

    #[test]
    fn width_change_relinebreaks_without_rebuilding() {
        let mut text = TextContext::new();
        let sentence = "a long sentence that will wrap";
        text.measure(KEY, sentence, style(), Available::Definite(200.0));
        text.measure(KEY, sentence, style(), Available::Definite(60.0));
        assert_eq!(
            text.stats().rebuilt,
            1,
            "a width change re-breaks; it never reshapes"
        );
        assert_eq!(
            (text.stats().relinebroken, text.stats().extracted),
            (0, 0),
            "measure's speculative breaks are not finalization: they touch \
             neither the persistent break record nor the shaped artifact"
        );

        text.finalize(KEY, 60.0, Alignment::Start);
        assert_eq!((text.stats().rebuilt, text.stats().relinebroken), (1, 1));

        text.finalize(KEY, 140.0, Alignment::Start);
        assert_eq!((text.stats().rebuilt, text.stats().relinebroken), (1, 2));
        assert_eq!(text.stats().extracted, 2, "each real break re-extracts");
    }

    #[test]
    fn changing_the_string_rebuilds() {
        let mut text = TextContext::new();
        text.measure(KEY, "first", style(), Available::MaxContent);
        text.measure(KEY, "second", style(), Available::MaxContent);
        assert_eq!(text.stats().rebuilt, 2);
        assert_eq!(
            text.stats().reused,
            2,
            "both intrinsic answers came from the per-entry cache -- `reused` \
             counts queries served without touching the layout, so a rebuild \
             and a cached answer for the same call are both recorded"
        );
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
        let shaped = text.finalize(KEY, new_width, Alignment::Start);
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
        let no_wrap_key = NodeKey::first(8);
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

        let shaped = text.finalize(KEY, max_width, Alignment::Start);
        let glyphs: usize = shaped.runs.iter().map(|run| run.glyphs.len()).sum();
        assert!(!shaped.runs.is_empty(), "bidi fixture should produce runs");
        assert!(
            !shaped.fonts.is_empty(),
            "bidi fixture should resolve a face"
        );
        assert!(glyphs > 0, "bidi fixture should produce glyphs");
    }

    /// A face built from arbitrary bytes. `cached_face` hashes and copies; it
    /// never parses, so these need not be real fonts.
    fn face(bytes: &[u8], index: u32) -> parley::FontData {
        let data: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(bytes.to_vec());
        parley::FontData::new(parley::fontique::Blob::new(data), index)
    }

    /// The regression that broke `layout_is_deterministic`.
    ///
    /// `Blob::new` hands out a fresh id every call, so two `TextContext`s
    /// loading the same file hold blobs with different ids. A key derived from
    /// the id alone therefore made one font produce two identities, and two
    /// identical layouts compare unequal.
    #[test]
    fn the_same_bytes_and_index_are_the_same_face_in_any_context() {
        let first = cached_face(&face(b"a plausible font file", 0));
        let second = cached_face(&face(b"a plausible font file", 0));

        assert_ne!(
            face(b"a plausible font file", 0).data.id(),
            face(b"a plausible font file", 0).data.id(),
            "the premise: blob ids differ even for identical bytes"
        );
        assert_eq!(
            first.key, second.key,
            "and the key must not, or layout stops being deterministic"
        );
    }

    /// A collection index selects a different face inside one file, so it is
    /// part of the identity rather than a detail hanging off it.
    #[test]
    fn a_collection_index_is_part_of_a_face_key() {
        let bytes = b"a font collection with several faces";
        assert_ne!(
            cached_face(&face(bytes, 0)).key,
            cached_face(&face(bytes, 1)).key,
            "two faces of one TTC would otherwise share a renderer cache entry"
        );
    }

    #[test]
    fn different_bytes_are_different_faces() {
        assert_ne!(
            cached_face(&face(b"one font", 0)).key,
            cached_face(&face(b"another font", 0)).key
        );
    }

    /// The other half of the defect: the bytes are copied once per face, not
    /// once per extraction.
    #[test]
    fn a_faces_bytes_are_copied_once_and_then_shared() {
        let font = face(b"a plausible font file", 0);
        let first = cached_face(&font);
        let second = cached_face(&font);

        assert!(
            Arc::ptr_eq(&first.data, &second.data),
            "a second extraction of the same face copied the font again -- this \
             cost 7.9 MB per extraction with a system face"
        );
    }

    /// Never dump the backing font bytes again.
    #[test]
    fn a_font_face_debugs_as_a_summary_rather_than_a_font() {
        let rendered = format!(
            "{:?}",
            FontFace {
                data: Arc::from(&b"pretend this is two megabytes"[..]),
                index: 3,
                key: 42,
            }
        );

        assert_eq!(rendered, "FontFace { key: 42, index: 3, bytes: 29 }");
        assert!(
            !rendered.contains("112"),
            "the derived Debug printed every byte as a decimal integer, which \
             turned one failed layout comparison into a 182 MB dump: {rendered}"
        );
    }
}
