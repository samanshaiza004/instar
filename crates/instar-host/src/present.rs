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
//! [`crash_scene`] shapes the host's own account of the failure directly.
//! The tempting shortcut — synthesizing an Instar tree that says "the app
//! crashed" and pushing it through the normal path — is rejected: it would
//! mean the host can author interfaces in the guest's name, and every
//! downstream consumer (hit-testing, the commit log, anything that later asks
//! "what did the guest commit?") would be told a lie by a layer that is
//! supposed to be transcribing. The retained tree keeps saying whatever the
//! guest last said. What the window shows is a separate question, and this
//! module is where it is answered.
//!
//! # Physical here, logical above
//!
//! Everything in this module is in physical pixels: it is the far side of the
//! boundary `docs/PHASE-1.md` draws, where `instar-host` converts the logical
//! geometry `instar-ui` produced into the physical target a renderer wants.
//! `instar-ui` never sees a scale factor, and nothing here feeds back into it.
//!
//! Text arrives as [`instar_ui::ShapedText`], already extracted from Parley
//! in logical space. Lowering multiplies the run's font size and every glyph
//! position by the display scale, and keeps `AffineTransform::identity()`:
//! Vello treats `font_size` as pixels-per-em when selecting bitmap and colour
//! glyph strikes, so scaling the transform instead would not be equivalent.

use std::collections::HashMap;
use std::sync::Arc;

use instar_kernel::runtime::GenerationId;
use instar_paint::{
    AffineTransform, Color, FontId, FontKey, FontResource, GlyphPosition, GlyphRun, PaintCommand,
    PaintScene, PhysicalSize, Rect,
};
use instar_ui::{
    Available, BUTTON_PADDING, LayoutSnapshot, NodeKey, NodeKind, ScrollOffset, ScrollState,
    ShapedText, ShapingStyle, TextContext, Tree,
};
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
/// Fonts are not owned here: the shaped text carries every face it used, and
/// [`Self::app_scene`] deduplicates those faces into the scene's font table.
#[derive(Debug, Default)]
pub struct SceneBuilder {
    theme: Theme,
}

impl SceneBuilder {
    pub fn new() -> Self {
        Self {
            theme: Theme::default(),
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
        scroll: &ScrollState,
        metrics: &WindowMetricsChanged,
        pressed: Option<instar_ui::NodeKey>,
    ) -> PaintScene {
        let scale = metrics.scale_factor as f32;
        let mut commands = vec![PaintCommand::Clear {
            color: self.theme.background,
        }];
        let mut fonts = Vec::new();
        let mut font_ids = HashMap::new();
        self.paint_node(
            &tree.root,
            layout,
            scroll,
            ScrollOffset::ZERO,
            pressed,
            scale,
            &mut commands,
            &mut fonts,
            &mut font_ids,
        );
        scene(metrics, commands, fonts)
    }

    /// Emits one node and its subtree, in paint order.
    ///
    /// # Why this recurses instead of walking `tree.iter()`
    ///
    /// `PushClip`/`PopClip` bracket the commands they apply to, so emitting
    /// them needs the shape of the tree, which a flat preorder iterator has
    /// thrown away.
    ///
    /// Text moved inline as part of the same change. It used to be collected
    /// into a list and flushed after every rectangle — which no clip could
    /// have contained, and which was already wrong for [`NodeKind::Stack`]:
    /// with every face drawn before every glyph, a lower stacked child's text
    /// painted over a higher child's face. Tree order is paint order for both
    /// now.
    #[allow(clippy::too_many_arguments)]
    fn paint_node(
        &self,
        node: &instar_ui::Node,
        layout: &LayoutSnapshot,
        scroll: &ScrollState,
        translation: ScrollOffset,
        pressed: Option<instar_ui::NodeKey>,
        scale: f32,
        commands: &mut Vec<PaintCommand>,
        fonts: &mut Vec<FontResource>,
        font_ids: &mut HashMap<u64, FontId>,
    ) {
        // `Display::None` and `Visibility::Hidden` are the same answer here,
        // and both cover the subtree. A `Display::None` node has no rect
        // either, so this is belt and braces for the first and the only guard
        // for the second.
        if !instar_ui::is_presented(node) {
            return;
        }
        let Some(rect) = layout.get(node.key) else {
            return;
        };
        // Every rect out of the snapshot is in absolute layout coordinates.
        // Painting inside a scrolled viewport moves it by the accumulated
        // translation of the viewports above it.
        let rect = instar_ui::Rect::new(
            rect.x - translation.x,
            rect.y - translation.y,
            rect.width,
            rect.height,
        );

        // A `Scroll` clips because it is a viewport, joining `Overflow::Clip`
        // at the step A3 established rather than opening a second path. Only
        // the translation below is new.
        let clipped = node.layout.overflow == instar_ui::WireOverflow::Clip
            || matches!(node.kind, NodeKind::Scroll);
        if clipped {
            commands.push(PaintCommand::PushClip {
                rect: physical(rect, scale),
            });
        }

        // Descendants of a viewport move up and left by its offset, which is
        // what "scrolled down" means. Accumulated rather than replaced, so a
        // viewport inside a viewport composes.
        let child_translation = match node.kind {
            NodeKind::Scroll => {
                let offset = scroll.get(node.key);
                ScrollOffset::new(translation.x + offset.x, translation.y + offset.y)
            }
            _ => translation,
        };

        match &node.kind {
            // Structure only. Drawing a background for these would mean the
            // host inventing appearance for a node whose whole meaning is
            // "these things are stacked".
            NodeKind::Root
            | NodeKind::Column
            | NodeKind::Row
            | NodeKind::Stack
            | NodeKind::Scroll => {}
            NodeKind::Text { .. } => {
                if let Some(shaped) = layout.text(node.key) {
                    push_shaped(
                        commands,
                        fonts,
                        font_ids,
                        shaped,
                        (rect.x as f32, rect.y as f32),
                        scale,
                        self.theme.text,
                    );
                }
            }
            NodeKind::Button { enabled, .. } => {
                let (face, ink) = match (enabled, pressed == Some(node.key)) {
                    (false, _) => (self.theme.disabled_face, self.theme.disabled_label),
                    (true, true) => (self.theme.pressed_face, self.theme.button_label),
                    (true, false) => (self.theme.button_face, self.theme.button_label),
                };
                let physical_rect = physical(rect, scale);
                commands.push(PaintCommand::FillRect {
                    rect: physical_rect,
                    color: face,
                });
                if *enabled {
                    commands.push(PaintCommand::StrokeRect {
                        rect: physical_rect,
                        width: 1.0,
                        color: self.theme.button_border,
                    });
                }
                // Layout reserved `BUTTON_PADDING` on every side; the label
                // sits inside it. Same constant layout measured with, so the
                // text lands where the box was sized for it.
                if let Some(shaped) = layout.text(node.key) {
                    push_shaped(
                        commands,
                        fonts,
                        font_ids,
                        shaped,
                        (
                            rect.x as f32 + BUTTON_PADDING,
                            rect.y as f32 + BUTTON_PADDING,
                        ),
                        scale,
                        ink,
                    );
                }
            }
        }

        for child in &node.children {
            self.paint_node(
                child,
                layout,
                scroll,
                child_translation,
                pressed,
                scale,
                commands,
                fonts,
                font_ids,
            );
        }

        if clipped {
            commands.push(PaintCommand::PopClip);
        }
    }

    /// The window before there is anything in it.
    ///
    /// Not an empty scene: an empty one leaves the buffer holding whatever was
    /// in that memory, which on most platforms is visible garbage. A window
    /// with no interface in it yet should look deliberately blank.
    pub fn blank_scene(&self, metrics: &WindowMetricsChanged) -> PaintScene {
        scene(
            metrics,
            vec![PaintCommand::Clear {
                color: self.theme.background,
            }],
            Vec::new(),
        )
    }

    /// The crash screen, built from nothing but the host's own account.
    ///
    /// Takes no tree and no layout, which is the point: it is reachable when
    /// there is no guest left to ask, and it must not depend on one. Text is
    /// shaped through the host's long-lived [`TextContext`]; the reserved key
    /// is far outside the protocol's node range, and the entry is overwritten
    /// whenever a new trap replaces the screen.
    pub fn crash_scene(
        &self,
        text: &mut TextContext,
        generation: GenerationId,
        message: &str,
        metrics: &WindowMetricsChanged,
    ) -> PaintScene {
        let scale = metrics.scale_factor as f32;
        let margin = 16.0;
        let content = format!("The application stopped responding ({generation})\n{message}");
        let width = (metrics.logical_size.width as f32 - 2.0 * margin).max(1.0);

        // Let Parley wrap the whole diagnostic to the content width. A wasm
        // backtrace can be hundreds of lines; lines below the window are
        // dropped here rather than rasterized into nothing.
        text.measure(
            CRASH_KEY,
            &content,
            ShapingStyle::default(),
            Available::Definite(width),
        );
        let mut visible = text.finalize(CRASH_KEY, width).clone();
        let visible_bottom = metrics.logical_size.height as f32 - margin;
        for run in &mut visible.runs {
            run.glyphs.retain(|glyph| glyph.y <= visible_bottom);
        }
        visible.runs.retain(|run| !run.glyphs.is_empty());

        let mut commands = vec![PaintCommand::Clear {
            color: self.theme.crash_background,
        }];
        let mut fonts = Vec::new();
        let mut font_ids = HashMap::new();
        push_shaped(
            &mut commands,
            &mut fonts,
            &mut font_ids,
            &visible,
            (margin, margin),
            scale,
            self.theme.crash_text,
        );
        scene(metrics, commands, fonts)
    }
}

/// The reserved cache key for the host-owned crash text. Protocol nodes are
/// bounded far below `u32::MAX`, so this cannot collide with a guest key.
const CRASH_KEY: NodeKey = NodeKey::first(u32::MAX);

fn scene(
    metrics: &WindowMetricsChanged,
    commands: Vec<PaintCommand>,
    fonts: Vec<FontResource>,
) -> PaintScene {
    PaintScene {
        size: PhysicalSize {
            width: metrics.physical_size.width,
            height: metrics.physical_size.height,
        },
        commands,
        masks: Vec::new(),
        fonts,
        images: Vec::new(),
    }
}

/// Appends one [`ShapedText`] as positioned glyph runs.
///
/// Fonts are deduplicated across the whole scene by [`instar_ui::FontFace::key`],
/// and each run indexes the scene's table. `FontFace::data` is an `Arc<[u8]>`,
/// so this is a refcount bump, never a copy of font bytes.
fn push_shaped(
    commands: &mut Vec<PaintCommand>,
    fonts: &mut Vec<FontResource>,
    font_ids: &mut HashMap<u64, FontId>,
    shaped: &ShapedText,
    origin: (f32, f32),
    scale: f32,
    color: Color,
) {
    for run in &shaped.runs {
        let face = &shaped.fonts[run.font];
        let font = match font_ids.get(&face.key) {
            Some(font) => *font,
            None => {
                let font = FontId(fonts.len() as u32);
                fonts.push(FontResource {
                    key: FontKey(face.key),
                    data: Arc::clone(&face.data),
                    index: face.index,
                });
                font_ids.insert(face.key, font);
                font
            }
        };
        let glyphs: Arc<[GlyphPosition]> = run
            .glyphs
            .iter()
            .map(|glyph| GlyphPosition {
                id: glyph.id,
                x: (origin.0 + glyph.x) * scale,
                y: (origin.1 + glyph.y) * scale,
            })
            .collect();
        if glyphs.is_empty() {
            continue;
        }
        commands.push(PaintCommand::GlyphRun {
            run: GlyphRun {
                font,
                font_size: run.font_size * scale,
                glyphs,
                transform: AffineTransform::identity(),
                color,
                hint: false,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_paint::PaintSceneError;
    use instar_ui::Viewport;
    use instar_ui::protocol::{BatchEncoder, WireAlign, WireLayout, flags, opcode};
    use instar_window::{LogicalSize, WindowId};

    const WINDOW: WindowId = WindowId::from_raw(1);
    const LABEL: NodeKey = NodeKey::first(2);
    const BUTTON: NodeKey = NodeKey::first(3);
    const DISABLED: NodeKey = NodeKey::first(4);

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
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey::first(1), 0, None, fill, 3)
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
        let mut text = TextContext::new();
        let layout = tree.layout(
            &mut text,
            Viewport::new(
                metrics.logical_size.width as f32,
                metrics.logical_size.height as f32,
            ),
        );
        SceneBuilder::new().app_scene(&tree, &layout, &ScrollState::new(), &metrics, pressed)
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
    /// every rectangle and every glyph position and font size, because
    /// `instar-ui` produced the same logical numbers both times and this
    /// module is the only thing that scales them.
    #[test]
    fn doubling_the_scale_doubles_the_geometry() {
        let single = scene(1.0, None);
        let double = scene(2.0, None);

        assert_eq!(fills(&single).len(), fills(&double).len());
        assert!(
            !fills(&single).is_empty(),
            "the fixture has buttons to fill"
        );
        for (one, two) in fills(&single).iter().zip(fills(&double)) {
            assert_eq!(
                (two.x, two.y, two.width, two.height),
                (one.x * 2, one.y * 2, one.width * 2, one.height * 2),
            );
        }

        let single_runs = glyph_runs(&single);
        let double_runs = glyph_runs(&double);
        assert_eq!(single_runs.len(), double_runs.len());
        for (one, two) in single_runs.iter().zip(&double_runs) {
            assert!(
                (two.font_size - one.font_size * 2.0).abs() < 0.001,
                "font size is physical pixels per em and must scale"
            );
            assert_eq!(two.glyphs.len(), one.glyphs.len());
            for (a, b) in one.glyphs.iter().zip(two.glyphs.iter()) {
                assert!(
                    (b.x - a.x * 2.0).abs() < 0.001 && (b.y - a.y * 2.0).abs() < 0.001,
                    "glyph positions must scale with the display"
                );
            }
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

    /// The property that keeps painting honest about who owns geometry: every
    /// glyph run must land inside the box the host laid out for the node whose
    /// text produced it, now with real proportional advances rather than a
    /// fixed-pitch column count.
    #[test]
    fn every_glyph_lands_inside_the_box_layout_computed_for_it() {
        let tree = tree();
        let metrics = metrics(1.0);
        let mut text = TextContext::new();
        let layout = tree.layout(
            &mut text,
            Viewport::new(
                metrics.logical_size.width as f32,
                metrics.logical_size.height as f32,
            ),
        );
        let scene =
            SceneBuilder::new().app_scene(&tree, &layout, &ScrollState::new(), &metrics, None);
        let keys = [LABEL, BUTTON, DISABLED];

        for run in glyph_runs(&scene) {
            let owner = keys.into_iter().find(|key| {
                let rect = physical(layout.get(*key).expect("laid out"), 1.0);
                run.glyphs.iter().all(|glyph| {
                    glyph.x >= rect.x as f32
                        && glyph.x < rect.x as f32 + rect.width as f32
                        && glyph.y >= rect.y as f32
                        && glyph.y <= rect.y as f32 + rect.height as f32
                })
            });
            assert!(
                owner.is_some(),
                "a glyph run escaped every text box in the fixture"
            );
        }

        for key in keys {
            let shaped = layout.text(key).expect("the node should have shaped text");
            assert!(!shaped.runs.is_empty(), "node {key} produced no runs");
            let rect = physical(layout.get(key).expect("laid out"), 1.0);
            assert!(
                glyph_runs(&scene).iter().any(|run| {
                    run.glyphs.iter().all(|glyph| {
                        glyph.x >= rect.x as f32
                            && glyph.x < rect.x as f32 + rect.width as f32
                            && glyph.y >= rect.y as f32
                            && glyph.y <= rect.y as f32 + rect.height as f32
                    })
                }),
                "node {key} has no run inside its own box"
            );
        }
    }

    #[test]
    fn scene_fonts_are_deduplicated_by_face_key() {
        let scene = scene(1.0, None);
        assert!(
            !scene.fonts.is_empty(),
            "the fixture should have shaped text"
        );
        let mut keys = scene
            .fonts
            .iter()
            .map(|font| font.key.0)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(
            keys.len(),
            scene.fonts.len(),
            "the same face must not be carried twice in one scene"
        );
        for run in glyph_runs(&scene) {
            assert!(
                (run.font.0 as usize) < scene.fonts.len(),
                "every run indexes the scene's font table"
            );
        }
    }

    #[test]
    fn text_is_painted_after_the_surfaces_it_sits_on() {
        let scene = scene(1.0, None);

        // Per button, not globally. This used to assert that every glyph came
        // after every fill, which held only because text was collected and
        // flushed at the end of the scene — and that deferral was itself a bug
        // for `Stack`, where a lower child's text painted over a higher
        // child's face. A3 made tree order paint order for both, so the real
        // property is the one the old assertion was standing in for: nothing
        // is filled between a button's face and its label.
        let mut fills = 0;
        let mut glyphs_since_fill = 0;
        for command in &scene.commands {
            match command {
                PaintCommand::FillRect { .. } => {
                    if fills > 0 {
                        assert!(
                            glyphs_since_fill > 0,
                            "a button's face was drawn before the previous \
                             button's label, which would erase it"
                        );
                    }
                    fills += 1;
                    glyphs_since_fill = 0;
                }
                PaintCommand::GlyphRun { .. } => glyphs_since_fill += 1,
                _ => {}
            }
        }
        assert!(fills > 0, "the fixture has buttons");
        assert!(
            glyphs_since_fill > 0,
            "the last button's label must follow its face"
        );
    }

    /// Painting is the other half of the same transform hit-testing does.
    ///
    /// Concrete numbers rather than a property: a button at content y = 200
    /// under an offset of 150 must be filled at viewport y = 50, and a sign
    /// error would put it at 350 while still passing any "it moved" check.
    #[test]
    fn a_scrolled_viewport_paints_its_content_translated() {
        let tree = Tree::new(instar_ui::Node::root(
            0,
            vec![
                instar_ui::Node::scroll(
                    1,
                    instar_ui::Node::column(
                        2,
                        vec![
                            instar_ui::Node::text(3, "spacer").with_layout(instar_ui::WireLayout {
                                height: instar_ui::WireSize::Fixed(200),
                                ..instar_ui::WireLayout::default()
                            }),
                            instar_ui::Node::button(4, "target").with_layout(
                                instar_ui::WireLayout {
                                    height: instar_ui::WireSize::Fixed(40),
                                    ..instar_ui::WireLayout::default()
                                },
                            ),
                        ],
                    ),
                )
                .with_layout(instar_ui::WireLayout {
                    height: instar_ui::WireSize::Fixed(100),
                    align_self: Some(instar_ui::WireAlign::Stretch),
                    ..instar_ui::WireLayout::default()
                }),
            ],
        ));
        let metrics = metrics(1.0);
        let mut text = TextContext::new();
        let layout = tree.layout(
            &mut text,
            Viewport::new(
                metrics.logical_size.width as f32,
                metrics.logical_size.height as f32,
            ),
        );
        assert_eq!(
            layout.get(NodeKey::first(4)).unwrap().y,
            200,
            "the fixture puts its target at content y = 200"
        );

        let mut scroll = ScrollState::new();
        scroll.set(NodeKey::first(1), ScrollOffset::new(0, 150));
        let scene = SceneBuilder::new().app_scene(&tree, &layout, &scroll, &metrics, None);

        let filled: Vec<i32> = scene
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::FillRect { rect, .. } => Some(rect.y),
                _ => None,
            })
            .collect();
        assert_eq!(
            filled,
            vec![50],
            "200 minus an offset of 150 is 50, and nothing else is filled"
        );
    }

    /// A viewport is a clip, and it uses the one A3 established.
    #[test]
    fn a_scroll_pushes_a_clip_and_balances_it() {
        let tree = Tree::new(instar_ui::Node::root(
            0,
            vec![instar_ui::Node::scroll(
                1,
                instar_ui::Node::button(2, "inside"),
            )],
        ));
        let metrics = metrics(1.0);
        let mut text = TextContext::new();
        let layout = tree.layout(
            &mut text,
            Viewport::new(
                metrics.logical_size.width as f32,
                metrics.logical_size.height as f32,
            ),
        );
        let scene =
            SceneBuilder::new().app_scene(&tree, &layout, &ScrollState::new(), &metrics, None);

        assert!(
            scene
                .commands
                .iter()
                .any(|command| matches!(command, PaintCommand::PushClip { .. })),
            "a viewport clips without being told to"
        );
        scene
            .validate()
            .expect("every PushClip a scroll emits must be matched");
    }

    /// The bug the deferred-text list was hiding.
    ///
    /// A `Stack` overlaps its children and later ones paint over earlier ones.
    /// With every face emitted before every glyph, the *first* child's text
    /// was drawn after the *second* child's face — so the thing underneath
    /// showed through the thing on top.
    #[test]
    fn a_stacked_child_does_not_paint_its_text_over_a_later_sibling() {
        let tree = Tree::new(instar_ui::Node::root(
            0,
            vec![instar_ui::Node::stack(
                1,
                vec![
                    instar_ui::Node::button(2, "under"),
                    instar_ui::Node::button(3, "over"),
                ],
            )],
        ));
        let metrics = metrics(1.0);
        let mut text = TextContext::new();
        let layout = tree.layout(
            &mut text,
            Viewport::new(
                metrics.logical_size.width as f32,
                metrics.logical_size.height as f32,
            ),
        );
        let scene =
            SceneBuilder::new().app_scene(&tree, &layout, &ScrollState::new(), &metrics, None);

        let kinds: Vec<&'static str> = scene
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::FillRect { .. } => Some("fill"),
                PaintCommand::GlyphRun { .. } => Some("glyphs"),
                _ => None,
            })
            .collect();
        let second_fill = kinds
            .iter()
            .enumerate()
            .filter(|(_, kind)| **kind == "fill")
            .nth(1)
            .map(|(index, _)| index)
            .expect("both stacked buttons are filled");
        let first_glyphs = kinds
            .iter()
            .position(|kind| *kind == "glyphs")
            .expect("both stacked buttons have labels");
        assert!(
            first_glyphs < second_fill,
            "the lower child's label must be painted before the upper \
             child's face covers it: {kinds:?}"
        );
    }

    #[test]
    fn without_shaped_text_everything_but_the_text_is_still_painted() {
        let tree = tree();
        let metrics = metrics(1.0);
        let mut text = TextContext::new();
        let mut layout = tree.layout(
            &mut text,
            Viewport::new(
                metrics.logical_size.width as f32,
                metrics.logical_size.height as f32,
            ),
        );

        let with =
            SceneBuilder::new().app_scene(&tree, &layout, &ScrollState::new(), &metrics, None);
        layout.text.clear();
        let without =
            SceneBuilder::new().app_scene(&tree, &layout, &ScrollState::new(), &metrics, None);

        assert_eq!(
            fills(&with),
            fills(&without),
            "missing shaped text should cost the glyphs and nothing else"
        );
        assert!(without.fonts.is_empty());
        assert!(glyph_runs(&without).is_empty());
    }

    // --- the crash screen ---

    #[test]
    fn a_crash_scene_needs_no_tree_and_no_layout() {
        let mut text = TextContext::new();
        let scene = SceneBuilder::new().crash_scene(
            &mut text,
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
        let mut text = TextContext::new();
        let scene =
            SceneBuilder::new().crash_scene(&mut text, GenerationId(1), &message, &metrics(1.0));

        // Glyph positions in a scene are absolute, so the bound is the content
        // area's right EDGE, not its width. The margin is part of the
        // coordinate, not something to subtract from the comparison — an
        // earlier version of this test compared an absolute x against a
        // relative width and reported perfectly well-wrapped text as escaping.
        let margin = 16.0;
        let right_edge = metrics(1.0).logical_size.width as f32 - margin;
        for run in glyph_runs(&scene) {
            for glyph in run.glyphs.iter() {
                assert!(
                    glyph.x >= margin && glyph.x <= right_edge,
                    "a glyph at {} falls outside the content area [{margin}, {right_edge}]; \
                     Parley should have wrapped the diagnostic to fit",
                    glyph.x
                );
            }
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
        let mut text = TextContext::new();
        let scene = SceneBuilder::new().crash_scene(
            &mut text,
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
        let mut text = TextContext::new();
        let scene =
            SceneBuilder::new().crash_scene(&mut text, GenerationId(1), &message, &metrics(1.0));

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
}
