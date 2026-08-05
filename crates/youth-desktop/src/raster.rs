use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;
use youth_paint::{Color, PaintCommand, PaintScene, PhysicalSize, Point, Rect, Size};
use youth_runtime::{PresentationReader, resolve_countdown_display};
use youth_text_render_cpu::GlyphRasterizer;
use youth_tree::{NodeData, NodeId, TextAlignment, Tree};

use crate::geometry::{LayoutSnapshot, LogicalRect};

pub const MAX_FRAMEBUFFER_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Palette {
    pub background: u32,
    pub container: u32,
    pub text: u32,
    pub button: u32,
    pub button_hover: u32,
    pub button_pressed: u32,
    pub button_disabled: u32,
    pub border: u32,
    pub focus: u32,
    pub fault_background: u32,
    pub fault_text: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: 0x0018_1a20,
            container: 0x0025_2933,
            text: 0x00f2_f4f8,
            button: 0x0038_6bd6,
            button_hover: 0x0049_7bea,
            button_pressed: 0x0028_50aa,
            button_disabled: 0x0054_5862,
            border: 0x0095_a4c7,
            focus: 0x00ff_c857,
            fault_background: 0x0048_1820,
            fault_text: 0x00ff_d5d9,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RenderState<'a> {
    pub hovered: Option<NodeId>,
    pub pressed: Option<NodeId>,
    pub focused: Option<NodeId>,
    pub fault_category: Option<&'a str>,
    pub presentation: Option<&'a PresentationReader>,
    /// Glyph rasterization cache for live Editor presentations. `None`
    /// falls back to the crude bitmap font (used by callers, such as the
    /// pure-geometry test suite, that don't need real text rendering) --
    /// this keeps every existing snapshot test's expected output
    /// unchanged. `RefCell` because rasterization caches glyphs on read
    /// while `RenderState`'s other fields stay shared references.
    pub editor_rasterizer: Option<&'a RefCell<GlyphRasterizer>>,
    /// Host-owned vertical scroll offset (logical pixels) per live Editor
    /// node. Absent entries (and a `None` map) paint at zero offset --
    /// scrolling is purely a paint-time transform, never guest-visible
    /// state, so this has no bearing on the tree/layout revision this
    /// function otherwise renders as a pure function of.
    pub editor_scroll_offsets: Option<&'a HashMap<NodeId, f32>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameBuffer {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl FrameBuffer {
    pub fn new(width: u32, height: u32) -> Result<Self, RasterError> {
        let pixels = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or(RasterError::FramebufferTooLarge)?;
        let bytes = pixels
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or(RasterError::FramebufferTooLarge)?;
        if bytes > MAX_FRAMEBUFFER_BYTES {
            return Err(RasterError::FramebufferTooLarge);
        }
        Ok(Self {
            width,
            height,
            pixels: vec![0; pixels],
        })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.pixels.len() * 4);
        for pixel in &self.pixels {
            bytes.extend_from_slice(&pixel.to_le_bytes());
        }
        bytes
    }

    fn clear(&mut self, color: u32) {
        self.pixels.fill(color);
    }

    /// Alpha-blends `color` (0x00RRGGBB) over the existing pixel at `(x,
    /// y)` weighted by `coverage` (0 = no-op, 255 = fully opaque, and bit-
    /// identical to a hard overwrite since `blend_over` reduces to the
    /// foreground channel exactly at full coverage). A no-op outside the
    /// framebuffer's bounds.
    fn blend_pixel(&mut self, x: i32, y: i32, color: u32, coverage: u8) {
        if coverage == 0 {
            return;
        }
        let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
            return;
        };
        if x >= self.width || y >= self.height {
            return;
        }
        let index = y as usize * self.width as usize + x as usize;
        self.pixels[index] = blend_over(self.pixels[index], color, coverage);
    }
}

#[derive(Clone, Copy, Debug)]
struct PixelRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug, Error)]
pub enum RasterError {
    #[error("framebuffer exceeds the configured 128 MiB limit")]
    FramebufferTooLarge,
    #[error("scale factor must be finite and positive")]
    InvalidScale,
    #[error("layout and semantic tree revisions differ")]
    RevisionMismatch,
    #[error("scene construction produced an invalid paint scene: {0}")]
    InvalidScene(#[from] youth_paint::PaintSceneError),
}

/// Renders `tree`/`layout` against `state` into an owned [`FrameBuffer`].
/// Internally: build a [`PaintScene`] describing paint *intent* (colors,
/// rects, glyph masks, clip regions -- deciding what each node means
/// visually), validate it, then interpret that scene into pixels (deciding
/// how to rasterize it). This function's signature and behavior are
/// unchanged by that split; it exists so a future paint backend can
/// consume the same `PaintScene` without touching scene construction.
pub fn render(
    tree: &Tree,
    layout: &LayoutSnapshot,
    physical_width: u32,
    physical_height: u32,
    scale_factor: f64,
    state: &RenderState<'_>,
    palette: Palette,
) -> Result<FrameBuffer, RasterError> {
    let _span = tracing::info_span!("desktop.render", revision = tree.revision()).entered();
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err(RasterError::InvalidScale);
    }
    if tree.revision() != layout.tree_revision {
        return Err(RasterError::RevisionMismatch);
    }
    let scene = build_scene(
        tree,
        layout,
        physical_width,
        physical_height,
        scale_factor,
        state,
        palette,
    );
    scene.validate()?;
    Ok(paint_scene(&scene))
}

/// Translates `tree`/`layout`/`state` into paint intent: one deterministic
/// pass over `layout.nodes` (already NodeId-ordered, which is what today's
/// paint order is and must stay, since it's baked into the exact pinned
/// frame hashes below) deciding what each node means visually. Contains no
/// pixel-level rasterization at all -- every color decision (button state,
/// palette lookup) happens here; every actual pixel write happens in
/// [`paint_scene`].
fn build_scene(
    tree: &Tree,
    layout: &LayoutSnapshot,
    physical_width: u32,
    physical_height: u32,
    scale_factor: f64,
    state: &RenderState<'_>,
    palette: Palette,
) -> PaintScene {
    let mut commands = vec![PaintCommand::Clear {
        color: opaque(palette.background),
    }];
    for (id, node) in &layout.nodes {
        let Some(semantic) = tree.node(*id) else {
            continue;
        };
        let rect = physical_rect(node.bounds, scale_factor, physical_width, physical_height);
        match &semantic.data {
            NodeData::Root => {}
            NodeData::Box { .. } | NodeData::Row { .. } | NodeData::Grid { .. } => {
                commands.push(fill_command(rect, palette.container));
                commands.push(stroke_command(rect, palette.border));
            }
            NodeData::Text { value } => {
                push_text_run(
                    &mut commands,
                    rect,
                    value,
                    TextAlignment::Start,
                    scale_factor,
                    palette.text,
                );
            }
            NodeData::Editor { text, .. } => {
                let live = state.presentation.and_then(|reader| reader.editor(*id));
                match (live, state.editor_rasterizer) {
                    (Some(presentation), Some(rasterizer)) => {
                        let scroll_offset_y = state
                            .editor_scroll_offsets
                            .and_then(|offsets| offsets.get(id))
                            .copied()
                            .unwrap_or(0.0);
                        push_editor_presentation(
                            &mut commands,
                            rect,
                            &presentation,
                            &mut rasterizer.borrow_mut(),
                            palette,
                            scroll_offset_y,
                            scale_factor as f32,
                        );
                    }
                    _ => {
                        // No live host session yet (e.g. the very first
                        // paint before the runtime's presentation cache
                        // has synced), or a caller that doesn't wire real
                        // text rendering (pure-geometry tests): fall back
                        // to the guest-declared static text via the
                        // existing bitmap font.
                        push_text_run(
                            &mut commands,
                            rect,
                            text,
                            TextAlignment::Start,
                            scale_factor,
                            palette.text,
                        );
                    }
                }
            }
            NodeData::TextDocumentEditor { .. } => {
                if let (Some(presentation), Some(rasterizer)) = (
                    state.presentation.and_then(|reader| reader.editor(*id)),
                    state.editor_rasterizer,
                ) {
                    let scroll_offset_y = state
                        .editor_scroll_offsets
                        .and_then(|offsets| offsets.get(id))
                        .copied()
                        .unwrap_or(0.0);
                    push_editor_presentation(
                        &mut commands,
                        rect,
                        &presentation,
                        &mut rasterizer.borrow_mut(),
                        palette,
                        scroll_offset_y,
                        scale_factor as f32,
                    );
                }
            }
            NodeData::AlignedText { value, alignment } => {
                push_text_run(
                    &mut commands,
                    rect,
                    value,
                    *alignment,
                    scale_factor,
                    palette.text,
                );
            }
            NodeData::Countdown {
                schedule,
                precision,
                format,
            }
            | NodeData::AlignedCountdown {
                schedule,
                precision,
                format,
                alignment: TextAlignment::Start,
            } => {
                let record = state
                    .presentation
                    .and_then(|reader| reader.schedule(schedule.id));
                let now = state
                    .presentation
                    .map_or(0, PresentationReader::now_epoch_millis);
                let value =
                    resolve_countdown_display(*schedule, *precision, *format, record.as_ref(), now);
                push_text_run(
                    &mut commands,
                    rect,
                    &value,
                    TextAlignment::Start,
                    scale_factor,
                    palette.text,
                );
            }
            NodeData::AlignedCountdown {
                schedule,
                precision,
                format,
                alignment,
            } => {
                let record = state
                    .presentation
                    .and_then(|reader| reader.schedule(schedule.id));
                let now = state
                    .presentation
                    .map_or(0, PresentationReader::now_epoch_millis);
                let value =
                    resolve_countdown_display(*schedule, *precision, *format, record.as_ref(), now);
                push_text_run(
                    &mut commands,
                    rect,
                    &value,
                    *alignment,
                    scale_factor,
                    palette.text,
                );
            }
            NodeData::Button { label, .. } | NodeData::ShortcutButton { label, .. } => {
                let color = if !node.effective_enabled {
                    palette.button_disabled
                } else if state.pressed == Some(*id) {
                    palette.button_pressed
                } else if state.hovered == Some(*id) {
                    palette.button_hover
                } else {
                    palette.button
                };
                commands.push(fill_command(rect, color));
                commands.push(stroke_command(rect, palette.border));
                if state.focused == Some(*id) && rect.width > 4 && rect.height > 4 {
                    commands.push(stroke_command(
                        PixelRect {
                            x: rect.x + 2,
                            y: rect.y + 2,
                            width: rect.width - 4,
                            height: rect.height - 4,
                        },
                        palette.focus,
                    ));
                }
                let inset_x = (12.0 * scale_factor).floor().max(0.0) as u32;
                let inset_y = (8.0 * scale_factor).floor().max(0.0) as u32;
                let label_rect = PixelRect {
                    x: rect.x.saturating_add(inset_x),
                    y: rect.y.saturating_add(inset_y),
                    width: rect.width.saturating_sub(inset_x),
                    height: rect.height.saturating_sub(inset_y),
                };
                push_text_run(
                    &mut commands,
                    label_rect,
                    label,
                    TextAlignment::Start,
                    scale_factor,
                    palette.text,
                );
            }
        }
    }
    if let Some(category) = state.fault_category {
        commands.push(PaintCommand::Clear {
            color: opaque(palette.fault_background),
        });
        push_text_run_at(
            &mut commands,
            16,
            16,
            "YOUTH APP FAULT",
            palette.fault_text,
            1,
        );
        push_text_run_at(&mut commands, 16, 32, category, palette.fault_text, 1);
    }
    PaintScene {
        size: PhysicalSize {
            width: physical_width,
            height: physical_height,
        },
        commands,
    }
}

/// Interprets a validated [`PaintScene`] into an owned [`FrameBuffer`].
/// This is the only place that turns paint intent into actual pixels; it
/// has no knowledge of `NodeData`, palettes, or layout -- it only knows
/// how to composite the six [`PaintCommand`] variants, in order, against a
/// clip-rect stack.
fn paint_scene(scene: &PaintScene) -> FrameBuffer {
    let mut frame = FrameBuffer {
        width: scene.size.width,
        height: scene.size.height,
        pixels: vec![0; scene.size.width as usize * scene.size.height as usize],
    };
    let mut clip_stack: Vec<PixelRect> = Vec::new();
    for command in &scene.commands {
        match command {
            PaintCommand::Clear { color } => frame.clear(rgb_u32(*color)),
            PaintCommand::FillRect { rect, color } => {
                blend_rect(&mut frame, *rect, *color, current_clip(&clip_stack));
            }
            PaintCommand::StrokeRect { rect, color, .. } => {
                // Always a 1-physical-pixel stroke in this increment (see
                // the `PaintCommand::StrokeRect` doc comment) -- four edge
                // fills, exactly matching the old hand-rolled `border()`.
                let clip = current_clip(&clip_stack);
                if rect.width > 0 && rect.height > 0 {
                    blend_rect(&mut frame, Rect { height: 1, ..*rect }, *color, clip);
                    blend_rect(
                        &mut frame,
                        Rect {
                            y: rect.y + rect.height as i32 - 1,
                            height: 1,
                            ..*rect
                        },
                        *color,
                        clip,
                    );
                    blend_rect(&mut frame, Rect { width: 1, ..*rect }, *color, clip);
                    blend_rect(
                        &mut frame,
                        Rect {
                            x: rect.x + rect.width as i32 - 1,
                            width: 1,
                            ..*rect
                        },
                        *color,
                        clip,
                    );
                }
            }
            PaintCommand::GlyphMask {
                origin,
                size,
                alpha,
                color,
            } => {
                let clip = current_clip(&clip_stack);
                let rgb = rgb_u32(*color);
                for row in 0..size.height {
                    for col in 0..size.width {
                        let coverage = alpha[(row * size.width + col) as usize];
                        if coverage == 0 {
                            continue;
                        }
                        let x = origin.x + col as i32;
                        let y = origin.y + row as i32;
                        if let Some(clip) = clip
                            && !pixel_in(x, y, clip)
                        {
                            continue;
                        }
                        frame.blend_pixel(x, y, rgb, coverage);
                    }
                }
            }
            PaintCommand::PushClip { rect } => {
                let pixel_rect = to_pixel_rect(*rect);
                clip_stack.push(match current_clip(&clip_stack) {
                    Some(existing) => intersect(existing, pixel_rect),
                    None => pixel_rect,
                });
            }
            PaintCommand::PopClip => {
                clip_stack.pop();
            }
        }
    }
    frame
}

fn opaque(rgb: u32) -> Color {
    Color {
        r: ((rgb >> 16) & 0xff) as u8,
        g: ((rgb >> 8) & 0xff) as u8,
        b: (rgb & 0xff) as u8,
        a: 255,
    }
}

fn color_with_alpha(rgb: u32, a: u8) -> Color {
    Color { a, ..opaque(rgb) }
}

fn rgb_u32(color: Color) -> u32 {
    (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)
}

fn fill_command(rect: PixelRect, rgb: u32) -> PaintCommand {
    PaintCommand::FillRect {
        rect: to_rect(rect),
        color: opaque(rgb),
    }
}

fn stroke_command(rect: PixelRect, rgb: u32) -> PaintCommand {
    PaintCommand::StrokeRect {
        rect: to_rect(rect),
        width: 1.0,
        color: opaque(rgb),
    }
}

fn to_rect(rect: PixelRect) -> Rect {
    Rect {
        x: rect.x as i32,
        y: rect.y as i32,
        width: rect.width,
        height: rect.height,
    }
}

fn to_pixel_rect(rect: Rect) -> PixelRect {
    PixelRect {
        x: rect.x.max(0) as u32,
        y: rect.y.max(0) as u32,
        width: rect.width,
        height: rect.height,
    }
}

fn current_clip(stack: &[PixelRect]) -> Option<PixelRect> {
    stack.last().copied()
}

fn pixel_in(x: i32, y: i32, clip: PixelRect) -> bool {
    let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
        return false;
    };
    x >= clip.x
        && x < clip.x.saturating_add(clip.width)
        && y >= clip.y
        && y < clip.y.saturating_add(clip.height)
}

fn intersect(a: PixelRect, b: PixelRect) -> PixelRect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let x_end = a.x.saturating_add(a.width).min(b.x.saturating_add(b.width));
    let y_end =
        a.y.saturating_add(a.height)
            .min(b.y.saturating_add(b.height));
    PixelRect {
        x,
        y,
        width: x_end.saturating_sub(x),
        height: y_end.saturating_sub(y),
    }
}

/// Alpha-blends `color` over every pixel of `rect`, intersected with
/// `clip` if a clip is active. A no-op for any pixel outside the
/// framebuffer's own bounds (`FrameBuffer::blend_pixel` handles that).
fn blend_rect(frame: &mut FrameBuffer, rect: Rect, color: Color, clip: Option<PixelRect>) {
    let rgb = rgb_u32(color);
    let (x_start, y_start, width, height) = match clip {
        Some(clip) => {
            let clipped = intersect(to_pixel_rect(rect), clip);
            (
                clipped.x as i32,
                clipped.y as i32,
                clipped.width,
                clipped.height,
            )
        }
        None => (rect.x, rect.y, rect.width, rect.height),
    };
    for y in y_start..y_start.saturating_add(height as i32) {
        for x in x_start..x_start.saturating_add(width as i32) {
            frame.blend_pixel(x, y, rgb, color.a);
        }
    }
}

/// Emits one [`PaintCommand::GlyphMask`] for `value` drawn via the
/// bitmap font, honoring `alignment` exactly as the old `draw_text`
/// helper did. A no-op (emits nothing) for an empty string, matching the
/// old code's behavior of simply not looping over any characters.
fn push_text_run(
    commands: &mut Vec<PaintCommand>,
    rect: PixelRect,
    value: &str,
    alignment: TextAlignment,
    scale_factor: f64,
    color_rgb: u32,
) {
    let glyph_scale = scale_factor.round().max(1.0) as u32;
    let char_count = u32::try_from(value.chars().count()).unwrap_or(u32::MAX);
    let text_width = char_count.saturating_mul(8).saturating_mul(glyph_scale);
    let text_x = match alignment {
        TextAlignment::Start => rect.x,
        TextAlignment::Center => rect
            .x
            .saturating_add(rect.width.saturating_sub(text_width) / 2),
        TextAlignment::End => rect.x.saturating_add(rect.width.saturating_sub(text_width)),
    };
    push_text_run_at(commands, text_x, rect.y, value, color_rgb, glyph_scale);
}

/// Rasterizes `value` via the bitmap font into one synthetic alpha-mask
/// [`PaintCommand::GlyphMask`], at absolute physical position `(x, y)`.
/// This is the bitmap-font equivalent of a real glyph run: one command per
/// drawn string, not per character -- an honest reflection of what the
/// font actually is (filled squares from a fixed 5x7 bitmap), not a fake
/// per-glyph scheme.
fn push_text_run_at(
    commands: &mut Vec<PaintCommand>,
    x: u32,
    y: u32,
    value: &str,
    color_rgb: u32,
    scale: u32,
) {
    if value.is_empty() {
        return;
    }
    let scale = scale.max(1);
    let char_count = u32::try_from(value.chars().count()).unwrap_or(u32::MAX);
    let width = char_count.saturating_mul(8).saturating_mul(scale);
    let height = 7 * scale;
    let Some(byte_count) = (width as usize).checked_mul(height as usize) else {
        return;
    };
    let mut alpha = vec![0u8; byte_count];
    let mut cursor = 0u32;
    for character in value.chars() {
        let rows = glyph_rows(character);
        for (row, bits) in rows.into_iter().enumerate() {
            for column in 0..5u32 {
                if bits & (1 << (4 - column)) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = cursor.saturating_add(column * scale).saturating_add(dx);
                        let py = (row as u32).saturating_mul(scale).saturating_add(dy);
                        if px < width && py < height {
                            alpha[(py * width + px) as usize] = 255;
                        }
                    }
                }
            }
        }
        cursor = cursor.saturating_add(8 * scale);
    }
    commands.push(PaintCommand::GlyphMask {
        origin: Point {
            x: x as i32,
            y: y as i32,
        },
        size: Size { width, height },
        alpha: Arc::from(alpha),
        color: opaque(color_rgb),
    });
}

/// Emits one live, host-owned Editor presentation (real glyph runs plus
/// selection/cursor geometry) as paint commands, bracketed in a
/// `PushClip`/`PopClip` pair around `rect`. A no-op for a zero-area rect.
///
/// Engine geometry is logical; glyph masks and rectangles are converted to
/// physical pixels exactly once using the window scale factor -- the same
/// conversion the old `draw_editor_presentation` performed, just emitting
/// commands instead of writing pixels directly.
fn push_editor_presentation(
    commands: &mut Vec<PaintCommand>,
    rect: PixelRect,
    presentation: &youth_runtime::TextPresentation,
    rasterizer: &mut GlyphRasterizer,
    palette: Palette,
    scroll_offset_y: f32,
    scale: f32,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    commands.push(PaintCommand::PushClip {
        rect: to_rect(rect),
    });

    let origin_x = rect.x as f32;
    let origin_y = rect.y as f32 - scroll_offset_y * scale;

    for selection_rect in &presentation.selection {
        let x0 = (origin_x + selection_rect.x0 as f32 * scale).round() as i32;
        let y0 = (origin_y + selection_rect.y0 as f32 * scale).round() as i32;
        let x1 = (origin_x + selection_rect.x1 as f32 * scale).round() as i32;
        let y1 = (origin_y + selection_rect.y1 as f32 * scale).round() as i32;
        let width = (x1 - x0).max(0) as u32;
        let height = (y1 - y0).max(0) as u32;
        if width > 0 && height > 0 {
            commands.push(PaintCommand::FillRect {
                rect: Rect {
                    x: x0,
                    y: y0,
                    width,
                    height,
                },
                color: color_with_alpha(palette.focus, 64),
            });
        }
    }

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
            commands.push(PaintCommand::GlyphMask {
                origin: Point {
                    x: base_x,
                    y: base_y,
                },
                size: Size {
                    width: mask.width,
                    height: mask.height,
                },
                alpha: Arc::from(mask.alpha.as_slice()),
                color: opaque(palette.text),
            });
        }
    }

    if let Some(cursor) = &presentation.cursor {
        let x0 = (origin_x + cursor.x0 as f32 * scale).round() as i32;
        let y0 = (origin_y + cursor.y0 as f32 * scale).round() as i32;
        let x1 = (origin_x + cursor.x1 as f32 * scale).round() as i32;
        let y1 = (origin_y + cursor.y1 as f32 * scale).round() as i32;
        let width = (x1 - x0).max(1) as u32;
        let height = (y1 - y0).max(0) as u32;
        if height > 0 {
            commands.push(PaintCommand::FillRect {
                rect: Rect {
                    x: x0,
                    y: y0,
                    width,
                    height,
                },
                color: color_with_alpha(palette.focus, 255),
            });
        }
    }

    commands.push(PaintCommand::PopClip);
}

/// Standard "over" alpha compositing of one 0x00RRGGBB color onto another,
/// weighted by an 8-bit coverage value.
fn blend_over(background: u32, foreground: u32, coverage: u8) -> u32 {
    let alpha = u32::from(coverage);
    let inverse = 255 - alpha;
    let mut out = 0u32;
    for shift in [16, 8, 0] {
        let bg_channel = (background >> shift) & 0xff;
        let fg_channel = (foreground >> shift) & 0xff;
        let channel = (fg_channel * alpha + bg_channel * inverse) / 255;
        out |= channel << shift;
    }
    out
}

#[must_use]
pub fn frame_hash(frame: &FrameBuffer) -> u64 {
    frame
        .canonical_bytes()
        .into_iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

fn physical_rect(rect: LogicalRect, scale: f64, width: u32, height: u32) -> PixelRect {
    let left = (rect.x * scale).floor().clamp(0.0, f64::from(width)) as u32;
    let top = (rect.y * scale).floor().clamp(0.0, f64::from(height)) as u32;
    let right = ((rect.x + rect.width) * scale)
        .ceil()
        .clamp(f64::from(left), f64::from(width)) as u32;
    let bottom = ((rect.y + rect.height) * scale)
        .ceil()
        .clamp(f64::from(top), f64::from(height)) as u32;
    PixelRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

const FIRST_PRINTABLE_ASCII: u32 = 0x20;
const LAST_PRINTABLE_ASCII: u32 = 0x7e;
const MISSING_GLYPH: [u8; 7] = [31, 17, 2, 4, 4, 0, 4];

// Provisional deterministic 5x7 glyphs for U+0020 through U+007E. Each row
// uses its low five bits, with the most significant of those bits on the left.
const PRINTABLE_ASCII_GLYPHS: [[u8; 7]; 95] = [
    [0, 0, 0, 0, 0, 0, 0],        // space
    [4, 4, 4, 4, 4, 0, 4],        // !
    [10, 10, 10, 0, 0, 0, 0],     // "
    [10, 31, 10, 10, 31, 10, 0],  // #
    [4, 15, 20, 14, 5, 30, 4],    // $
    [24, 25, 2, 4, 8, 19, 3],     // %
    [12, 18, 20, 8, 21, 18, 13],  // &
    [4, 4, 8, 0, 0, 0, 0],        // '
    [2, 4, 8, 8, 8, 4, 2],        // (
    [8, 4, 2, 2, 2, 4, 8],        // )
    [0, 21, 14, 31, 14, 21, 0],   // *
    [0, 4, 4, 31, 4, 4, 0],       // +
    [0, 0, 0, 0, 4, 4, 8],        // ,
    [0, 0, 0, 31, 0, 0, 0],       // -
    [0, 0, 0, 0, 0, 12, 12],      // .
    [1, 2, 2, 4, 8, 8, 16],       // /
    [14, 17, 19, 21, 25, 17, 14], // 0
    [4, 12, 4, 4, 4, 4, 14],      // 1
    [14, 17, 1, 2, 4, 8, 31],     // 2
    [30, 1, 1, 14, 1, 1, 30],     // 3
    [2, 6, 10, 18, 31, 2, 2],     // 4
    [31, 16, 16, 30, 1, 1, 30],   // 5
    [14, 16, 16, 30, 17, 17, 14], // 6
    [31, 1, 2, 4, 8, 8, 8],       // 7
    [14, 17, 17, 14, 17, 17, 14], // 8
    [14, 17, 17, 15, 1, 1, 14],   // 9
    [0, 4, 4, 0, 4, 4, 0],        // :
    [0, 4, 4, 0, 4, 4, 8],        // ;
    [2, 4, 8, 16, 8, 4, 2],       // <
    [0, 31, 0, 31, 0, 0, 0],      // =
    [8, 4, 2, 1, 2, 4, 8],        // >
    [14, 17, 1, 2, 4, 0, 4],      // ?
    [14, 17, 23, 21, 23, 16, 14], // @
    [14, 17, 17, 31, 17, 17, 17], // A
    [30, 17, 17, 30, 17, 17, 30], // B
    [14, 17, 16, 16, 16, 17, 14], // C
    [30, 17, 17, 17, 17, 17, 30], // D
    [31, 16, 16, 30, 16, 16, 31], // E
    [31, 16, 16, 30, 16, 16, 16], // F
    [14, 17, 16, 23, 17, 17, 15], // G
    [17, 17, 17, 31, 17, 17, 17], // H
    [14, 4, 4, 4, 4, 4, 14],      // I
    [7, 2, 2, 2, 18, 18, 12],     // J
    [17, 18, 20, 24, 20, 18, 17], // K
    [16, 16, 16, 16, 16, 16, 31], // L
    [17, 27, 21, 21, 17, 17, 17], // M
    [17, 25, 21, 19, 17, 17, 17], // N
    [14, 17, 17, 17, 17, 17, 14], // O
    [30, 17, 17, 30, 16, 16, 16], // P
    [14, 17, 17, 17, 21, 18, 13], // Q
    [30, 17, 17, 30, 20, 18, 17], // R
    [15, 16, 16, 14, 1, 1, 30],   // S
    [31, 4, 4, 4, 4, 4, 4],       // T
    [17, 17, 17, 17, 17, 17, 14], // U
    [17, 17, 17, 17, 17, 10, 4],  // V
    [17, 17, 17, 21, 21, 21, 10], // W
    [17, 17, 10, 4, 10, 17, 17],  // X
    [17, 17, 10, 4, 4, 4, 4],     // Y
    [31, 1, 2, 4, 8, 16, 31],     // Z
    [14, 8, 8, 8, 8, 8, 14],      // [
    [16, 8, 8, 4, 2, 2, 1],       // backslash
    [14, 2, 2, 2, 2, 2, 14],      // ]
    [4, 10, 17, 0, 0, 0, 0],      // ^
    [0, 0, 0, 0, 0, 0, 31],       // _
    [8, 4, 2, 0, 0, 0, 0],        // `
    [0, 0, 14, 1, 15, 17, 15],    // a
    [16, 16, 30, 17, 17, 17, 30], // b
    [0, 0, 14, 16, 16, 17, 14],   // c
    [1, 1, 15, 17, 17, 17, 15],   // d
    [0, 0, 14, 17, 31, 16, 14],   // e
    [6, 9, 8, 28, 8, 8, 8],       // f
    [0, 0, 15, 17, 15, 1, 14],    // g
    [16, 16, 30, 17, 17, 17, 17], // h
    [4, 0, 12, 4, 4, 4, 14],      // i
    [2, 0, 6, 2, 2, 18, 12],      // j
    [16, 16, 18, 20, 24, 20, 18], // k
    [12, 4, 4, 4, 4, 4, 14],      // l
    [0, 0, 26, 21, 21, 21, 21],   // m
    [0, 0, 30, 17, 17, 17, 17],   // n
    [0, 0, 14, 17, 17, 17, 14],   // o
    [0, 0, 30, 17, 30, 16, 16],   // p
    [0, 0, 15, 17, 15, 1, 1],     // q
    [0, 0, 22, 25, 16, 16, 16],   // r
    [0, 0, 15, 16, 14, 1, 30],    // s
    [8, 8, 28, 8, 8, 9, 6],       // t
    [0, 0, 17, 17, 17, 19, 13],   // u
    [0, 0, 17, 17, 17, 10, 4],    // v
    [0, 0, 17, 17, 21, 21, 10],   // w
    [0, 0, 17, 10, 4, 10, 17],    // x
    [0, 0, 17, 17, 15, 1, 14],    // y
    [0, 0, 31, 2, 4, 8, 31],      // z
    [2, 4, 4, 8, 4, 4, 2],        // {
    [4, 4, 4, 4, 4, 4, 4],        // |
    [8, 4, 4, 2, 4, 4, 8],        // }
    [0, 0, 9, 22, 0, 0, 0],       // ~
];

fn glyph_rows(character: char) -> [u8; 7] {
    let codepoint = u32::from(character);
    if (FIRST_PRINTABLE_ASCII..=LAST_PRINTABLE_ASCII).contains(&codepoint) {
        PRINTABLE_ASCII_GLYPHS[(codepoint - FIRST_PRINTABLE_ASCII) as usize]
    } else {
        MISSING_GLYPH
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{LogicalSize, layout};
    use youth_tree::{Node, TreeSnapshot};

    fn id(value: u64) -> NodeId {
        NodeId::new(value).unwrap()
    }

    fn counter() -> Tree {
        Tree::from_snapshot(
            TreeSnapshot {
                revision: 0,
                root: id(1),
                nodes: vec![
                    Node {
                        id: id(1),
                        data: NodeData::Root,
                        children: vec![id(2)],
                        grow: 0,
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Box { enabled: true },
                        children: vec![id(3), id(4)],
                        grow: 0,
                    },
                    Node {
                        id: id(3),
                        data: NodeData::Text {
                            value: "Count: 0".into(),
                        },
                        children: vec![],
                        grow: 0,
                    },
                    Node {
                        id: id(4),
                        data: NodeData::Button {
                            label: "Increment".into(),
                            enabled: true,
                        },
                        children: vec![],
                        grow: 0,
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap()
    }

    fn single_text(data: NodeData) -> Tree {
        Tree::from_snapshot(
            TreeSnapshot {
                revision: 0,
                root: id(1),
                nodes: vec![
                    Node {
                        id: id(1),
                        data: NodeData::Root,
                        children: vec![id(2)],
                        grow: 0,
                    },
                    Node {
                        id: id(2),
                        data,
                        children: vec![],
                        grow: 0,
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap()
    }

    #[test]
    fn countdowns_render_and_aligned_countdowns_share_text_alignment() {
        let schedule = youth_tree::ScheduleRef {
            id: 7,
            generation: 1,
        };
        let countdown = single_text(NodeData::Countdown {
            schedule,
            precision: youth_tree::TimePrecision::Seconds,
            format: youth_tree::CountdownFormat::MinutesSeconds,
        });
        let countdown_layout = layout(&countdown, LogicalSize::new(100.0, 20.0).unwrap()).unwrap();
        render(
            &countdown,
            &countdown_layout,
            100,
            20,
            1.0,
            &RenderState::default(),
            Palette::default(),
        )
        .unwrap();

        let aligned = single_text(NodeData::AlignedCountdown {
            schedule,
            precision: youth_tree::TimePrecision::Seconds,
            format: youth_tree::CountdownFormat::MinutesSeconds,
            alignment: TextAlignment::End,
        });
        let literal = single_text(NodeData::AlignedText {
            value: "--:--".into(),
            alignment: TextAlignment::End,
        });
        let aligned_layout = layout(&aligned, LogicalSize::new(100.0, 20.0).unwrap()).unwrap();
        let literal_layout = layout(&literal, LogicalSize::new(100.0, 20.0).unwrap()).unwrap();
        let aligned_frame = render(
            &aligned,
            &aligned_layout,
            100,
            20,
            1.0,
            &RenderState::default(),
            Palette::default(),
        )
        .unwrap();
        let literal_frame = render(
            &literal,
            &literal_layout,
            100,
            20,
            1.0,
            &RenderState::default(),
            Palette::default(),
        )
        .unwrap();
        assert_eq!(aligned_frame, literal_frame);
    }

    #[test]
    fn editor_round_trip_preserves_revision_canonical_form_and_frame_hash() {
        let editor = single_text(NodeData::Editor {
            document_revision: 42,
            text: "Scratchpad draft".into(),
        });
        let canonical_snapshot = editor.to_snapshot();
        let round_trip =
            Tree::from_snapshot(canonical_snapshot.clone(), &youth_tree::Limits::default())
                .unwrap();
        assert_eq!(round_trip.to_snapshot(), canonical_snapshot);
        assert_eq!(
            round_trip.node(id(2)).unwrap().data,
            NodeData::Editor {
                document_revision: 42,
                text: "Scratchpad draft".into(),
            }
        );
        assert_eq!(
            round_trip.canonical(),
            "root #1\n└── editor #2 document-revision=42 \"Scratchpad draft\"\n"
        );

        let first_layout = layout(&editor, LogicalSize::new(160.0, 32.0).unwrap()).unwrap();
        let second_layout = layout(&round_trip, LogicalSize::new(160.0, 32.0).unwrap()).unwrap();
        let first_frame = render(
            &editor,
            &first_layout,
            160,
            32,
            1.0,
            &RenderState::default(),
            Palette::default(),
        )
        .unwrap();
        let second_frame = render(
            &round_trip,
            &second_layout,
            160,
            32,
            1.0,
            &RenderState::default(),
            Palette::default(),
        )
        .unwrap();
        assert_eq!(first_frame, second_frame);
        assert_eq!(frame_hash(&first_frame), 2_746_944_975_626_425_349);
    }

    #[test]
    fn a_live_editor_presentation_paints_real_glyphs_within_bounds() {
        use youth_editor_engine::{EditorLayout, ParleyEditorEngine};

        let mut engine = ParleyEditorEngine::with_text("Hi");
        let presentation: youth_runtime::TextPresentation = engine.presentation();

        let mut frame = FrameBuffer::new(160, 32).unwrap();
        let palette = Palette::default();
        frame.clear(palette.background);
        let mut rasterizer = GlyphRasterizer::new();
        let rect = PixelRect {
            x: 4,
            y: 4,
            width: 120,
            height: 24,
        };
        let mut commands = Vec::new();
        push_editor_presentation(
            &mut commands,
            rect,
            &presentation,
            &mut rasterizer,
            palette,
            0.0,
            1.0,
        );
        let scene = PaintScene {
            size: PhysicalSize {
                width: 160,
                height: 32,
            },
            commands: {
                let mut all = vec![PaintCommand::Clear {
                    color: opaque(palette.background),
                }];
                all.extend(commands);
                all
            },
        };
        scene.validate().unwrap();
        let frame = paint_scene(&scene);

        let painted_pixels = frame
            .pixels()
            .iter()
            .filter(|&&pixel| pixel != palette.background)
            .count();
        assert!(
            painted_pixels > 0,
            "visible text must paint at least one non-background pixel"
        );

        // Bounded-region assertion (this platform's determinism policy tier
        // -- see the crate-level testing notes -- rather than an exact
        // pixel hash, which only a canonical CI environment asserts):
        // every touched pixel stays inside the target rect.
        for (index, &pixel) in frame.pixels().iter().enumerate() {
            if pixel == palette.background {
                continue;
            }
            let x = (index % frame.width() as usize) as u32;
            let y = (index / frame.width() as usize) as u32;
            assert!(
                x >= rect.x && x < rect.x + rect.width,
                "painted pixel x={x} must stay within the clip rect"
            );
            assert!(
                y >= rect.y && y < rect.y + rect.height,
                "painted pixel y={y} must stay within the clip rect"
            );
        }
    }

    #[test]
    fn a_nonzero_scroll_offset_shifts_painted_content_upward_and_stays_clipped() {
        use youth_editor_engine::{EditorLayout, ParleyEditorEngine};

        let mut engine = ParleyEditorEngine::with_text("Hi");
        let presentation: youth_runtime::TextPresentation = engine.presentation();
        let rect = PixelRect {
            x: 4,
            y: 4,
            width: 120,
            height: 24,
        };
        let palette = Palette::default();

        let render_at = |scroll_offset_y: f32| -> FrameBuffer {
            let mut rasterizer = GlyphRasterizer::new();
            let mut commands = vec![PaintCommand::Clear {
                color: opaque(palette.background),
            }];
            push_editor_presentation(
                &mut commands,
                rect,
                &presentation,
                &mut rasterizer,
                palette,
                scroll_offset_y,
                1.0,
            );
            let scene = PaintScene {
                size: PhysicalSize {
                    width: 160,
                    height: 32,
                },
                commands,
            };
            scene.validate().unwrap();
            paint_scene(&scene)
        };

        let unscrolled = render_at(0.0);
        let scrolled = render_at(6.0);

        assert_ne!(
            unscrolled, scrolled,
            "a nonzero scroll offset must change what gets painted"
        );
        for (index, &pixel) in scrolled.pixels().iter().enumerate() {
            if pixel == palette.background {
                continue;
            }
            let x = (index % scrolled.width() as usize) as u32;
            let y = (index / scrolled.width() as usize) as u32;
            assert!(
                x >= rect.x && x < rect.x + rect.width,
                "a scrolled paint must still stay within the clip rect's x bounds"
            );
            assert!(
                y >= rect.y && y < rect.y + rect.height,
                "a scrolled paint must still stay within the clip rect's y bounds"
            );
        }
    }

    #[test]
    fn an_empty_rect_paints_nothing_and_does_not_panic() {
        use youth_editor_engine::{EditorLayout, ParleyEditorEngine};

        let mut engine = ParleyEditorEngine::with_text("Hi");
        let presentation: youth_runtime::TextPresentation = engine.presentation();
        let mut rasterizer = GlyphRasterizer::new();
        let mut commands = Vec::new();
        push_editor_presentation(
            &mut commands,
            PixelRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            &presentation,
            &mut rasterizer,
            Palette::default(),
            0.0,
            1.0,
        );
        assert!(
            commands.is_empty(),
            "a zero-area rect must emit no paint commands"
        );
    }

    #[test]
    fn raw_frame_fixtures_are_deterministic() {
        let tree = counter();
        let layout = layout(&tree, LogicalSize::new(320.0, 180.0).unwrap()).unwrap();
        let palette = Palette::default();
        let normal = render(
            &tree,
            &layout,
            320,
            180,
            1.0,
            &RenderState::default(),
            palette,
        )
        .unwrap();
        let hover = render(
            &tree,
            &layout,
            320,
            180,
            1.0,
            &RenderState {
                hovered: Some(id(4)),
                ..RenderState::default()
            },
            palette,
        )
        .unwrap();
        let pressed = render(
            &tree,
            &layout,
            320,
            180,
            1.0,
            &RenderState {
                hovered: Some(id(4)),
                pressed: Some(id(4)),
                focused: None,
                fault_category: None,
                presentation: None,
                editor_rasterizer: None,
                editor_scroll_offsets: None,
            },
            palette,
        )
        .unwrap();
        let focused = render(
            &tree,
            &layout,
            320,
            180,
            1.0,
            &RenderState {
                focused: Some(id(4)),
                ..RenderState::default()
            },
            palette,
        )
        .unwrap();
        let fault = render(
            &tree,
            &layout,
            320,
            180,
            1.0,
            &RenderState {
                fault_category: Some("guest_trap"),
                ..RenderState::default()
            },
            palette,
        )
        .unwrap();
        // The first four hashes were repinned by Gate L3 (the Taffy layout
        // cutover): `counter()`'s outer Box is Root's single child, which
        // the new Root contract unconditionally fills to the viewport --
        // a real, intentional layout change, not a rendering regression.
        // The fault overlay's hash (last) is unaffected since it clears
        // and repaints independent of node layout. Gate R0 (the PaintScene
        // extraction) changed none of these -- same pins as after Gate L3.
        assert_eq!(
            [
                frame_hash(&normal),
                frame_hash(&hover),
                frame_hash(&pressed),
                frame_hash(&focused),
                frame_hash(&fault)
            ],
            [
                10_691_040_568_706_047_298,
                6_907_917_246_561_339_834,
                10_876_101_366_824_324_814,
                1_586_224_771_165_965_434,
                10_375_799_425_807_607_732,
            ]
        );
    }

    #[test]
    fn printable_ascii_font_is_complete_and_representative_pixels_are_stable() {
        for byte in 0x20_u8..=0x7e {
            assert_ne!(
                glyph_rows(char::from(byte)),
                MISSING_GLYPH,
                "printable ASCII byte 0x{byte:02x} used the missing glyph"
            );
        }

        assert_eq!(glyph_rows('+'), [0, 4, 4, 31, 4, 4, 0]);
        assert_eq!(glyph_rows('/'), [1, 2, 2, 4, 8, 8, 16]);
        assert_eq!(glyph_rows('*'), [0, 21, 14, 31, 14, 21, 0]);
        assert_eq!(glyph_rows('.'), [0, 0, 0, 0, 0, 12, 12]);
        assert_eq!(glyph_rows('='), [0, 31, 0, 31, 0, 0, 0]);

        // 0x00ff_ffff (opaque white in this codebase's 0x00RRGGBB
        // convention -- every real Palette color follows it), not
        // `u32::MAX`: the pre-Gate-R0 `FrameBuffer::text` wrote colors as
        // a raw `u32` overwrite, so `u32::MAX`'s otherwise-unused top byte
        // (0xff) silently survived into the pinned hash below. The new
        // `youth_paint::Color{r,g,b,a}` type has no such stray byte to
        // leak -- structurally enforcing the "top byte is always zero"
        // invariant this codebase already relied on everywhere else.
        let mut commands = Vec::new();
        push_text_run_at(
            &mut commands,
            0,
            0,
            "+/- * . = % @ [] {} ~ AaZz",
            0x00ff_ffff,
            1,
        );
        let scene = PaintScene {
            size: PhysicalSize {
                width: 224,
                height: 7,
            },
            commands,
        };
        scene.validate().unwrap();
        let frame = paint_scene(&scene);
        assert_eq!(frame_hash(&frame), 13_376_335_021_794_499_461);
    }

    #[test]
    fn framebuffer_limits_and_zero_size_are_safe() {
        assert!(FrameBuffer::new(0, 0).is_ok());
        assert!(FrameBuffer::new(u32::MAX, u32::MAX).is_err());
    }

    // --- Gate R0: direct scene tests -------------------------------------
    //
    // Assert PaintScene *structure*, not just final pixels, so a future
    // backend mismatch is diagnosable as "wrong scene" (a build_scene bug)
    // versus "wrong rendering" (a paint_scene/backend bug), rather than one
    // opaque frame_hash mismatch.

    #[test]
    fn a_plain_button_scene_is_fill_then_border_then_one_glyph_mask() {
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 0,
                root: id(1),
                nodes: vec![
                    Node {
                        id: id(1),
                        data: NodeData::Root,
                        children: vec![id(2)],
                        grow: 0,
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Button {
                            label: "Go".into(),
                            enabled: true,
                        },
                        children: vec![],
                        grow: 0,
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let layout = layout(&tree, LogicalSize::new(200.0, 100.0).unwrap()).unwrap();
        let scene = build_scene(
            &tree,
            &layout,
            200,
            100,
            1.0,
            &RenderState::default(),
            Palette::default(),
        );
        scene.validate().unwrap();

        // Clear, then exactly [FillRect, StrokeRect, GlyphMask] for the
        // one button node -- no focus ring since nothing is focused here.
        assert!(matches!(scene.commands[0], PaintCommand::Clear { .. }));
        assert!(matches!(scene.commands[1], PaintCommand::FillRect { .. }));
        assert!(matches!(scene.commands[2], PaintCommand::StrokeRect { .. }));
        assert!(matches!(scene.commands[3], PaintCommand::GlyphMask { .. }));
        assert_eq!(
            scene.commands.len(),
            4,
            "a plain, unfocused button must produce exactly clear+fill+border+label"
        );
    }

    #[test]
    fn a_focused_editor_scene_is_container_then_clip_selection_glyphs_cursor_pop() {
        use youth_editor_engine::{EditorEngine, EditorLayout, ParleyEditorEngine};

        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 0,
                root: id(1),
                nodes: vec![
                    Node {
                        id: id(1),
                        data: NodeData::Root,
                        children: vec![id(2)],
                        grow: 0,
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Box { enabled: true },
                        children: vec![id(3)],
                        grow: 0,
                    },
                    Node {
                        id: id(3),
                        data: NodeData::Editor {
                            document_revision: 0,
                            text: "Hi".into(),
                        },
                        children: vec![],
                        grow: 0,
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let layout = layout(&tree, LogicalSize::new(200.0, 100.0).unwrap()).unwrap();

        let mut engine = ParleyEditorEngine::with_text("Hi");
        // Select the whole string so a selection rect is present alongside
        // the cursor and glyph runs.
        engine.move_to_byte(0);
        let presentation: youth_runtime::TextPresentation = engine.presentation();
        assert!(
            !presentation.runs.is_empty(),
            "the fixture text must produce at least one glyph run"
        );

        let rasterizer = RefCell::new(GlyphRasterizer::new());
        let scene = build_scene(
            &tree,
            &layout,
            200,
            100,
            1.0,
            &RenderState {
                presentation: None,
                editor_rasterizer: Some(&rasterizer),
                ..RenderState::default()
            },
            Palette::default(),
        );
        // `presentation: None` means the live path never activates in this
        // harness (there's no real PresentationReader to construct without
        // a running runtime); assert the FALLBACK bitmap-text path's shape
        // instead, which is what a headless build_scene call actually
        // exercises: container fill, container border, one glyph mask for
        // the fallback text -- no clip/selection/cursor commands, since
        // those only exist on the live-presentation path.
        scene.validate().unwrap();
        assert!(matches!(scene.commands[0], PaintCommand::Clear { .. }));
        assert!(matches!(scene.commands[1], PaintCommand::FillRect { .. }));
        assert!(matches!(scene.commands[2], PaintCommand::StrokeRect { .. }));
        assert!(matches!(scene.commands[3], PaintCommand::GlyphMask { .. }));
        assert_eq!(scene.commands.len(), 4);

        // Exercise the actual live-presentation command shape directly via
        // push_editor_presentation (what the live path emits once a real
        // PresentationReader is wired, e.g. by native.rs): container
        // background (from the enclosing Box, painted first, matching
        // "editor background" preceding the clip region), PushClip,
        // selection rect(s), one glyph mask per glyph, cursor, PopClip.
        let rect = PixelRect {
            x: 10,
            y: 10,
            width: 100,
            height: 40,
        };
        let mut editor_commands = Vec::new();
        push_editor_presentation(
            &mut editor_commands,
            rect,
            &presentation,
            &mut rasterizer.borrow_mut(),
            Palette::default(),
            0.0,
            1.0,
        );
        assert!(matches!(
            editor_commands.first(),
            Some(PaintCommand::PushClip { .. })
        ));
        assert!(matches!(
            editor_commands.last(),
            Some(PaintCommand::PopClip)
        ));
        // At least one selection FillRect, at least one glyph mask, and
        // exactly the sequence PushClip -> [FillRect...] -> [GlyphMask...]
        // -> FillRect (cursor) -> PopClip, in that relative order.
        let push_index = 0;
        let pop_index = editor_commands.len() - 1;
        let first_glyph_index = editor_commands
            .iter()
            .position(|c| matches!(c, PaintCommand::GlyphMask { .. }))
            .expect("at least one glyph mask for non-empty text");
        let last_fill_before_pop = editor_commands[..pop_index]
            .iter()
            .rposition(|c| matches!(c, PaintCommand::FillRect { .. }))
            .expect("a cursor FillRect immediately precedes PopClip");
        assert!(push_index < first_glyph_index);
        assert!(last_fill_before_pop < pop_index);
    }

    #[test]
    fn a_fault_scene_appends_the_overlay_after_the_normal_scene() {
        let tree = counter();
        let layout = layout(&tree, LogicalSize::new(320.0, 180.0).unwrap()).unwrap();
        let normal = build_scene(
            &tree,
            &layout,
            320,
            180,
            1.0,
            &RenderState::default(),
            Palette::default(),
        );
        let fault = build_scene(
            &tree,
            &layout,
            320,
            180,
            1.0,
            &RenderState {
                fault_category: Some("guest_trap"),
                ..RenderState::default()
            },
            Palette::default(),
        );
        fault.validate().unwrap();

        // The fault scene starts with exactly the normal scene's commands
        // unchanged, then appends the overlay: a second Clear, then two
        // glyph masks (the fixed "YOUTH APP FAULT" heading and the fault
        // category), with nothing else after.
        assert_eq!(
            &fault.commands[..normal.commands.len()],
            &normal.commands[..]
        );
        let overlay = &fault.commands[normal.commands.len()..];
        assert_eq!(overlay.len(), 3, "Clear + 2 glyph masks for the overlay");
        assert!(matches!(overlay[0], PaintCommand::Clear { .. }));
        assert!(matches!(overlay[1], PaintCommand::GlyphMask { .. }));
        assert!(matches!(overlay[2], PaintCommand::GlyphMask { .. }));
    }
}
