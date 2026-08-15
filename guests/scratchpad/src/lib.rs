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
    preedit_cursor: Option<(usize, usize)>,
    composition_target: Option<Selection>,
    pointer_anchor: Option<Position>,
    pub scroll_y: usize,
}

const PRESENTATION_MAX_BYTES: usize = 4096;

impl Scratchpad {
    pub fn new(text: &str) -> Self {
        Self {
            document: Document::from_text(text),
            carets: vec![Selection::at(0)],
            preedit: None,
            preedit_cursor: None,
            composition_target: None,
            pointer_anchor: None,
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
        self.preedit_cursor = None;
        self.composition_target = None;
        Ok(())
    }

    /// First non-empty preedit captures the canonical selection. Empty preedit
    /// clears only the visual projection so the following commit can still
    /// replace the saved target.
    pub fn preedit(&mut self, text: impl Into<String>) {
        self.preedit_with_cursor(text, None);
    }

    pub fn preedit_with_cursor(&mut self, text: impl Into<String>, cursor: Option<(usize, usize)>) {
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
        self.preedit_cursor = self.preedit.as_deref().and_then(|text| {
            cursor.filter(|(start, end)| {
                start <= end
                    && *end <= text.len()
                    && text.is_char_boundary(*start)
                    && text.is_char_boundary(*end)
            })
        });
    }

    fn cancel_composition(&mut self) {
        self.preedit = None;
        self.preedit_cursor = None;
        self.composition_target = None;
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

    fn visible_rows(&self, rows: usize) -> Result<Vec<String>, instar_editor_core::EditError> {
        let count = self.document.len_lines();
        if count == 0 {
            return Ok(vec![String::new()]);
        }
        let first = self.scroll_y.min(count - 1);
        (first..first.saturating_add(rows).min(count))
            .map(|line| Ok(self.bounded_line(line)?.1))
            .collect()
    }

    /// Builds only the bounded projected viewport. Preedit bytes are never
    /// written to Crop; the resulting hard rows are shaped independently by
    /// the same host TextLayout service used for canonical rows.
    fn visible_rows_projected(
        &self,
        rows: usize,
    ) -> Result<Vec<String>, instar_editor_core::EditError> {
        let range = self.visible_range(rows)?;
        if self.preedit.is_none() || self.composition_target.is_none() {
            return self.visible_rows(rows);
        }
        let projected = self.projected_slice_bounded(range, rows)?;
        Ok(projected
            .split_inclusive('\n')
            .map(ToOwned::to_owned)
            .take(rows.max(1))
            .collect())
    }

    fn projected_slice_bounded(
        &self,
        range: Range<usize>,
        rows: usize,
    ) -> Result<String, instar_editor_core::EditError> {
        let Some(preedit) = &self.preedit else {
            return self.document.slice(range);
        };
        let Some(target) = self.composition_target else {
            return self.document.slice(range);
        };
        if target.range().start > range.end || target.range().end < range.start {
            return self.document.slice(range);
        }

        // Keep transient projection bounded even when an IME supplies
        // thousands of lines. The guest may scan the supplied preedit, but it
        // never allocates the full projection merely to shape the viewport.
        let limit = rows
            .max(1)
            .saturating_mul(PRESENTATION_MAX_BYTES.saturating_add(1));
        let mut projected =
            String::with_capacity(limit.min(range.len().saturating_add(preedit.len())));
        let target = target.range();
        let prefix_end = target.start.min(range.end);
        if range.start < prefix_end {
            projected.push_str(&self.document.slice(range.start..prefix_end)?);
        }
        let remaining = limit.saturating_sub(projected.len());
        let mut preedit_end = preedit.len().min(remaining);
        while preedit_end > 0 && !preedit.is_char_boundary(preedit_end) {
            preedit_end -= 1;
        }
        projected.push_str(&preedit[..preedit_end]);
        if preedit_end == preedit.len() {
            let suffix_start = target.end.max(range.start);
            if suffix_start < range.end && projected.len() < limit {
                let suffix = self.document.slice(suffix_start..range.end)?;
                let suffix_end = suffix.len().min(limit - projected.len());
                projected.push_str(&suffix[..suffix_end]);
            }
        }
        Ok(projected)
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

    /// Returns the cursor that belongs in the projected presentation.
    ///
    /// During composition the active end of the IME cursor is relative to the
    /// transient preedit, not to the canonical document. Mapping it onto the
    /// document line that owns the composition target keeps the drawn caret
    /// and the native candidate rectangle at the same visual position.
    fn visual_cursor(&self) -> Result<(usize, usize), instar_editor_core::EditError> {
        if let (Some(preedit), Some(target), Some((_, end))) = (
            self.preedit.as_deref(),
            self.composition_target,
            self.preedit_cursor,
        ) && let Some(prefix) = preedit.get(..end)
        {
            let target = target.range();
            if self.document.len_lines() == 0 {
                if let Some(newline) = prefix.rfind('\n') {
                    let line = prefix[..=newline]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count();
                    return Ok((line, prefix.len() - newline - 1));
                }
                return Ok((0, prefix.len()));
            }
            let target_line = self.document.line_of_byte(target.start)?;
            let target_line_start = self.document.line_range(target_line)?.start;
            if let Some(newline) = prefix.rfind('\n') {
                let line = target_line
                    + prefix[..=newline]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count();
                return Ok((line, prefix.len() - newline - 1));
            }
            return Ok((target_line, target.start - target_line_start + prefix.len()));
        }

        if self.document.len_lines() == 0 {
            return Ok((0, 0));
        }

        let position = self.primary_position();
        let line = self.document.line_of_byte(position.byte)?;
        let line_start = self.document.line_range(line)?.start;
        Ok((line, position.byte.saturating_sub(line_start)))
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
        let (start, text) = self.bounded_line(line)?;
        Ok((line, start, text))
    }

    fn bounded_line(&self, line: usize) -> Result<(usize, String), instar_editor_core::EditError> {
        let range = self.document.line_range(line)?;
        let mut end = range.start + range.len().min(PRESENTATION_MAX_BYTES);
        while end > range.start && !self.document.is_char_boundary(end) {
            end -= 1;
        }
        Ok((range.start, self.document.slice(range.start..end)?))
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

    fn move_visual(
        &mut self,
        right: bool,
        extend: bool,
        presentation: &Presentation,
    ) -> Result<(), String> {
        let (line_number, line_start, line) =
            self.primary_line().map_err(|error| error.to_string())?;
        let layout = presentation
            .row_for_line(line_number)
            .map(|row| &row.layout);
        let owned_layout = if layout.is_none() {
            Some(
                text_layouts::create_layout(&line, Self::layout_style())
                    .map_err(|error| format!("navigation layout failed: {error:?}"))?,
            )
        } else {
            None
        };
        let layout = layout.or(owned_layout.as_ref());
        let cursor = Cursor {
            byte_index: (self.primary_position().byte - line_start) as u32,
            affinity: Affinity::Downstream,
        };
        let next = if right {
            layout.unwrap().next_visual(cursor)
        } else {
            layout.unwrap().previous_visual(cursor)
        }
        .map_err(|error| format!("navigation failed: {error:?}"))?;
        let current = self.primary_position().byte;
        let mut position = line_start + next.byte_index as usize;
        if position == current {
            if right && line_number + 1 < self.document.len_lines() {
                let (next_start, next_line) = self
                    .bounded_line(line_number + 1)
                    .map_err(|error| error.to_string())?;
                let next_layout = presentation
                    .row_for_line(line_number + 1)
                    .map(|row| &row.layout);
                let owned_next = if next_layout.is_none() {
                    Some(
                        text_layouts::create_layout(&next_line, Self::layout_style())
                            .map_err(|error| format!("navigation layout failed: {error:?}"))?,
                    )
                } else {
                    None
                };
                let next_layout = next_layout.or(owned_next.as_ref()).unwrap();
                position = next_start
                    + next_layout
                        .hard_line_start(Cursor {
                            byte_index: 0,
                            affinity: Affinity::Downstream,
                        })
                        .map_err(|error| format!("row transition failed: {error:?}"))?
                        .byte_index as usize;
            } else if !right && line_number > 0 {
                let (previous_start, previous_line) = self
                    .bounded_line(line_number - 1)
                    .map_err(|error| error.to_string())?;
                let previous_layout = presentation
                    .row_for_line(line_number - 1)
                    .map(|row| &row.layout);
                let owned_previous = if previous_layout.is_none() {
                    Some(
                        text_layouts::create_layout(&previous_line, Self::layout_style())
                            .map_err(|error| format!("navigation layout failed: {error:?}"))?,
                    )
                } else {
                    None
                };
                let previous_layout = previous_layout.or(owned_previous.as_ref()).unwrap();
                position = previous_start
                    + previous_layout
                        .hard_line_end(Cursor {
                            byte_index: previous_line.len() as u32,
                            affinity: Affinity::Downstream,
                        })
                        .map_err(|error| format!("row transition failed: {error:?}"))?
                        .byte_index as usize;
            }
        }
        let next_position = Position::at(position);
        self.carets[0] = if extend {
            self.carets[0].extend_to(next_position)
        } else if self.carets[0].is_empty() {
            Selection::at(position)
        } else {
            Selection::at(if right {
                self.carets[0].range().end
            } else {
                self.carets[0].range().start
            })
        };
        Ok(())
    }

    fn move_edge(
        &mut self,
        end: bool,
        extend: bool,
        presentation: &Presentation,
    ) -> Result<(), String> {
        let (line_number, line_start, line) =
            self.primary_line().map_err(|error| error.to_string())?;
        let owned_layout = if presentation.row_for_line(line_number).is_none() {
            Some(
                text_layouts::create_layout(&line, Self::layout_style())
                    .map_err(|error| format!("edge layout failed: {error:?}"))?,
            )
        } else {
            None
        };
        let layout = presentation
            .row_for_line(line_number)
            .map(|row| &row.layout)
            .or(owned_layout.as_ref())
            .unwrap();
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
        let position = Position::at(line_start + edge.byte_index as usize);
        self.carets[0] = if extend {
            self.carets[0].extend_to(position)
        } else {
            Selection::at(position.byte)
        };
        Ok(())
    }
}

const SURFACE: instar_ui_protocol::NodeKey = instar_ui_protocol::NodeKey::first(7);

fn surface_key() -> instar::kernel::surface_types::NodeKey {
    instar::kernel::surface_types::NodeKey {
        id: SURFACE.id,
        generation: SURFACE.generation,
    }
}

struct PresentedRow {
    line: usize,
    start: usize,
    text: String,
    layout: text_layouts::TextLayout,
}

struct Presentation {
    rows: Vec<PresentedRow>,
    row_height: f32,
}

impl Presentation {
    fn row_for_line(&self, line: usize) -> Option<&PresentedRow> {
        self.rows.iter().find(|row| row.line == line)
    }
}

#[derive(Clone, Copy)]
enum PointerPhase {
    Down,
    Move,
    Up,
}

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

    async fn present(
        app: &Scratchpad,
        presentation: &mut Option<Presentation>,
    ) -> Result<(), String> {
        let style = Scratchpad::layout_style();
        const VIEWPORT_ROWS: usize = 26;
        const OVERSCAN_ROWS: usize = 2;
        let mut encoder = instar_surface_protocol::Encoder::new();
        encoder.command(instar_surface_protocol::Command::FillRect {
            rect: instar_surface_protocol::Rect::new(0.0, 0.0, 640.0, 480.0),
            color: instar_surface_protocol::Color::rgba(20, 20, 24, 255),
        });
        // A tiny guest-owned proof marker makes the joined pixel test
        // independent of font rasterization: it is derived from the guest's
        // canonical document and changes only after a real edit.
        encoder.command(instar_surface_protocol::Command::FillRect {
            rect: instar_surface_protocol::Rect::new(612.0, 448.0, 16.0, 16.0),
            color: if app.pointer_anchor.is_some()
                || !app
                    .carets
                    .first()
                    .is_some_and(|selection| selection.is_empty())
            {
                instar_surface_protocol::Color::rgba(240, 170, 70, 255)
            } else if app.document.is_empty() {
                instar_surface_protocol::Color::rgba(90, 90, 100, 255)
            } else {
                instar_surface_protocol::Color::rgba(80, 210, 120, 255)
            },
        });
        let rows = app
            .visible_rows_projected(VIEWPORT_ROWS + OVERSCAN_ROWS)
            .map_err(|error| format!("visible row extraction failed: {error}"))?;
        let first_line = app.scroll_y.min(app.document.len_lines().saturating_sub(1));
        // Rows this call can reuse instead of re-shaping. Every prior call
        // shaped every visible row unconditionally -- fine for a document
        // small enough that few rows exist at all, but once a document has
        // VIEWPORT_ROWS + OVERSCAN_ROWS (28) or more lines, every single
        // keystroke re-shaped all 28 of them from scratch regardless of
        // which row (usually exactly one) actually changed. That, not a
        // document-size scan anywhere in the line-lookup path (`Document`'s
        // `line_of_byte`/`line_range` are Crop-native O(log n) calls), is
        // what blew the p95 <= 5 ms typing budget for a large document --
        // see benchmarks/text-latency/README.md and
        // docs/PHASE-3.md's "Latency gate" section for the measurements
        // that found this. A row is reusable exactly when its line number
        // and its bounded text are unchanged since the last presentation;
        // `take()` moves the old rows out so their `TextLayout` handles can
        // be relocated into `presented_rows` below instead of being dropped
        // and immediately re-created.
        let mut previous_rows = presentation.take().map_or_else(Vec::new, |p| p.rows);
        let mut presented_rows = Vec::with_capacity(rows.len());
        let mut row_height = 20.0_f32;
        let mut candidate = instar::kernel::surface_types::LocalRect {
            x: 8.0,
            y: 24.0,
            width: 1.0,
            height: row_height,
        };
        let (visual_line, visual_byte) = app
            .visual_cursor()
            .map_err(|error| format!("visual cursor lookup failed: {error:?}"))?;
        for (slot, row) in rows.iter().enumerate() {
            let mut end = row.len().min(PRESENTATION_MAX_BYTES);
            while end > 0 && !row.is_char_boundary(end) {
                end -= 1;
            }
            let bounded = &row[..end];
            let line = first_line + slot;
            let reusable = previous_rows
                .iter()
                .position(|previous| previous.line == line && previous.text == bounded);
            let layout = if let Some(index) = reusable {
                previous_rows.remove(index).layout
            } else {
                text_layouts::create_layout(bounded, style)
                    .map_err(|error| format!("layout creation failed: {error:?}"))?
            };
            if slot == 0 {
                row_height = layout
                    .metrics()
                    .map_err(|error| format!("layout metrics failed: {error:?}"))?
                    .height;
                candidate.height = row_height;
            }
            let row_start = app
                .document
                .line_range(line.min(app.document.len_lines().saturating_sub(1)))
                .map(|range| range.start)
                .unwrap_or(0);
            for selection in &app.carets {
                let selection_range = selection.range();
                let row_end = row_start + bounded.len();
                if selection_range.start < row_end && selection_range.end > row_start {
                    let anchor = selection.anchor.byte.clamp(row_start, row_end) - row_start;
                    let head = selection.head.byte.clamp(row_start, row_end) - row_start;
                    let rects = layout
                        .selection_rects(
                            Cursor {
                                byte_index: anchor as u32,
                                affinity: Affinity::Downstream,
                            },
                            Cursor {
                                byte_index: head as u32,
                                affinity: Affinity::Downstream,
                            },
                        )
                        .map_err(|error| format!("selection geometry failed: {error:?}"))?;
                    for rect in rects {
                        encoder.command(instar_surface_protocol::Command::FillRect {
                            rect: instar_surface_protocol::Rect::new(
                                8.0 + rect.x,
                                24.0 + slot as f32 * row_height + rect.y,
                                rect.width,
                                rect.height,
                            ),
                            color: instar_surface_protocol::Color::rgba(55, 85, 125, 220),
                        });
                    }
                }
            }
            if visual_line == line {
                let local = visual_byte.min(bounded.len());
                let caret = layout
                    .caret_rect(
                        Cursor {
                            byte_index: local as u32,
                            affinity: Affinity::Downstream,
                        },
                        1.0,
                    )
                    .map_err(|error| format!("caret geometry failed: {error:?}"))?;
                let x = 8.0 + caret.x;
                let y = 24.0 + slot as f32 * row_height + caret.y;
                encoder.command(instar_surface_protocol::Command::FillRect {
                    rect: instar_surface_protocol::Rect::new(
                        x,
                        y,
                        caret.width.max(1.0),
                        caret.height.max(1.0),
                    ),
                    color: instar_surface_protocol::Color::rgba(120, 190, 255, 220),
                });
                candidate = instar::kernel::surface_types::LocalRect {
                    x,
                    y,
                    width: caret.width.max(1.0),
                    height: caret.height.max(1.0),
                };
            }
            for caret_selection in app.carets.iter().skip(1) {
                if caret_selection.is_empty() && caret_selection.head.byte >= row_start {
                    let local = caret_selection
                        .head
                        .byte
                        .saturating_sub(row_start)
                        .min(bounded.len());
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
                            24.0 + slot as f32 * row_height + caret.y,
                            caret.width.max(1.0),
                            caret.height.max(1.0),
                        ),
                        color: instar_surface_protocol::Color::rgba(120, 190, 255, 220),
                    });
                }
            }
            encoder.command(instar_surface_protocol::Command::DrawTextLayout {
                layout_slot: slot as u16,
                x: 8.0,
                y: 24.0 + slot as f32 * row_height,
                color: instar_surface_protocol::Color::rgba(240, 240, 240, 255),
            });
            presented_rows.push(PresentedRow {
                line,
                start: row_start,
                text: bounded.to_owned(),
                layout,
            });
        }
        let scene = encoder.finish().map_err(|error| error.to_string())?;
        surfaces::update_surface(
            instar::kernel::surface_types::NodeKey {
                id: SURFACE.id,
                generation: SURFACE.generation,
            },
            scene,
            presented_rows.iter().map(|row| &row.layout).collect(),
        )
        .await
        .map_err(|error| format!("surface update failed: {error:?}"))?;
        let _ = surfaces::configure_text_input(
            instar::kernel::surface_types::NodeKey {
                id: SURFACE.id,
                generation: SURFACE.generation,
            },
            true,
            candidate,
        );
        *presentation = Some(Presentation {
            rows: presented_rows,
            row_height,
        });
        Ok(())
    }

    async fn pointer(
        app: &mut Scratchpad,
        presentation: &mut Option<Presentation>,
        phase: PointerPhase,
        x: f64,
        y: f64,
    ) -> Result<(), String> {
        if app.preedit.is_some() || app.composition_target.is_some() {
            app.cancel_composition();
            Self::present(app, presentation).await?;
        }
        let Some(presentation) = presentation.as_ref() else {
            return Ok(());
        };
        let slot = ((y - 24.0).max(0.0) / f64::from(presentation.row_height)).floor() as usize;
        let slot = slot.min(presentation.rows.len().saturating_sub(1));
        let Some(row) = presentation.rows.get(slot) else {
            return Ok(());
        };
        let cursor = row
            .layout
            .cursor_from_point(
                (x - 8.0) as f32,
                (y - 24.0 - slot as f64 * f64::from(presentation.row_height)) as f32,
            )
            .map_err(|error| format!("pointer query failed: {error:?}"))?;
        let mut cursor = cursor;
        // Some platforms report a point above the first glyph's ink box as
        // the row start. Preserve TextLayout as the authority while walking
        // its visual carets to recover the horizontal hit for that row.
        if cursor.byte_index == 0 && x > 8.0 {
            let point_x = (x - 8.0) as f32;
            let mut probe = cursor;
            for _ in 0..row.text.len().min(4096) {
                let next = row
                    .layout
                    .next_visual(probe)
                    .map_err(|error| format!("pointer navigation failed: {error:?}"))?;
                if next.byte_index == probe.byte_index && next.affinity == probe.affinity {
                    break;
                }
                let caret = row
                    .layout
                    .caret_rect(next, 1.0)
                    .map_err(|error| format!("pointer caret failed: {error:?}"))?;
                if caret.x <= point_x {
                    cursor = next;
                    probe = next;
                } else {
                    break;
                }
            }
        }
        let byte = row.start + (cursor.byte_index as usize).min(row.text.len());
        let position = Position::at(byte);
        match phase {
            PointerPhase::Down => {
                app.pointer_anchor = Some(position);
                app.carets[0] = Selection::at(byte);
                surfaces::capture_pointer(surface_key())
                    .map_err(|error| format!("pointer capture failed: {error:?}"))?;
            }
            PointerPhase::Move => {
                if let Some(anchor) = app.pointer_anchor {
                    app.carets[0] = Selection {
                        anchor,
                        head: position,
                    };
                }
            }
            PointerPhase::Up => {
                if let Some(anchor) = app.pointer_anchor {
                    app.carets[0] = Selection {
                        anchor,
                        head: position,
                    };
                }
                app.pointer_anchor = None;
                let _ = surfaces::release_pointer(surface_key());
            }
        }
        Ok(())
    }
}

impl Guest for GuestComponent {
    async fn run() -> Result<(), String> {
        let mut app = Scratchpad::new("");
        let mut presentation = None;
        GuestComponent::commit_surface_tree().await?;
        GuestComponent::present(&app, &mut presentation).await?;
        loop {
            let payload = match kernel_runtime::next_event().await {
                Ok(payload) => payload,
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            };
            let event = SurfaceEvent::decode(&payload)
                .map_err(|error| format!("undecodable Surface event: {error}"))?;
            // `present()` used to run unconditionally after every event,
            // including ones that provably changed nothing visible: a key
            // release (there is no `pressed: false` arm below, so it always
            // fell to the wildcard), a pointer move with no drag in progress
            // and no composition to cancel, `Focus { focused: true }`,
            // `ImeEnabled`, and `Metrics` all reached the trailing call with
            // `app` untouched. Each arm below opts in to `needs_present`
            // explicitly instead, so the default is "nothing changed, don't
            // repaint" rather than "repaint unless proven unnecessary".
            let mut needs_present = false;
            match event {
                SurfaceEvent::Key {
                    logical,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let control = modifiers & (1 << 1) != 0;
                    let extend = modifiers & 1 != 0;
                    match logical {
                        5 => app.move_visual(false, extend, presentation.as_ref().unwrap())?,
                        6 => app.move_visual(true, extend, presentation.as_ref().unwrap())?,
                        9 => app.move_edge(false, extend, presentation.as_ref().unwrap())?,
                        10 => app.move_edge(true, extend, presentation.as_ref().unwrap())?,
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
                    needs_present = true;
                }
                SurfaceEvent::PointerDown { x, y, .. } => {
                    GuestComponent::pointer(&mut app, &mut presentation, PointerPhase::Down, x, y)
                        .await?;
                    needs_present = true;
                }
                SurfaceEvent::PointerUp { x, y, .. } => {
                    GuestComponent::pointer(&mut app, &mut presentation, PointerPhase::Up, x, y)
                        .await?;
                    needs_present = true;
                }
                SurfaceEvent::PointerMove { x, y, .. } => {
                    // `pointer()`'s own body only changes anything for `Move`
                    // when a drag is in progress, and its leading composition
                    // check only fires when there is a composition to cancel
                    // -- so a hover move that is neither is not just an
                    // unnecessary `present()`, it is an unnecessary host call
                    // to hit-test a point whose result nothing will use.
                    if app.pointer_anchor.is_some()
                        || app.preedit.is_some()
                        || app.composition_target.is_some()
                    {
                        GuestComponent::pointer(
                            &mut app,
                            &mut presentation,
                            PointerPhase::Move,
                            x,
                            y,
                        )
                        .await?;
                        needs_present = true;
                    }
                }
                SurfaceEvent::Wheel { dy, .. } => {
                    app.scroll(dy);
                    needs_present = true;
                }
                SurfaceEvent::ImeCommit { text, .. } => {
                    app.commit(&text).map_err(|error| error.to_string())?;
                    needs_present = true;
                }
                SurfaceEvent::ImePreedit { text, cursor, .. } => {
                    app.preedit_with_cursor(
                        text,
                        cursor.map(|(start, end)| (start as usize, end as usize)),
                    );
                    needs_present = true;
                }
                SurfaceEvent::ImeDisabled { .. } | SurfaceEvent::Focus { focused: false, .. } => {
                    // The scene depends on `preedit` (composed text is drawn)
                    // and on `pointer_anchor` (it feeds the proof marker's
                    // color); `configure_text_input` itself has no visual
                    // effect on the Surface. So this only needs a repaint
                    // when clearing state actually clears something.
                    needs_present = app.preedit.is_some()
                        || app.pointer_anchor.is_some()
                        || app.composition_target.is_some();
                    app.cancel_composition();
                    app.pointer_anchor = None;
                    let _ = surfaces::configure_text_input(
                        surface_key(),
                        false,
                        instar::kernel::surface_types::LocalRect {
                            x: 0.0,
                            y: 0.0,
                            width: 0.0,
                            height: 0.0,
                        },
                    );
                }
                SurfaceEvent::Focus { focused: true, .. }
                | SurfaceEvent::ImeEnabled { .. }
                | SurfaceEvent::Metrics { .. } => {}
                _ => {}
            }
            if needs_present {
                GuestComponent::present(&app, &mut presentation).await?;
            }
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
    fn preedit_cursor_maps_to_the_projected_visual_position() {
        let mut app = Scratchpad::new("abc");
        app.carets = vec![Selection::at(1)];
        app.preedit_with_cursor("XY", Some((0, 1)));

        // The canonical caret remains at the composition target while the
        // active IME cursor is one byte into the projected preedit.
        assert_eq!(app.primary_position(), Position::at(1));
        assert_eq!(app.visual_cursor().unwrap(), (0, 2));

        app.preedit_with_cursor("XY", Some((0, 2)));
        assert_eq!(app.visual_cursor().unwrap(), (0, 3));
    }

    #[test]
    fn multiline_preedit_cursor_maps_after_its_inserted_line() {
        let mut app = Scratchpad::new("ab\ncd");
        app.carets = vec![Selection::at(1)];
        app.preedit_with_cursor("X\nYZ", Some((0, 3)));

        assert_eq!(app.visual_cursor().unwrap(), (1, 1));
    }

    #[test]
    fn empty_document_preedit_cursor_has_projected_geometry() {
        let mut app = Scratchpad::new("");
        app.preedit_with_cursor("abcdef", Some((0, 6)));

        assert_eq!(app.visual_cursor().unwrap(), (0, 6));
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

    #[test]
    fn multiline_preedit_stays_in_the_bounded_row_projection() {
        let mut app = Scratchpad::new("before\nafter");
        app.carets = vec![Selection {
            anchor: Position::at(0),
            head: Position::at(6),
        }];
        app.preedit("one\ntwo\nthree");
        let visible = app.visible_projection(4).unwrap();
        assert_eq!(visible, "one\ntwo\nthree\nafter");
        assert_eq!(app.document.as_string(), "before\nafter");
    }
}
