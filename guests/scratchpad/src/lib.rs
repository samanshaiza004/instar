//! Guest-owned editor proof for Phase 3.
//!
//! This is deliberately a small userland policy, not a host editor API. The
//! host only sees input events and the resulting bounded presentation scene.

wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
});

use std::ops::Range;

use instar_editor_core::{Document, Position, Selection, TextEdit};
use instar_ui_protocol::{BatchEncoder, SurfaceEvent, WireLayout, WireSize, flags, opcode};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;
use crate::instar::kernel::surfaces;
use crate::instar::kernel::text_layout_types::{
    Affinity, Alignment, Cursor, FontRole, LayoutStyle, LineHeight,
};
use crate::instar::kernel::text_layouts;

#[derive(Debug, Clone)]
pub struct Scratchpad {
    pub document: Document,
    pub carets: Vec<Selection>,
    pub preedit: Option<String>,
    composition_target: Option<Selection>,
    pub scroll_y: usize,
}

const PRESENTATION_MAX_BYTES: usize = 4096;

impl Scratchpad {
    pub fn new(text: &str) -> Self {
        Self {
            document: Document::from_text(text),
            carets: vec![Selection::at(0)],
            preedit: None,
            composition_target: None,
            scroll_y: 0,
        }
    }

    /// Applies Scratchpad's guest-side commit policy.
    ///
    /// An active composition has one saved primary target. Its commit replaces
    /// that target even when the live selection moved after preedit began.
    /// Without a saved target, ordinary insertion replaces every live caret.
    /// Resulting carets come from editor-core's transaction position map.
    pub fn commit(&mut self, text: &str) -> Result<(), instar_editor_core::EditError> {
        let composition_target = self.composition_target;
        let targets = composition_target.map_or_else(|| self.carets.clone(), |target| vec![target]);
        let edits = targets
            .iter()
            .map(|selection| TextEdit::replace(selection.range(), text))
            .collect();
        let transaction = self.document.apply_transaction(edits)?;
        self.carets = if composition_target.is_some() {
            vec![transaction.map_selection(targets[0])]
        } else {
            targets
                .iter()
                .map(|selection| transaction.map_selection(*selection))
                .collect()
        };
        self.preedit = None;
        self.composition_target = None;
        Ok(())
    }

    /// First non-empty preedit captures the canonical selection. Empty preedit
    /// clears only the visual projection so the following commit can still
    /// replace the saved target.
    pub fn preedit(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.is_empty() && self.composition_target.is_none() {
            self.composition_target = self.carets.first().copied();
            if let Some(target) = self.composition_target {
                // Scratchpad's explicit policy is that composition is owned by
                // the primary caret.  Secondary carets remain a guest-only
                // editing feature, but do not participate in an IME session.
                self.carets = vec![target];
            }
        }
        self.preedit = (!text.is_empty()).then_some(text);
    }

    /// Returns the bounded document window currently intended for
    /// presentation. The real presentation path never materializes the whole
    /// document.
    pub fn visible_projection(&self, rows: usize) -> Result<String, instar_editor_core::EditError> {
        let range = self.visible_range(rows)?;
        self.projected_slice(range)
    }

    fn visible_range(&self, rows: usize) -> Result<Range<usize>, instar_editor_core::EditError> {
        let line_count = self.document.len_lines();
        if line_count == 0 {
            return Ok(0..0);
        }
        let first_line = self.scroll_y.min(line_count.saturating_sub(1));
        let last_line = first_line
            .saturating_add(rows.max(1).saturating_sub(1))
            .min(line_count.saturating_sub(1));
        let start = self.document.line_range(first_line)?.start;
        let line_end = self.document.line_range(last_line)?.end;
        let mut end = line_end.min(start + PRESENTATION_MAX_BYTES);
        while end > start && !self.document.is_char_boundary(end) {
            end -= 1;
        }
        Ok(start..end)
    }

    /// Projects the current preedit over one bounded document range. The
    /// caller supplies a range from the pre-transaction document; only that
    /// range and the preedit are copied.
    pub fn projected_slice(
        &self,
        range: Range<usize>,
    ) -> Result<String, instar_editor_core::EditError> {
        let Some(preedit) = &self.preedit else {
            return self.document.slice(range);
        };
        let Some(target) = self.composition_target else {
            return self.document.slice(range);
        };

        let target = target.range();
        if target.start > range.end || target.end < range.start {
            return self.document.slice(range);
        }

        let mut projected = String::with_capacity(range.len() + preedit.len());
        let prefix_end = target.start.min(range.end);
        if range.start < prefix_end {
            projected.push_str(&self.document.slice(range.start..prefix_end)?);
        }
        projected.push_str(preedit);
        let suffix_start = target.end.max(range.start);
        if suffix_start < range.end {
            projected.push_str(&self.document.slice(suffix_start..range.end)?);
        }
        Ok(projected)
    }

    /// Whole-document convenience retained for tests/debugging only. The
    /// component presentation path uses [`Self::visible_projection`].
    #[cfg(any(test, debug_assertions))]
    pub fn projected_text(&self) -> Result<String, instar_editor_core::EditError> {
        self.projected_slice(0..self.document.len_bytes())
    }

    pub fn primary_position(&self) -> Position {
        self.carets.first().map_or(Position::at(0), |s| s.head)
    }

    fn layout_style() -> LayoutStyle {
        LayoutStyle {
            role: FontRole::Monospace,
            size: 14.0,
            weight: 400,
            wrap: false,
            line_height: LineHeight::FontSizeRelative(1.4),
            width: None,
            alignment: Alignment::Start,
        }
    }

    fn primary_line(&self) -> Result<(usize, usize, String), instar_editor_core::EditError> {
        let byte = self.primary_position().byte;
        let line = self.document.line_of_byte(byte)?;
        let range = self.document.line_range(line)?;
        Ok((line, range.start, self.document.slice(range)?))
    }

    fn delete_backward(&mut self) -> Result<(), instar_editor_core::EditError> {
        let selection = self.carets[0];
        if !selection.is_empty() {
            return self.commit("");
        }
        let end = selection.head.byte;
        let start = self.document.previous_grapheme_boundary(end)?;
        if start != end {
            self.document.apply(TextEdit::delete(start..end))?;
            self.carets[0] = Selection::at(start);
        }
        Ok(())
    }

    fn delete_forward(&mut self) -> Result<(), instar_editor_core::EditError> {
        let selection = self.carets[0];
        if !selection.is_empty() {
            return self.commit("");
        }
        let start = selection.head.byte;
        let end = self.document.next_grapheme_boundary(start)?;
        if start != end {
            self.document.apply(TextEdit::delete(start..end))?;
        }
        Ok(())
    }

    fn ctrl_k(&mut self) -> Result<(), instar_editor_core::EditError> {
        let (_, line_start, line) = self.primary_line()?;
        let end = line_start + line.trim_end_matches(['\r', '\n']).len();
        let start = self.primary_position().byte;
        if start < end {
            self.document.apply(TextEdit::delete(start..end))?;
        }
        Ok(())
    }

    fn scroll(&mut self, dy: f64) {
        let lines = self.document.len_lines().saturating_sub(26);
        if dy > 0.0 {
            self.scroll_y = (self.scroll_y + dy.ceil() as usize).min(lines);
        } else if dy < 0.0 {
            self.scroll_y = self.scroll_y.saturating_sub((-dy).ceil() as usize);
        }
    }

    fn move_to(&mut self, cursor: Cursor, line_start: usize) {
        self.carets[0] = Selection::at(line_start + cursor.byte_index as usize);
    }

    fn move_visual(&mut self, right: bool) -> Result<(), String> {
        let (_, line_start, line) = self.primary_line().map_err(|error| error.to_string())?;
        let layout = text_layouts::create_layout(&line, Self::layout_style())
            .map_err(|error| format!("navigation layout failed: {error:?}"))?;
        let cursor = Cursor {
            byte_index: (self.primary_position().byte - line_start) as u32,
            affinity: Affinity::Downstream,
        };
        let next = if right {
            layout.next_visual(cursor)
        } else {
            layout.previous_visual(cursor)
        }
        .map_err(|error| format!("navigation failed: {error:?}"))?;
        self.move_to(next, line_start);
        Ok(())
    }

    fn move_edge(&mut self, end: bool) -> Result<(), String> {
        let (_, line_start, line) = self.primary_line().map_err(|error| error.to_string())?;
        let layout = text_layouts::create_layout(&line, Self::layout_style())
            .map_err(|error| format!("edge layout failed: {error:?}"))?;
        let cursor = Cursor {
            byte_index: (self.primary_position().byte - line_start) as u32,
            affinity: Affinity::Downstream,
        };
        let edge = if end {
            layout.hard_line_end(cursor)
        } else {
            layout.hard_line_start(cursor)
        }
        .map_err(|error| format!("edge query failed: {error:?}"))?;
        self.move_to(edge, line_start);
        Ok(())
    }

    async fn pointer(&mut self, x: f64, y: f64) -> Result<(), String> {
        let line = self.scroll_y + (y.max(0.0) / 20.0).floor() as usize;
        if line >= self.document.len_lines() {
            return Ok(());
        }
        let range = self
            .document
            .line_range(line)
            .map_err(|error| error.to_string())?;
        let text = self
            .document
            .slice(range.clone())
            .map_err(|error| error.to_string())?;
        let layout = text_layouts::create_layout(&text, Self::layout_style())
            .map_err(|error| format!("pointer layout failed: {error:?}"))?;
        let cursor = layout
            .cursor_from_point(x as f32, 0.0)
            .map_err(|error| format!("pointer query failed: {error:?}"))?;
        self.carets[0] = Selection::at(range.start + cursor.byte_index as usize);
        Ok(())
    }
}

const SURFACE: instar_ui_protocol::NodeKey = instar_ui_protocol::NodeKey::first(7);

/// The actual component-facing loop. The policy object above remains
/// independently testable; this adapter owns only public WIT handles and the
/// bounded presentation bridge.
struct GuestComponent;

impl GuestComponent {
    async fn commit_surface_tree() -> Result<(), String> {
        let mut encoder = BatchEncoder::new();
        let surface_layout = WireLayout {
            width: WireSize::Fixed(640),
            height: WireSize::Fixed(480),
            ..WireLayout::default()
        };
        encoder
            .node(
                opcode::NODE_ROOT,
                instar_ui_protocol::NodeKey::first(0),
                0,
                None,
                surface_layout,
                1,
            )
            .node(
                opcode::NODE_SURFACE,
                SURFACE,
                flags::SURFACE_FOCUSABLE
                    | flags::SURFACE_POINTER_BUTTONS
                    | flags::SURFACE_POINTER_MOVEMENT
                    | flags::SURFACE_WHEEL
                    | flags::SURFACE_RAW_KEYS
                    | flags::SURFACE_FOCUS
                    | flags::SURFACE_TEXT_INPUT,
                None,
                surface_layout,
                0,
            );
        kernel_ui::commit(encoder.finish())
            .await
            .map(|_| ())
            .map_err(|error| format!("surface tree commit failed: {error:?}"))
    }

    async fn present(app: &Scratchpad) -> Result<(), String> {
        let style = Scratchpad::layout_style();
        const VIEWPORT_ROWS: usize = 26;
        let mut encoder = instar_surface_protocol::Encoder::new();
        encoder.command(instar_surface_protocol::Command::FillRect {
            rect: instar_surface_protocol::Rect::new(0.0, 0.0, 640.0, 480.0),
            color: instar_surface_protocol::Color::rgba(20, 20, 24, 255),
        });
        let range = app
            .visible_range(VIEWPORT_ROWS)
            .map_err(|error| format!("visible range failed: {error}"))?;
        let projected = app
            .projected_slice(range.clone())
            .map_err(|error| format!("visible projection failed: {error}"))?;
        let layout = text_layouts::create_layout(&projected, style)
            .map_err(|error| format!("layout creation failed: {error:?}"))?;
        let row_height = layout
            .metrics()
            .map_err(|error| format!("layout metrics failed: {error:?}"))?
            .height;
        let local = app
            .primary_position()
            .byte
            .saturating_sub(range.start)
            .min(projected.len());
        let caret = layout
            .caret_rect(
                Cursor {
                    byte_index: local as u32,
                    affinity: Affinity::Downstream,
                },
                1.0,
            )
            .map_err(|error| format!("caret geometry failed: {error:?}"))?;
        encoder.command(instar_surface_protocol::Command::FillRect {
            rect: instar_surface_protocol::Rect::new(
                8.0 + caret.x,
                24.0 + caret.y,
                caret.width.max(1.0),
                caret.height.max(1.0),
            ),
            color: instar_surface_protocol::Color::rgba(120, 190, 255, 220),
        });
        encoder.command(instar_surface_protocol::Command::DrawTextLayout {
            layout_slot: 0,
            x: 8.0,
            y: 24.0,
            color: instar_surface_protocol::Color::rgba(240, 240, 240, 255),
        });
        let scene = encoder.finish().map_err(|error| error.to_string())?;
        surfaces::update_surface(
            instar::kernel::surface_types::NodeKey {
                id: SURFACE.id,
                generation: SURFACE.generation,
            },
            scene,
            vec![&layout],
        )
        .await
        .map_err(|error| format!("surface update failed: {error:?}"))?;
        let _ = surfaces::configure_text_input(
            instar::kernel::surface_types::NodeKey {
                id: SURFACE.id,
                generation: SURFACE.generation,
            },
            true,
            instar::kernel::surface_types::LocalRect {
                x: 8.0 + caret.x,
                y: 24.0 + caret.y,
                width: caret.width.max(1.0),
                height: caret.height.max(row_height),
            },
        );
        Ok(())
    }
}

impl Guest for GuestComponent {
    async fn run() -> Result<(), String> {
        let mut app = Scratchpad::new("");
        GuestComponent::commit_surface_tree().await?;
        GuestComponent::present(&app).await?;
        loop {
            let payload = match kernel_runtime::next_event().await {
                Ok(payload) => payload,
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            };
            let event = SurfaceEvent::decode(&payload)
                .map_err(|error| format!("undecodable Surface event: {error}"))?;
            match event {
                SurfaceEvent::Key {
                    logical,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let control = modifiers & (1 << 1) != 0;
                    match logical {
                        5 => app.move_visual(false)?,
                        6 => app.move_visual(true)?,
                        9 => app.move_edge(false)?,
                        10 => app.move_edge(true)?,
                        11 => app.delete_backward().map_err(|error| error.to_string())?,
                        12 => app.delete_forward().map_err(|error| error.to_string())?,
                        value if control && value == 'k' as u16 => {
                            app.ctrl_k().map_err(|error| error.to_string())?
                        }
                        value if control && value == 'z' as u16 => {
                            app.document.undo().map_err(|error| error.to_string())?;
                        }
                        value if control && value == 'y' as u16 => {
                            app.document.redo().map_err(|error| error.to_string())?;
                        }
                        value if !control && value >= 32 => {
                            if let Some(character) = char::from_u32(value as u32) {
                                app.commit(&character.to_string())
                                    .map_err(|error| error.to_string())?;
                            }
                        }
                        _ => {}
                    }
                }
                SurfaceEvent::PointerDown { x, y, .. }
                | SurfaceEvent::PointerUp { x, y, .. }
                | SurfaceEvent::PointerMove { x, y, .. } => app.pointer(x, y).await?,
                SurfaceEvent::Wheel { dy, .. } => app.scroll(dy),
                SurfaceEvent::ImeCommit { text, .. } => {
                    app.commit(&text).map_err(|error| error.to_string())?
                }
                SurfaceEvent::ImePreedit { text, .. } => app.preedit(text),
                _ => {}
            }
            GuestComponent::present(&app).await?;
        }
    }
}

export!(GuestComponent);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_carets_are_guest_policy() {
        let mut app = Scratchpad::new("abc");
        app.carets = vec![Selection::at(1), Selection::at(3)];
        app.commit("X").unwrap();
        assert_eq!(app.document.as_string(), "aXbcX");
        // Kills mutant: map every caret from the original document without
        // accounting for the insertion before the second caret.
        assert_eq!(app.carets, vec![Selection::at(2), Selection::at(5)]);
    }

    #[test]
    fn empty_preedit_keeps_target_when_live_selection_moves() {
        let mut app = Scratchpad::new("hello world");
        app.carets = vec![Selection {
            anchor: Position::at(0),
            head: Position::at(5),
        }];
        app.preedit("a\nb");
        assert_eq!(app.projected_slice(0..11).unwrap(), "a\nb world");
        app.preedit("");
        // Kills mutant: empty preedit clears the saved composition target.
        assert!(app.composition_target.is_some());
        app.carets = vec![Selection::at(6)];
        app.commit("a\nb").unwrap();
        // Kills mutant: commit follows the moved live caret instead of A.
        assert_eq!(app.document.as_string(), "a\nb world");
        assert_eq!(app.carets, vec![Selection::at(3)]);
    }

    #[test]
    fn composition_commit_uses_saved_target_not_live_caret() {
        let mut app = Scratchpad::new("abcde");
        app.carets = vec![Selection::at(1)];
        app.preedit("X");
        app.carets = vec![Selection::at(4)];
        app.commit("Y").unwrap();
        // Kills mutant: composition commit replaces the current caret.
        assert_eq!(app.document.as_string(), "aYbcde");
        assert_eq!(app.carets, vec![Selection::at(2)]);
    }

    #[test]
    fn composition_commit_targets_one_primary_caret() {
        let mut app = Scratchpad::new("abc");
        app.carets = vec![Selection::at(1), Selection::at(3)];
        app.preedit("X");
        app.commit("X").unwrap();
        // Kills mutant: one IME composition is replicated to every caret.
        assert_eq!(app.document.as_string(), "aXbc");
        assert_eq!(app.carets, vec![Selection::at(2)]);
    }

    #[test]
    fn visible_projection_is_bounded() {
        let source = "x".repeat(PRESENTATION_MAX_BYTES * 2);
        let app = Scratchpad::new(&source);
        let visible = app.visible_projection(26).unwrap();
        // Kills mutant: visible projection calls as_string() and returns the
        // whole document to the real presentation path.
        assert_eq!(visible.len(), PRESENTATION_MAX_BYTES);
        assert!(visible.chars().all(|character| character == 'x'));
    }
}
