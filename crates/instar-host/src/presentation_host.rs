//! Main-thread ownership for immutable layouts and retained Surface scenes.

use std::sync::Arc;

use instar_kernel::presentation::{
    BridgeAffinity, BridgeCursor, BridgeMetrics, BridgeRect, LayoutQuery, OpaqueLayoutKey,
    PresentationAnswer, PresentationRefusal,
};
use instar_kernel::runtime::GenerationId;
use instar_surface_protocol::{Command, Scene};
use instar_text_layout::{
    Affinity, Alignment, LayoutError, LineHeight, ShapingStyle, SharedLayout, TextCursor,
    TextEngine,
};

pub type CandidateRect = BridgeRect;

#[derive(Debug)]
struct LayoutSlot {
    incarnation: u32,
    generation: GenerationId,
    guest_lease: bool,
    layout: SharedLayout,
}

#[derive(Debug, Default)]
pub struct LayoutRegistry {
    slots: Vec<Option<LayoutSlot>>,
    next_incarnation: u32,
}

#[derive(Debug, Clone)]
pub struct RetainedSurfaceCommand {
    pub command: Command,
    /// Present exactly for `DrawTextLayout`. This is the internal strong
    /// reference; the guest capability and Wasmtime table entry never enter a
    /// retained scene.
    pub layout: Option<SharedLayout>,
}

#[derive(Debug, Clone)]
pub struct RetainedSurfaceScene {
    pub revision: u64,
    pub commands: Vec<RetainedSurfaceCommand>,
}

impl LayoutRegistry {
    pub fn create(
        &mut self,
        engine: &mut TextEngine,
        generation: GenerationId,
        text: &str,
        style: instar_kernel::presentation::BridgeLayoutStyle,
    ) -> Result<OpaqueLayoutKey, PresentationRefusal> {
        let live = self
            .slots
            .iter()
            .flatten()
            .filter(|slot| slot.generation == generation && slot.guest_lease)
            .count();
        if live >= instar_text_layout::MAX_LIVE_LAYOUTS {
            return Err(PresentationRefusal::TooManyLiveLayouts(live as u32));
        }
        let shaping = ShapingStyle {
            role: match style.role {
                instar_kernel::presentation::BridgeFontRole::SystemUi => {
                    instar_text_layout::FontRole::SystemUi
                }
                instar_kernel::presentation::BridgeFontRole::Monospace => {
                    instar_text_layout::FontRole::Monospace
                }
            },
            size: style.size,
            weight: style.weight,
            wrap: style.wrap,
        };
        let line_height = match style.line_height {
            instar_kernel::presentation::BridgeLineHeight::MetricsRelative(value) => {
                LineHeight::MetricsRelative(value)
            }
            instar_kernel::presentation::BridgeLineHeight::FontSizeRelative(value) => {
                LineHeight::FontSizeRelative(value)
            }
            instar_kernel::presentation::BridgeLineHeight::Absolute(value) => {
                LineHeight::Absolute(value)
            }
        };
        let alignment = match style.alignment {
            instar_kernel::presentation::BridgeAlignment::Start => Alignment::Start,
            instar_kernel::presentation::BridgeAlignment::Center => Alignment::Center,
            instar_kernel::presentation::BridgeAlignment::End => Alignment::End,
        };
        let layout = engine
            .create_layout(text, shaping, line_height, style.width, alignment)
            .map_err(map_layout_error)?;
        self.next_incarnation = self.next_incarnation.wrapping_add(1).max(1);
        let incarnation = self.next_incarnation;
        let slot = match self.slots.iter().position(Option::is_none) {
            Some(slot) => {
                self.slots[slot] = Some(LayoutSlot {
                    incarnation,
                    generation,
                    guest_lease: true,
                    layout,
                });
                slot
            }
            None => {
                self.slots.push(Some(LayoutSlot {
                    incarnation,
                    generation,
                    guest_lease: true,
                    layout,
                }));
                self.slots.len() - 1
            }
        };
        Ok(OpaqueLayoutKey {
            slot: slot as u32,
            incarnation,
        })
    }

    pub fn resolve(
        &self,
        generation: GenerationId,
        key: OpaqueLayoutKey,
    ) -> Result<SharedLayout, PresentationRefusal> {
        let Some(slot) = self.slots.get(key.slot as usize).and_then(Option::as_ref) else {
            return Err(PresentationRefusal::NoSuchLayout);
        };
        if slot.incarnation != key.incarnation || slot.generation != generation || !slot.guest_lease
        {
            return Err(PresentationRefusal::NoSuchLayout);
        }
        Ok(Arc::clone(&slot.layout))
    }

    pub fn release(&mut self, generation: GenerationId, key: OpaqueLayoutKey) {
        if let Some(slot) = self
            .slots
            .get_mut(key.slot as usize)
            .and_then(Option::as_mut)
            && slot.incarnation == key.incarnation
            && slot.generation == generation
        {
            slot.guest_lease = false;
        }
        self.collect();
    }

    pub fn retire_generation(&mut self, generation: GenerationId) {
        for slot in self.slots.iter_mut().flatten() {
            if slot.generation == generation {
                slot.guest_lease = false;
            }
        }
        self.collect();
    }

    pub fn collect(&mut self) {
        for slot in &mut self.slots {
            if slot
                .as_ref()
                .is_some_and(|slot| !slot.guest_lease && Arc::strong_count(&slot.layout) == 1)
            {
                *slot = None;
            }
        }
    }

    pub fn query(
        &self,
        generation: GenerationId,
        key: OpaqueLayoutKey,
        query: LayoutQuery,
    ) -> Result<PresentationAnswer, PresentationRefusal> {
        let layout = self.resolve(generation, key)?;
        let cursor = |cursor: BridgeCursor| TextCursor {
            index: cursor.index as usize,
            affinity: match cursor.affinity {
                BridgeAffinity::Downstream => Affinity::Downstream,
                BridgeAffinity::Upstream => Affinity::Upstream,
            },
        };
        let answer_cursor = |cursor: TextCursor| {
            PresentationAnswer::Cursor(BridgeCursor {
                index: cursor.index as u32,
                affinity: match cursor.affinity {
                    Affinity::Downstream => BridgeAffinity::Downstream,
                    Affinity::Upstream => BridgeAffinity::Upstream,
                },
            })
        };
        Ok(match query {
            LayoutQuery::Metrics => {
                let m = layout.metrics();
                PresentationAnswer::Metrics(BridgeMetrics {
                    width: m.width,
                    height: m.height,
                    lines: m.lines as u32,
                    clusters: m.clusters as u32,
                })
            }
            LayoutQuery::CursorFromPoint { x_bits, y_bits } => answer_cursor(
                layout.cursor_from_point(f32::from_bits(x_bits), f32::from_bits(y_bits)),
            ),
            LayoutQuery::CaretRect {
                cursor: c,
                width_bits,
            } => PresentationAnswer::Rect(rect(
                layout
                    .caret_rect(cursor(c), f32::from_bits(width_bits))
                    .map_err(map_layout_error)?,
            )),
            LayoutQuery::SelectionRects { anchor, focus } => PresentationAnswer::Rects(
                layout
                    .selection_rects(cursor(anchor), cursor(focus))
                    .map_err(map_layout_error)?
                    .into_iter()
                    .map(rect)
                    .collect(),
            ),
            LayoutQuery::PreviousVisual(c) => answer_cursor(
                layout
                    .previous_visual(cursor(c))
                    .map_err(map_layout_error)?,
            ),
            LayoutQuery::NextVisual(c) => {
                answer_cursor(layout.next_visual(cursor(c)).map_err(map_layout_error)?)
            }
            LayoutQuery::VisualLineStart(c) => answer_cursor(
                layout
                    .visual_line_start(cursor(c))
                    .map_err(map_layout_error)?,
            ),
            LayoutQuery::VisualLineEnd(c) => answer_cursor(
                layout
                    .visual_line_end(cursor(c))
                    .map_err(map_layout_error)?,
            ),
            LayoutQuery::HardLineStart(c) => answer_cursor(
                layout
                    .hard_line_start(cursor(c))
                    .map_err(map_layout_error)?,
            ),
            LayoutQuery::HardLineEnd(c) => {
                answer_cursor(layout.hard_line_end(cursor(c)).map_err(map_layout_error)?)
            }
            LayoutQuery::PreviousWord(c) => answer_cursor(
                layout
                    .previous_standard_word_boundary(cursor(c))
                    .map_err(map_layout_error)?,
            ),
            LayoutQuery::NextWord(c) => answer_cursor(
                layout
                    .next_standard_word_boundary(cursor(c))
                    .map_err(map_layout_error)?,
            ),
        })
    }
}

pub fn stage_scene(
    decoded: Scene,
    layouts: &[SharedLayout],
    revision: u64,
) -> RetainedSurfaceScene {
    let commands = decoded
        .commands
        .into_iter()
        .map(|command| {
            let layout = match command {
                Command::DrawTextLayout { layout_slot, .. } => {
                    Some(Arc::clone(&layouts[layout_slot as usize]))
                }
                _ => None,
            };
            RetainedSurfaceCommand { command, layout }
        })
        .collect();
    RetainedSurfaceScene { revision, commands }
}

fn rect(value: instar_text_layout::CaretGeometry) -> BridgeRect {
    BridgeRect {
        x: value.x,
        y: value.y,
        width: value.width,
        height: value.height,
    }
}

fn map_layout_error(error: LayoutError) -> PresentationRefusal {
    match error {
        LayoutError::TextTooLarge(value) => PresentationRefusal::TextTooLarge(value as u32),
        LayoutError::InvalidStyle => PresentationRefusal::InvalidStyle,
        LayoutError::TooManyLines(value) => PresentationRefusal::TooManyLines(value as u32),
        LayoutError::TooManyClusters(value) => PresentationRefusal::TooManyClusters(value as u32),
        LayoutError::InvalidCursor { index } => PresentationRefusal::InvalidCursor(index as u32),
        LayoutError::TooManySelectionRects(value) => {
            PresentationRefusal::TooManySelectionRects(value as u32)
        }
    }
}
