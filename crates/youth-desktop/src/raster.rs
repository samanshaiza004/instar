use thiserror::Error;
use youth_tree::{NodeData, NodeId, Tree};

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
            fault_background: 0x0048_1820,
            fault_text: 0x00ff_d5d9,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderState<'a> {
    pub hovered: Option<NodeId>,
    pub pressed: Option<NodeId>,
    pub fault_category: Option<&'a str>,
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

    fn fill(&mut self, rect: PixelRect, color: u32) {
        let x_end = rect.x.saturating_add(rect.width).min(self.width);
        let y_end = rect.y.saturating_add(rect.height).min(self.height);
        for y in rect.y.min(self.height)..y_end {
            let row = y as usize * self.width as usize;
            for x in rect.x.min(self.width)..x_end {
                self.pixels[row + x as usize] = color;
            }
        }
    }

    fn border(&mut self, rect: PixelRect, color: u32) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.fill(PixelRect { height: 1, ..rect }, color);
        self.fill(
            PixelRect {
                y: rect.y.saturating_add(rect.height.saturating_sub(1)),
                height: 1,
                ..rect
            },
            color,
        );
        self.fill(PixelRect { width: 1, ..rect }, color);
        self.fill(
            PixelRect {
                x: rect.x.saturating_add(rect.width.saturating_sub(1)),
                width: 1,
                ..rect
            },
            color,
        );
    }

    fn text(&mut self, x: u32, y: u32, value: &str, color: u32, scale: u32) {
        let scale = scale.max(1);
        let mut cursor = x;
        for character in value.chars() {
            let rows = glyph_rows(character);
            for (row, bits) in rows.into_iter().enumerate() {
                for column in 0..5 {
                    if bits & (1 << (4 - column)) != 0 {
                        self.fill(
                            PixelRect {
                                x: cursor.saturating_add(column * scale),
                                y: y.saturating_add(row as u32 * scale),
                                width: scale,
                                height: scale,
                            },
                            color,
                        );
                    }
                }
            }
            cursor = cursor.saturating_add(8 * scale);
        }
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
}

pub fn render(
    tree: &Tree,
    layout: &LayoutSnapshot,
    physical_width: u32,
    physical_height: u32,
    scale_factor: f64,
    state: &RenderState<'_>,
    palette: Palette,
) -> Result<FrameBuffer, RasterError> {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err(RasterError::InvalidScale);
    }
    if tree.revision() != layout.tree_revision {
        return Err(RasterError::RevisionMismatch);
    }
    let mut frame = FrameBuffer::new(physical_width, physical_height)?;
    frame.clear(palette.background);
    for (id, node) in &layout.nodes {
        let Some(semantic) = tree.node(*id) else {
            continue;
        };
        let rect = physical_rect(node.bounds, scale_factor, physical_width, physical_height);
        match &semantic.data {
            NodeData::Root => {}
            NodeData::Box { .. } => {
                frame.fill(rect, palette.container);
                frame.border(rect, palette.border);
            }
            NodeData::Text { value } => {
                frame.text(
                    rect.x,
                    rect.y,
                    value,
                    palette.text,
                    scale_factor.round() as u32,
                );
            }
            NodeData::Button { label, .. } => {
                let color = if !node.effective_enabled {
                    palette.button_disabled
                } else if state.pressed == Some(*id) {
                    palette.button_pressed
                } else if state.hovered == Some(*id) {
                    palette.button_hover
                } else {
                    palette.button
                };
                frame.fill(rect, color);
                frame.border(rect, palette.border);
                let inset_x = (12.0 * scale_factor).floor().max(0.0) as u32;
                let inset_y = (8.0 * scale_factor).floor().max(0.0) as u32;
                frame.text(
                    rect.x.saturating_add(inset_x),
                    rect.y.saturating_add(inset_y),
                    label,
                    palette.text,
                    scale_factor.round() as u32,
                );
            }
        }
    }
    if let Some(category) = state.fault_category {
        frame.clear(palette.fault_background);
        frame.text(16, 16, "YOUTH APP FAULT", palette.fault_text, 1);
        frame.text(16, 32, category, palette.fault_text, 1);
    }
    Ok(frame)
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

fn glyph_rows(character: char) -> [u8; 7] {
    match character.to_ascii_uppercase() {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        ':' => [0, 4, 4, 0, 4, 4, 0],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        ' ' => [0; 7],
        _ => [31, 17, 2, 4, 4, 0, 4],
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
                    },
                    Node {
                        id: id(2),
                        data: NodeData::Box { enabled: true },
                        children: vec![id(3), id(4)],
                    },
                    Node {
                        id: id(3),
                        data: NodeData::Text {
                            value: "Count: 0".into(),
                        },
                        children: vec![],
                    },
                    Node {
                        id: id(4),
                        data: NodeData::Button {
                            label: "Increment".into(),
                            enabled: true,
                        },
                        children: vec![],
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap()
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
                fault_category: None,
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
        assert_eq!(
            [
                frame_hash(&normal),
                frame_hash(&hover),
                frame_hash(&pressed),
                frame_hash(&fault)
            ],
            [
                17_820_981_758_339_064_687,
                14_595_224_954_947_096_910,
                10_747_383_323_860_802_854,
                10_205_311_049_527_053_737,
            ]
        );
    }

    #[test]
    fn framebuffer_limits_and_zero_size_are_safe() {
        assert!(FrameBuffer::new(0, 0).is_ok());
        assert!(FrameBuffer::new(u32::MAX, u32::MAX).is_err());
    }
}
