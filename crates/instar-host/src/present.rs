//! Presentation: what the window is showing, and the paint intent for it (WP7B2).
//!
//! ```text
//! accepted UiCommit -> layout -> lower to PaintScene -> request redraw
//! RedrawRequested   -> Ready? present the scene : keep it pending
//! GuestTrapped      -> PresentationState::Crashed -> request redraw
//! ```
//!
//! # Lowering happens on commit, not on redraw
//!
//! A frame callback is the worst place to discover work. Lowering at commit
//! time means the redraw path is "hand the backend a scene that already
//! exists", and it means anything that could still refuse a batch has already
//! happened by the time the host promises a frame.
//!
//! # The crash screen is the host's, and is not a UI tree
//!
//! When a guest traps there is no guest left to describe anything, so
//! [`crash_scene`] emits paint commands directly. The tempting shortcut —
//! synthesizing an Instar tree that says "the app crashed" and pushing it
//! through the normal path — is rejected: it would mean the host can author
//! interfaces in the guest's name, and every downstream consumer (hit-testing,
//! the commit log, anything that later asks "what did the guest commit?")
//! would be told a lie by a layer that is supposed to be transcribing. The
//! retained tree keeps saying whatever the guest last said. What the window
//! shows is a separate question, and this module is where it is answered.
//!
//! # Physical here, logical above
//!
//! Everything in this module is in physical pixels: it is the far side of the
//! boundary `docs/PHASE-1.md` draws, where `instar-host` converts the logical
//! geometry `instar-ui` produced into the physical target a renderer wants.
//! `instar-ui` never sees a scale factor, and nothing here feeds back into it.

use std::sync::Arc;

use instar_kernel::runtime::GenerationId;
use instar_paint::{
    AffineTransform, Color, FontId, FontResource, GlyphPosition, GlyphRun, PaintCommand,
    PaintScene, PhysicalSize, Rect,
};
use instar_ui::{LayoutSnapshot, Node, NodeKind, TEXT_METRICS, Tree};
use instar_window::WindowMetricsChanged;

/// What the window is showing.
///
/// Not a property of the guest's interface but of the host's willingness to
/// show it, which is why a trap changes this and does not touch the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PresentationState {
    /// Whatever the guest last committed.
    #[default]
    App,
    /// The guest generation died. `message` is the host's account of it, and
    /// `generation` says which one — a later generation's crash screen must
    /// not be mistaken for a stale one still on screen.
    Crashed {
        generation: GenerationId,
        message: String,
    },
}

impl PresentationState {
    pub fn is_crashed(&self) -> bool {
        matches!(self, Self::Crashed { .. })
    }
}

/// Colors. Host-owned, like geometry: a Phase 1 guest describes structure and
/// text, never appearance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub background: Color,
    pub text: Color,
    pub button_face: Color,
    pub button_border: Color,
    pub button_label: Color,
    /// A disabled button is drawn differently rather than hidden: the host
    /// already refuses to activate it, and a control that vanishes when it
    /// stops working is worse than one that looks unavailable.
    pub disabled_face: Color,
    pub disabled_label: Color,
    pub pressed_face: Color,
    pub crash_background: Color,
    pub crash_text: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Color::opaque(0x1e, 0x1e, 0x22),
            text: Color::opaque(0xe6, 0xe6, 0xea),
            button_face: Color::opaque(0x33, 0x33, 0x3c),
            button_border: Color::opaque(0x55, 0x55, 0x62),
            button_label: Color::opaque(0xf0, 0xf0, 0xf4),
            disabled_face: Color::opaque(0x26, 0x26, 0x2c),
            disabled_label: Color::opaque(0x6a, 0x6a, 0x74),
            pressed_face: Color::opaque(0x44, 0x44, 0x52),
            // Deliberately unlike anything the app palette can produce. A
            // crash screen that could be mistaken for a running app is a
            // crash screen that gets ignored.
            crash_background: Color::opaque(0x3a, 0x10, 0x14),
            crash_text: Color::opaque(0xff, 0xd8, 0xd8),
        }
    }
}

/// Where glyphs come from.
///
/// `instar-host` owns *where* text goes — layout decided that, in logical
/// coordinates, using [`instar_ui::TEXT_METRICS`] — and deliberately owns no
/// font machinery at all. An implementation supplies the face and the
/// character-to-glyph mapping, and nothing else; it is not consulted about
/// advances, because the advance is layout's and a shaper that disagreed with
/// it would push text out of the boxes the host computed.
///
/// That is a placeholder arrangement and is expected to end when real shaping
/// lands, at which point measurement and glyph positions come from one font
/// context and this trait dissolves into it.
pub trait GlyphSource: Send + Sync + 'static {
    /// The face every run built from this source refers to.
    fn font(&self) -> FontResource;

    /// This face's glyph for `ch`, or `None` if it has none.
    fn glyph(&self, ch: char) -> Option<u32>;

    /// The size, in physical pixels per em, at which one advance in this face
    /// is `char_width` physical pixels wide.
    ///
    /// The caller passes an already-scaled `char_width`, so a source needs to
    /// know nothing about DPI.
    fn em_size(&self, char_width: f32) -> f32;
}

/// The most trap text the crash surface will retain and draw.
///
/// A trap message is guest-influenced and effectively unbounded — a wasm
/// backtrace runs to hundreds of lines, and a guest that panics with a
/// megabyte of its own choosing is not a strange case to consider. The surface
/// the host puts up *because* something went wrong must itself be impossible
/// to overwhelm, or the failure path becomes the attack surface.
///
/// The cap applies to what is retained, not to what is reported:
/// [`crate::HostEffect::GuestGone`] still carries the whole diagnostic, and the
/// shell logs it in full. Truncation costs you nothing you cannot read
/// elsewhere.
pub const MAX_CRASH_MESSAGE_BYTES: usize = 32 * 1024;

/// The most trap *lines* the crash surface will retain. Binds before
/// [`MAX_CRASH_MESSAGE_BYTES`] for the usual shape of a backtrace: many short
/// frames rather than a few long ones.
pub const MAX_CRASH_MESSAGE_LINES: usize = 512;

/// Appended when either cap bit, so the screen does not quietly imply the
/// diagnostic ended where it was cut.
const TRUNCATED: &str = "\n… truncated; the full diagnostic is in the log";

/// The largest index at or below `index` that splits `text` between characters.
///
/// `str::floor_char_boundary` is unstable; this is the same thing. Slicing a
/// UTF-8 string at an arbitrary byte would panic, and a crash surface that
/// panics while reporting a crash is the one bug it cannot afford.
fn floor_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Trims a diagnostic to something a crash surface can hold.
///
/// Bounded by both caps, whichever binds first, and always on a character
/// boundary. The result never exceeds [`MAX_CRASH_MESSAGE_BYTES`] plus the
/// truncation marker.
pub fn clamp_diagnostic(message: &str) -> String {
    let mut kept = String::new();
    let mut truncated = false;

    // Every iteration either appends one line or breaks, so the enumeration
    // index is the number of lines kept so far.
    for (lines, line) in message.lines().enumerate() {
        if lines == MAX_CRASH_MESSAGE_LINES {
            truncated = true;
            break;
        }
        // The separator is part of the budget: a message of a million empty
        // lines is still a million bytes of newlines.
        let separator = usize::from(!kept.is_empty());
        let Some(budget) = MAX_CRASH_MESSAGE_BYTES.checked_sub(kept.len() + separator) else {
            truncated = true;
            break;
        };
        if separator == 1 {
            kept.push('\n');
        }
        if line.len() <= budget {
            kept.push_str(line);
        } else {
            kept.push_str(&line[..floor_boundary(line, budget)]);
            truncated = true;
            break;
        }
    }

    if truncated {
        kept.push_str(TRUNCATED);
    }
    kept
}

/// One line of text to draw, in physical pixels.
struct TextRun<'a> {
    text: &'a str,
    /// Left edge of the first glyph's advance box.
    x: f32,
    /// Top edge of the line box; the baseline is derived from it.
    y: f32,
    color: Color,
}

/// Turns absolute logical geometry into physical geometry.
///
/// A single place where the scale factor is applied, so "which coordinate
/// space is this in?" has one answer per function rather than one per line.
fn physical(rect: instar_ui::Rect, scale: f32) -> Rect {
    Rect {
        x: (rect.x as f32 * scale).round() as i32,
        y: (rect.y as f32 * scale).round() as i32,
        width: (rect.width as f32 * scale).round() as u32,
        height: (rect.height as f32 * scale).round() as u32,
    }
}

/// Builds the paint intent for one frame.
///
/// Holds the glyph source across frames because a `FontResource` carries the
/// whole font file, and rebuilding one per commit would allocate a font's
/// worth of bytes on every click.
pub struct SceneBuilder {
    theme: Theme,
    glyphs: Option<Arc<dyn GlyphSource>>,
}

impl std::fmt::Debug for SceneBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneBuilder")
            .field("theme", &self.theme)
            .field("has_glyphs", &self.glyphs.is_some())
            .finish()
    }
}

impl Default for SceneBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SceneBuilder {
    /// A builder with no font.
    ///
    /// Scenes come out fully laid out and painted except for glyphs, which are
    /// simply absent. That is the right shape for a headless test — geometry
    /// and color are exactly what they would be with text — and it means the
    /// only thing a missing font can break is text.
    pub fn new() -> Self {
        Self {
            theme: Theme::default(),
            glyphs: None,
        }
    }

    pub fn with_glyphs(glyphs: Arc<dyn GlyphSource>) -> Self {
        Self {
            theme: Theme::default(),
            glyphs: Some(glyphs),
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Lowers the guest's interface to paint intent.
    ///
    /// `metrics` must be usable — [`crate::MetricsState::usable`] is the only
    /// place they should come from. A scene built against invalidated geometry
    /// is exactly what the barrier exists to prevent, and the caller enforces
    /// that rather than this function guessing.
    pub fn app_scene(
        &self,
        tree: &Tree,
        layout: &LayoutSnapshot,
        metrics: &WindowMetricsChanged,
        pressed: Option<instar_ui::NodeKey>,
    ) -> PaintScene {
        let scale = metrics.scale_factor as f32;
        let mut commands = vec![PaintCommand::Clear {
            color: self.theme.background,
        }];
        let mut runs = Vec::new();

        // Tree order is paint order: the wire format is a depth-first
        // preorder, so a parent is drawn before its children and the result
        // is the containment the layout describes.
        for node in tree.iter() {
            let Some(rect) = layout.get(node.key).map(|rect| physical(rect, scale)) else {
                continue;
            };
            self.lower_node(node, rect, scale, pressed, &mut commands, &mut runs);
        }

        self.finish(commands, runs, metrics, scale)
    }

    fn lower_node<'a>(
        &self,
        node: &'a Node,
        rect: Rect,
        scale: f32,
        pressed: Option<instar_ui::NodeKey>,
        commands: &mut Vec<PaintCommand>,
        runs: &mut Vec<TextRun<'a>>,
    ) {
        match &node.kind {
            // Structure only. Drawing a background for these would mean the
            // host inventing appearance for a node whose whole meaning is
            // "these things are stacked".
            NodeKind::Root | NodeKind::Column => {}
            NodeKind::Text { text } => runs.push(TextRun {
                text,
                x: rect.x as f32,
                y: rect.y as f32,
                color: self.theme.text,
            }),
            NodeKind::Button { label, enabled } => {
                let (face, ink) = match (enabled, pressed == Some(node.key)) {
                    (false, _) => (self.theme.disabled_face, self.theme.disabled_label),
                    (true, true) => (self.theme.pressed_face, self.theme.button_label),
                    (true, false) => (self.theme.button_face, self.theme.button_label),
                };
                commands.push(PaintCommand::FillRect { rect, color: face });
                if *enabled {
                    commands.push(PaintCommand::StrokeRect {
                        rect,
                        width: 1.0,
                        color: self.theme.button_border,
                    });
                }
                // Layout reserved `button_padding` on every side; the label
                // sits inside it. Same constant layout measured with, so the
                // text lands where the box was sized for it.
                let padding = TEXT_METRICS.button_padding * scale;
                runs.push(TextRun {
                    text: label,
                    x: rect.x as f32 + padding,
                    y: rect.y as f32 + padding,
                    color: ink,
                });
            }
        }
    }

    /// The window before there is anything in it.
    ///
    /// Not an empty scene: an empty one leaves the buffer holding whatever was
    /// in that memory, which on most platforms is visible garbage. A window
    /// with no interface in it yet should look deliberately blank.
    pub fn blank_scene(&self, metrics: &WindowMetricsChanged) -> PaintScene {
        let scale = metrics.scale_factor as f32;
        self.finish(
            vec![PaintCommand::Clear {
                color: self.theme.background,
            }],
            Vec::new(),
            metrics,
            scale,
        )
    }

    /// The crash screen, built from nothing but the host's own account.
    ///
    /// Takes no tree and no layout, which is the point: it is reachable when
    /// there is no guest left to ask, and it must not depend on one.
    pub fn crash_scene(
        &self,
        generation: GenerationId,
        message: &str,
        metrics: &WindowMetricsChanged,
    ) -> PaintScene {
        let scale = metrics.scale_factor as f32;
        let margin = 16.0 * scale;
        let line = TEXT_METRICS.line_height * scale;

        // Long traps are common and a wall of clipped text helps nobody, so
        // everything here is wrapped to the window rather than run off the
        // edge — the heading included, since a generation id makes it longer
        // than it looks and a narrow window is not a special case.
        let columns = (((metrics.logical_size.width as f32 - 2.0 * 16.0) / TEXT_METRICS.char_width)
            .floor() as usize)
            .max(1);
        let mut lines = wrap(
            &format!("The application stopped responding ({generation})"),
            columns,
        );
        lines.extend(wrap(message, columns));

        // A wasm backtrace can be hundreds of lines. Emitting a glyph run per
        // line for all of them would cost a rasterization pass each and put
        // every one of them off the bottom of the window, so the ones that
        // cannot be seen are not drawn.
        let spacing = line * 1.5;
        let visible = (((metrics.logical_size.height as f32 * scale - margin) / spacing).floor()
            as usize)
            .max(1);
        lines.truncate(visible);

        let runs = lines
            .iter()
            .enumerate()
            .map(|(index, text)| TextRun {
                text,
                x: margin,
                y: margin + index as f32 * spacing,
                color: self.theme.crash_text,
            })
            .collect::<Vec<_>>();

        self.finish(
            vec![PaintCommand::Clear {
                color: self.theme.crash_background,
            }],
            runs,
            metrics,
            scale,
        )
    }

    /// Appends the text runs and assembles the scene.
    ///
    /// Text goes last so it draws over the surfaces it sits on, and all of it
    /// shares one [`PaintCommand::GlyphRun`] per line against a single font
    /// resource, so the backend converts the face once per scene.
    fn finish(
        &self,
        mut commands: Vec<PaintCommand>,
        runs: Vec<TextRun<'_>>,
        metrics: &WindowMetricsChanged,
        scale: f32,
    ) -> PaintScene {
        let size = PhysicalSize {
            width: metrics.physical_size.width,
            height: metrics.physical_size.height,
        };

        let fonts = match &self.glyphs {
            Some(source) => vec![source.font()],
            // No font: geometry and color are still exactly right, and the
            // text is simply not there. See `SceneBuilder::new`.
            None => Vec::new(),
        };

        if let Some(source) = &self.glyphs {
            let advance = TEXT_METRICS.char_width * scale;
            let em = source.em_size(advance);
            for run in runs {
                let glyphs: Arc<[GlyphPosition]> = run
                    .text
                    .chars()
                    .enumerate()
                    .filter_map(|(column, ch)| {
                        source.glyph(ch).map(|id| GlyphPosition {
                            id,
                            x: run.x + column as f32 * advance,
                            // Positions are baselines. Sitting the baseline at
                            // the line box's bottom would clip descenders, so
                            // it goes at the conventional ~80% of the box.
                            y: run.y + TEXT_METRICS.line_height * scale * 0.8,
                        })
                    })
                    .collect();
                if glyphs.is_empty() {
                    continue;
                }
                commands.push(PaintCommand::GlyphRun {
                    run: GlyphRun {
                        font: FontId(0),
                        font_size: em,
                        glyphs,
                        transform: AffineTransform::identity(),
                        color: run.color,
                        hint: true,
                    },
                });
            }
        }

        PaintScene {
            size,
            commands,
            masks: Vec::new(),
            fonts,
            images: Vec::new(),
        }
    }
}

/// Breaks `text` into lines of at most `columns` characters, at whitespace
/// where it can and mid-word where a single word is longer than the window.
fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > columns {
                lines.push(std::mem::take(&mut current));
            }
            // A word wider than the window still has to go somewhere; cutting
            // it is better than one line running off the edge.
            let mut word = word;
            while word.chars().count() > columns {
                let split = word
                    .char_indices()
                    .nth(columns)
                    .map(|(index, _)| index)
                    .unwrap_or(word.len());
                let (head, tail) = word.split_at(split);
                lines.push(head.to_string());
                word = tail;
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_paint::PaintSceneError;
    use instar_ui::NodeKey;
    use instar_ui::protocol::{BatchEncoder, WireDimension, WireLayout, flags, opcode};
    use instar_window::{LogicalSize, WindowId, WindowMetricsChanged};

    const WINDOW: WindowId = WindowId::from_raw(1);
    const LABEL: NodeKey = NodeKey(2);
    const BUTTON: NodeKey = NodeKey(3);
    const DISABLED: NodeKey = NodeKey(4);

    /// Maps every character to a distinct non-zero glyph id, so a test can
    /// count glyphs and see which ones without a font file.
    struct FakeGlyphs;

    impl GlyphSource for FakeGlyphs {
        fn font(&self) -> FontResource {
            FontResource {
                key: instar_paint::FontKey(1),
                data: Arc::from(&b"not a real font"[..]),
                index: 0,
            }
        }

        fn glyph(&self, ch: char) -> Option<u32> {
            // Space maps to nothing, like a face with no space glyph, so the
            // "missing glyphs are skipped" path is exercised by ordinary text.
            (ch != ' ').then_some(ch as u32)
        }

        fn em_size(&self, char_width: f32) -> f32 {
            char_width * 2.0
        }
    }

    fn metrics(scale: f64) -> WindowMetricsChanged {
        WindowMetricsChanged {
            window_id: WINDOW,
            logical_size: LogicalSize {
                width: 400.0,
                height: 300.0,
            },
            physical_size: instar_window::PhysicalSize {
                width: (400.0 * scale) as u32,
                height: (300.0 * scale) as u32,
            },
            scale_factor: scale,
        }
    }

    fn tree() -> Tree {
        let fill = WireLayout {
            width: WireDimension::Fill,
            height: WireDimension::Content,
            padding: 0,
            gap: 0,
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey(1), 0, None, fill, 3)
            .node(
                opcode::NODE_TEXT,
                LABEL,
                0,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                BUTTON,
                flags::ENABLED,
                Some("Press me"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                DISABLED,
                0,
                Some("Reset"),
                WireLayout::default(),
                0,
            );
        Tree::decode(&encoder.finish()).expect("the fixture batch is valid")
    }

    fn scene(scale: f64, pressed: Option<NodeKey>) -> PaintScene {
        let tree = tree();
        let metrics = metrics(scale);
        let layout = tree.layout(instar_ui::Viewport::new(
            metrics.logical_size.width as f32,
            metrics.logical_size.height as f32,
        ));
        SceneBuilder::with_glyphs(Arc::new(FakeGlyphs)).app_scene(&tree, &layout, &metrics, pressed)
    }

    fn fills(scene: &PaintScene) -> Vec<Rect> {
        scene
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::FillRect { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect()
    }

    fn glyph_runs(scene: &PaintScene) -> Vec<&GlyphRun> {
        scene
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::GlyphRun { run } => Some(run),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_scene_opens_with_an_opaque_clear_and_balances_its_clips() {
        let scene = scene(1.0, None);
        assert!(
            matches!(
                scene.commands.first(),
                Some(PaintCommand::Clear { color }) if color.a == 255
            ),
            "presentation packs into an opaque buffer; an alpha frame cannot be shown"
        );
        assert_eq!(scene.validate(), Ok::<(), PaintSceneError>(()));
    }

    #[test]
    fn the_scene_is_sized_in_physical_pixels() {
        let scene = scene(2.0, None);
        assert_eq!(
            scene.size,
            PhysicalSize {
                width: 800,
                height: 600
            },
            "a renderer wants the physical target, whatever the logical viewport was"
        );
    }

    /// The DPI split, from the paint side: doubling the scale must double
    /// every rectangle, because `instar-ui` produced the same logical numbers
    /// both times and this module is the only thing that scales them.
    #[test]
    fn doubling_the_scale_doubles_the_geometry() {
        let single = fills(&scene(1.0, None));
        let double = fills(&scene(2.0, None));

        assert_eq!(single.len(), double.len());
        assert!(!single.is_empty(), "the fixture has buttons to fill");
        for (one, two) in single.iter().zip(&double) {
            assert_eq!(
                (two.x, two.y, two.width, two.height),
                (one.x * 2, one.y * 2, one.width * 2, one.height * 2),
            );
        }
    }

    #[test]
    fn a_disabled_button_is_drawn_differently_and_without_a_border() {
        let scene = scene(1.0, None);
        let theme = Theme::default();

        let faces: Vec<Color> = scene
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::FillRect { color, .. } => Some(*color),
                _ => None,
            })
            .collect();
        assert!(faces.contains(&theme.button_face));
        assert!(
            faces.contains(&theme.disabled_face),
            "a disabled control should look unavailable, not vanish"
        );

        let borders = scene
            .commands
            .iter()
            .filter(|command| matches!(command, PaintCommand::StrokeRect { .. }))
            .count();
        assert_eq!(borders, 1, "only the enabled button is outlined");
    }

    #[test]
    fn a_pressed_button_is_drawn_pressed() {
        let idle = scene(1.0, None);
        let held = scene(1.0, Some(BUTTON));
        assert_ne!(
            fills(&idle).len(),
            0,
            "the fixture should produce filled buttons"
        );
        assert_ne!(
            idle.commands, held.commands,
            "a held button must look different, or a click has no feedback"
        );
    }

    /// The property that keeps painting honest about who owns geometry: the
    /// host measured these boxes with [`TEXT_METRICS`], so the glyphs must be
    /// placed with the same advance. A painter using its font's own advances
    /// would pass every other test here and produce text hanging out of its
    /// button.
    #[test]
    fn every_glyph_lands_inside_the_box_layout_computed_for_it() {
        let tree = tree();
        let metrics = metrics(1.0);
        let layout = tree.layout(instar_ui::Viewport::new(400.0, 300.0));
        let scene = SceneBuilder::with_glyphs(Arc::new(FakeGlyphs))
            .app_scene(&tree, &layout, &metrics, None);

        // `FakeGlyphs` maps a character to its own code point, so a run can be
        // matched back to the node whose text produced it — which is what makes
        // "inside *its own* box" checkable rather than "inside some box".
        for (key, text) in [
            (LABEL, "Clicked 0 times"),
            (BUTTON, "Press me"),
            (DISABLED, "Reset"),
        ] {
            let expected: Vec<u32> = text
                .chars()
                .filter(|c| *c != ' ')
                .map(|c| c as u32)
                .collect();
            let run = glyph_runs(&scene)
                .into_iter()
                .find(|run| run.glyphs.iter().map(|g| g.id).eq(expected.iter().copied()))
                .unwrap_or_else(|| panic!("{text:?} should have been laid into a glyph run"));

            let box_ = layout.get(key).expect("laid out");
            for glyph in run.glyphs.iter() {
                assert!(
                    glyph.x >= box_.x as f32
                        && glyph.x + TEXT_METRICS.char_width <= (box_.x + box_.width) as f32,
                    "a glyph of {text:?} at x={} escapes the box layout sized for it \
                     ({box_:?}); painting must use the advance layout measured with",
                    glyph.x
                );
                assert!(
                    glyph.y >= box_.y as f32 && glyph.y <= (box_.y + box_.height) as f32,
                    "the baseline of {text:?} at y={} is outside its box ({box_:?})",
                    glyph.y
                );
            }
        }
    }

    #[test]
    fn text_is_painted_after_the_surfaces_it_sits_on() {
        let scene = scene(1.0, None);
        let first_glyphs = scene
            .commands
            .iter()
            .position(|command| matches!(command, PaintCommand::GlyphRun { .. }))
            .expect("the fixture has text");
        let last_fill = scene
            .commands
            .iter()
            .rposition(|command| matches!(command, PaintCommand::FillRect { .. }))
            .expect("the fixture has buttons");
        assert!(
            first_glyphs > last_fill,
            "a button drawn after its label would erase it"
        );
    }

    #[test]
    fn without_a_font_everything_but_the_text_is_still_painted() {
        let tree = tree();
        let metrics = metrics(1.0);
        let layout = tree.layout(instar_ui::Viewport::new(400.0, 300.0));

        let with = SceneBuilder::with_glyphs(Arc::new(FakeGlyphs))
            .app_scene(&tree, &layout, &metrics, None);
        let without = SceneBuilder::new().app_scene(&tree, &layout, &metrics, None);

        assert_eq!(
            fills(&with),
            fills(&without),
            "a missing font should cost the text and nothing else"
        );
        assert!(without.fonts.is_empty());
        assert!(glyph_runs(&without).is_empty());
    }

    // --- the crash screen ---

    #[test]
    fn a_crash_scene_needs_no_tree_and_no_layout() {
        let scene = SceneBuilder::with_glyphs(Arc::new(FakeGlyphs)).crash_scene(
            GenerationId(3),
            "guest trapped: unreachable",
            &metrics(1.0),
        );

        assert!(
            matches!(
                scene.commands.first(),
                Some(PaintCommand::Clear { color }) if *color == Theme::default().crash_background
            ),
            "the crash screen must not be mistakable for a running app"
        );
        assert!(
            !glyph_runs(&scene).is_empty(),
            "a crash screen that says nothing is a blank window"
        );
        assert_eq!(scene.validate(), Ok::<(), PaintSceneError>(()));
    }

    #[test]
    fn a_long_trap_message_is_wrapped_rather_than_run_off_the_edge() {
        let message = "trap: ".to_string() + &"detail ".repeat(200);
        let scene = SceneBuilder::with_glyphs(Arc::new(FakeGlyphs)).crash_scene(
            GenerationId(1),
            &message,
            &metrics(1.0),
        );

        let width = metrics(1.0).logical_size.width as f32;
        for run in glyph_runs(&scene) {
            let right = run
                .glyphs
                .iter()
                .map(|glyph| glyph.x + TEXT_METRICS.char_width)
                .fold(0.0_f32, f32::max);
            assert!(
                right <= width,
                "a run reaching {right} escapes a {width}-wide window"
            );
        }
    }

    // --- boundedness of the crash surface ---

    #[test]
    fn a_diagnostic_within_both_caps_is_kept_verbatim() {
        let message = "guest trapped: unreachable\n  at frame 0\n  at frame 1";
        assert_eq!(clamp_diagnostic(message), message);
    }

    #[test]
    fn too_many_lines_are_cut_and_the_cut_is_admitted() {
        let message = (0..MAX_CRASH_MESSAGE_LINES * 4)
            .map(|n| format!("frame {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let clamped = clamp_diagnostic(&message);

        assert_eq!(
            clamped.lines().count(),
            MAX_CRASH_MESSAGE_LINES + 1,
            "the extra line is the truncation marker"
        );
        assert!(
            clamped.ends_with(TRUNCATED),
            "a silent cut would imply the backtrace ended where it was trimmed"
        );
    }

    /// The case line-counting misses: one enormous line. A guest choosing its
    /// own panic message can produce a megabyte without a single newline.
    #[test]
    fn one_enormous_line_is_bounded_too() {
        let clamped = clamp_diagnostic(&"x".repeat(MAX_CRASH_MESSAGE_BYTES * 4));
        assert!(
            clamped.len() <= MAX_CRASH_MESSAGE_BYTES + TRUNCATED.len(),
            "a single line of {} bytes must still be bounded, got {}",
            MAX_CRASH_MESSAGE_BYTES * 4,
            clamped.len()
        );
        assert!(clamped.ends_with(TRUNCATED));
    }

    /// Every cap is a byte index into text a guest chose, and slicing UTF-8
    /// at the wrong byte panics. A crash surface that panics while reporting a
    /// crash is the one bug it cannot afford.
    #[test]
    fn a_multibyte_diagnostic_is_cut_on_a_character_boundary() {
        // 3 bytes per character, so a byte cap lands mid-character constantly.
        let clamped = clamp_diagnostic(&"☃".repeat(MAX_CRASH_MESSAGE_BYTES));
        assert!(clamped.len() <= MAX_CRASH_MESSAGE_BYTES + TRUNCATED.len());

        let kept = clamped
            .strip_suffix(TRUNCATED)
            .expect("a cap this far below the input must have truncated");
        assert!(
            kept.chars().all(|c| c == '☃'),
            "the cut must land between characters, not inside one"
        );
    }

    #[test]
    fn a_flood_of_empty_lines_is_bounded_by_its_newlines() {
        let clamped = clamp_diagnostic(&"\n".repeat(MAX_CRASH_MESSAGE_BYTES * 4));
        assert!(
            clamped.len() <= MAX_CRASH_MESSAGE_BYTES + TRUNCATED.len(),
            "separators count against the budget, or empty lines are unbounded"
        );
    }

    /// The cap has to survive contact with the thing it protects: a clamped
    /// message still has to lay out and draw.
    #[test]
    fn a_clamped_diagnostic_still_produces_a_drawable_crash_screen() {
        let flood = "trap\n".repeat(MAX_CRASH_MESSAGE_LINES * 8);
        let scene = SceneBuilder::with_glyphs(Arc::new(FakeGlyphs)).crash_scene(
            GenerationId(1),
            &clamp_diagnostic(&flood),
            &metrics(1.0),
        );
        assert_eq!(scene.validate(), Ok::<(), PaintSceneError>(()));
        assert!(!glyph_runs(&scene).is_empty());
    }

    /// A trap can carry a wasm backtrace hundreds of lines long. Drawing the
    /// ones below the window would cost a rasterization pass each to produce
    /// nothing anybody can see.
    #[test]
    fn a_crash_message_is_not_drawn_past_the_bottom_of_the_window() {
        let message = (0..500)
            .map(|n| format!("frame {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let scene = SceneBuilder::with_glyphs(Arc::new(FakeGlyphs)).crash_scene(
            GenerationId(1),
            &message,
            &metrics(1.0),
        );

        let height = metrics(1.0).logical_size.height as f32;
        let runs = glyph_runs(&scene);
        assert!(!runs.is_empty(), "the visible lines should still be drawn");
        for run in runs {
            let lowest = run.glyphs.iter().map(|g| g.y).fold(0.0_f32, f32::max);
            assert!(
                lowest <= height,
                "a baseline at {lowest} is below a {height}-tall window"
            );
        }
    }

    #[test]
    fn wrapping_breaks_a_word_longer_than_the_window() {
        let lines = wrap(&"x".repeat(25), 10);
        assert!(
            lines.iter().all(|line| line.chars().count() <= 10),
            "an unbreakable word still has to fit: {lines:?}"
        );
        assert_eq!(lines.concat(), "x".repeat(25), "no characters are lost");
    }

    #[test]
    fn wrapping_preserves_every_word() {
        let lines = wrap("the guest trapped while handling a click", 12);
        assert!(lines.iter().all(|line| line.chars().count() <= 12));
        assert_eq!(lines.join(" "), "the guest trapped while handling a click");
    }
}
