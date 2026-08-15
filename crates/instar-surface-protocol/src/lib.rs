//! Bounded, dependency-free wire format for an independent [`Surface`](Scene).
//!
//! A scene is derived presentation. It contains no semantic identity, strings,
//! paths, images, or reusable display-list resources. Text commands refer to
//! immutable host layouts supplied alongside the transaction.

#![forbid(unsafe_code)]

use core::fmt;

pub const MAGIC: [u8; 4] = *b"ISF0";
pub const VERSION: u8 = 0;

pub mod limits {
    pub const MAX_SCENE_BYTES: usize = 1 << 20;
    pub const MAX_COMMANDS: usize = 65_535;
    pub const MAX_RESOURCE_REFERENCES: usize = 4_096;
    pub const MAX_CLIP_DEPTH: usize = 64;
    pub const MAX_TRANSFORM_DEPTH: usize = 64;
}

mod opcode {
    pub const FILL_RECT: u8 = 0;
    pub const STROKE_RECT: u8 = 1;
    pub const FILL_ROUNDED_RECT: u8 = 2;
    pub const STROKE_ROUNDED_RECT: u8 = 3;
    pub const PUSH_CLIP: u8 = 4;
    pub const POP_CLIP: u8 = 5;
    pub const PUSH_TRANSFORM: u8 = 6;
    pub const POP_TRANSFORM: u8 = 7;
    pub const DRAW_TEXT_LAYOUT: u8 = 8;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn valid(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 0.0
            && self.height >= 0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Radii {
    pub top_left: f32,
    pub top_right: f32,
    pub bottom_right: f32,
    pub bottom_left: f32,
}

impl Radii {
    fn valid(self) -> bool {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub matrix: [f32; 6],
}

impl Transform {
    pub const IDENTITY: Self = Self {
        matrix: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
    };

    fn valid(self) -> bool {
        self.matrix.into_iter().all(f32::is_finite)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    FillRect {
        rect: Rect,
        color: Color,
    },
    StrokeRect {
        rect: Rect,
        color: Color,
        width: f32,
    },
    FillRoundedRect {
        rect: Rect,
        radii: Radii,
        color: Color,
    },
    StrokeRoundedRect {
        rect: Rect,
        radii: Radii,
        color: Color,
        width: f32,
    },
    PushClip {
        rect: Rect,
    },
    PopClip,
    PushTransform {
        transform: Transform,
    },
    PopTransform,
    DrawTextLayout {
        layout_slot: u16,
        x: f32,
        y: f32,
        color: Color,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scene {
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    TooLarge(usize),
    TooManyCommands(usize),
    TooManyResourceReferences(usize),
    BadMagic,
    UnsupportedVersion(u8),
    Truncated(&'static str),
    TrailingBytes(usize),
    UnknownOpcode(u8),
    NonFiniteOrInvalid(&'static str),
    ResourceSlotOutOfBounds { slot: usize, resources: usize },
    ClipStackUnderflow,
    TransformStackUnderflow,
    ClipDepthExceeded,
    TransformDepthExceeded,
    UnbalancedClipStack(usize),
    UnbalancedTransformStack(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl core::error::Error for Error {}

#[derive(Debug, Default)]
pub struct Encoder {
    commands: Vec<Command>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn command(&mut self, command: Command) -> &mut Self {
        self.commands.push(command);
        self
    }

    pub fn finish(self) -> Result<Vec<u8>, Error> {
        if self.commands.len() > limits::MAX_COMMANDS {
            return Err(Error::TooManyCommands(self.commands.len()));
        }
        let mut out = Vec::with_capacity(9 + self.commands.len() * 24);
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&(self.commands.len() as u32).to_le_bytes());
        for command in self.commands {
            encode_command(&mut out, command)?;
            if out.len() > limits::MAX_SCENE_BYTES {
                return Err(Error::TooLarge(out.len()));
            }
        }
        Ok(out)
    }
}

pub fn decode(bytes: &[u8], resource_count: usize) -> Result<Scene, Error> {
    if bytes.len() > limits::MAX_SCENE_BYTES {
        return Err(Error::TooLarge(bytes.len()));
    }
    if resource_count > limits::MAX_RESOURCE_REFERENCES {
        return Err(Error::TooManyResourceReferences(resource_count));
    }
    let mut reader = Reader { bytes, cursor: 0 };
    if reader.take(4, "magic")? != MAGIC {
        return Err(Error::BadMagic);
    }
    let version = reader.u8("version")?;
    if version != VERSION {
        return Err(Error::UnsupportedVersion(version));
    }
    let count = reader.u32("command count")? as usize;
    if count > limits::MAX_COMMANDS {
        return Err(Error::TooManyCommands(count));
    }
    let mut commands = Vec::with_capacity(count.min(256));
    let mut clip_depth = 0usize;
    let mut transform_depth = 0usize;
    for _ in 0..count {
        let command = decode_command(&mut reader, resource_count)?;
        match command {
            Command::PushClip { .. } => {
                clip_depth += 1;
                if clip_depth > limits::MAX_CLIP_DEPTH {
                    return Err(Error::ClipDepthExceeded);
                }
            }
            Command::PopClip => {
                clip_depth = clip_depth.checked_sub(1).ok_or(Error::ClipStackUnderflow)?;
            }
            Command::PushTransform { .. } => {
                transform_depth += 1;
                if transform_depth > limits::MAX_TRANSFORM_DEPTH {
                    return Err(Error::TransformDepthExceeded);
                }
            }
            Command::PopTransform => {
                transform_depth = transform_depth
                    .checked_sub(1)
                    .ok_or(Error::TransformStackUnderflow)?;
            }
            _ => {}
        }
        commands.push(command);
    }
    if clip_depth != 0 {
        return Err(Error::UnbalancedClipStack(clip_depth));
    }
    if transform_depth != 0 {
        return Err(Error::UnbalancedTransformStack(transform_depth));
    }
    if reader.cursor != bytes.len() {
        return Err(Error::TrailingBytes(bytes.len() - reader.cursor));
    }
    Ok(Scene { commands })
}

fn encode_command(out: &mut Vec<u8>, command: Command) -> Result<(), Error> {
    match command {
        Command::FillRect { rect, color } => {
            out.push(opcode::FILL_RECT);
            rect_out(out, rect)?;
            color_out(out, color);
        }
        Command::StrokeRect { rect, color, width } => {
            out.push(opcode::STROKE_RECT);
            rect_out(out, rect)?;
            color_out(out, color);
            positive(out, width, "stroke width")?;
        }
        Command::FillRoundedRect { rect, radii, color } => {
            out.push(opcode::FILL_ROUNDED_RECT);
            rect_out(out, rect)?;
            radii_out(out, radii)?;
            color_out(out, color);
        }
        Command::StrokeRoundedRect {
            rect,
            radii,
            color,
            width,
        } => {
            out.push(opcode::STROKE_ROUNDED_RECT);
            rect_out(out, rect)?;
            radii_out(out, radii)?;
            color_out(out, color);
            positive(out, width, "stroke width")?;
        }
        Command::PushClip { rect } => {
            out.push(opcode::PUSH_CLIP);
            rect_out(out, rect)?;
        }
        Command::PopClip => out.push(opcode::POP_CLIP),
        Command::PushTransform { transform } => {
            if !transform.valid() {
                return Err(Error::NonFiniteOrInvalid("transform"));
            }
            out.push(opcode::PUSH_TRANSFORM);
            for value in transform.matrix {
                f32_out(out, value);
            }
        }
        Command::PopTransform => out.push(opcode::POP_TRANSFORM),
        Command::DrawTextLayout {
            layout_slot,
            x,
            y,
            color,
        } => {
            if !x.is_finite() || !y.is_finite() {
                return Err(Error::NonFiniteOrInvalid("text origin"));
            }
            out.push(opcode::DRAW_TEXT_LAYOUT);
            out.extend_from_slice(&layout_slot.to_le_bytes());
            f32_out(out, x);
            f32_out(out, y);
            color_out(out, color);
        }
    }
    Ok(())
}

fn decode_command(reader: &mut Reader<'_>, resources: usize) -> Result<Command, Error> {
    Ok(match reader.u8("command opcode")? {
        opcode::FILL_RECT => Command::FillRect {
            rect: reader.rect()?,
            color: reader.color()?,
        },
        opcode::STROKE_RECT => Command::StrokeRect {
            rect: reader.rect()?,
            color: reader.color()?,
            width: reader.positive("stroke width")?,
        },
        opcode::FILL_ROUNDED_RECT => Command::FillRoundedRect {
            rect: reader.rect()?,
            radii: reader.radii()?,
            color: reader.color()?,
        },
        opcode::STROKE_ROUNDED_RECT => Command::StrokeRoundedRect {
            rect: reader.rect()?,
            radii: reader.radii()?,
            color: reader.color()?,
            width: reader.positive("stroke width")?,
        },
        opcode::PUSH_CLIP => Command::PushClip {
            rect: reader.rect()?,
        },
        opcode::POP_CLIP => Command::PopClip,
        opcode::PUSH_TRANSFORM => Command::PushTransform {
            transform: reader.transform()?,
        },
        opcode::POP_TRANSFORM => Command::PopTransform,
        opcode::DRAW_TEXT_LAYOUT => {
            let slot = reader.u16("layout slot")? as usize;
            if slot >= resources {
                return Err(Error::ResourceSlotOutOfBounds { slot, resources });
            }
            let x = reader.f32("text x")?;
            let y = reader.f32("text y")?;
            if !x.is_finite() || !y.is_finite() {
                return Err(Error::NonFiniteOrInvalid("text origin"));
            }
            Command::DrawTextLayout {
                layout_slot: slot as u16,
                x,
                y,
                color: reader.color()?,
            }
        }
        value => return Err(Error::UnknownOpcode(value)),
    })
}

fn f32_out(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn color_out(out: &mut Vec<u8>, color: Color) {
    out.extend_from_slice(&[color.r, color.g, color.b, color.a]);
}
fn positive(out: &mut Vec<u8>, value: f32, what: &'static str) -> Result<(), Error> {
    if !value.is_finite() || value <= 0.0 {
        return Err(Error::NonFiniteOrInvalid(what));
    }
    f32_out(out, value);
    Ok(())
}
fn rect_out(out: &mut Vec<u8>, rect: Rect) -> Result<(), Error> {
    if !rect.valid() {
        return Err(Error::NonFiniteOrInvalid("rectangle"));
    }
    for value in [rect.x, rect.y, rect.width, rect.height] {
        f32_out(out, value);
    }
    Ok(())
}
fn radii_out(out: &mut Vec<u8>, radii: Radii) -> Result<(), Error> {
    if !radii.valid() {
        return Err(Error::NonFiniteOrInvalid("corner radii"));
    }
    for value in [
        radii.top_left,
        radii.top_right,
        radii.bottom_right,
        radii.bottom_left,
    ] {
        f32_out(out, value);
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize, what: &'static str) -> Result<&'a [u8], Error> {
        let end = self
            .cursor
            .checked_add(count)
            .ok_or(Error::Truncated(what))?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or(Error::Truncated(what))?;
        self.cursor = end;
        Ok(value)
    }
    fn u8(&mut self, what: &'static str) -> Result<u8, Error> {
        Ok(self.take(1, what)?[0])
    }
    fn u16(&mut self, what: &'static str) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.take(2, what)?.try_into().unwrap()))
    }
    fn u32(&mut self, what: &'static str) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into().unwrap()))
    }
    fn f32(&mut self, what: &'static str) -> Result<f32, Error> {
        Ok(f32::from_le_bytes(self.take(4, what)?.try_into().unwrap()))
    }
    fn color(&mut self) -> Result<Color, Error> {
        let c = self.take(4, "color")?;
        Ok(Color::rgba(c[0], c[1], c[2], c[3]))
    }
    fn positive(&mut self, what: &'static str) -> Result<f32, Error> {
        let value = self.f32(what)?;
        if value.is_finite() && value > 0.0 {
            Ok(value)
        } else {
            Err(Error::NonFiniteOrInvalid(what))
        }
    }
    fn rect(&mut self) -> Result<Rect, Error> {
        let rect = Rect::new(
            self.f32("rect x")?,
            self.f32("rect y")?,
            self.f32("rect width")?,
            self.f32("rect height")?,
        );
        if rect.valid() {
            Ok(rect)
        } else {
            Err(Error::NonFiniteOrInvalid("rectangle"))
        }
    }
    fn radii(&mut self) -> Result<Radii, Error> {
        let radii = Radii {
            top_left: self.f32("radius")?,
            top_right: self.f32("radius")?,
            bottom_right: self.f32("radius")?,
            bottom_left: self.f32("radius")?,
        };
        if radii.valid() {
            Ok(radii)
        } else {
            Err(Error::NonFiniteOrInvalid("corner radii"))
        }
    }
    fn transform(&mut self) -> Result<Transform, Error> {
        let transform = Transform {
            matrix: [
                self.f32("transform")?,
                self.f32("transform")?,
                self.f32("transform")?,
                self.f32("transform")?,
                self.f32("transform")?,
                self.f32("transform")?,
            ],
        };
        if transform.valid() {
            Ok(transform)
        } else {
            Err(Error::NonFiniteOrInvalid("transform"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_command() {
        let rect = Rect::new(1.0, 2.0, 30.0, 40.0);
        let radii = Radii {
            top_left: 1.0,
            top_right: 2.0,
            bottom_right: 3.0,
            bottom_left: 4.0,
        };
        let color = Color::rgba(1, 2, 3, 4);
        let commands = vec![
            Command::FillRect { rect, color },
            Command::StrokeRect {
                rect,
                color,
                width: 2.0,
            },
            Command::FillRoundedRect { rect, radii, color },
            Command::StrokeRoundedRect {
                rect,
                radii,
                color,
                width: 3.0,
            },
            Command::PushClip { rect },
            Command::PushTransform {
                transform: Transform::IDENTITY,
            },
            Command::DrawTextLayout {
                layout_slot: 0,
                x: 5.0,
                y: 6.0,
                color,
            },
            Command::PopTransform,
            Command::PopClip,
        ];
        let mut encoder = Encoder::new();
        for command in &commands {
            encoder.command(command.clone());
        }
        assert_eq!(
            decode(&encoder.finish().unwrap(), 1).unwrap().commands,
            commands
        );
    }

    #[test]
    fn mutation_never_panics() {
        let mut encoder = Encoder::new();
        encoder.command(Command::FillRect {
            rect: Rect::new(0.0, 0.0, 1.0, 1.0),
            color: Color::rgba(0, 0, 0, 255),
        });
        let valid = encoder.finish().unwrap();
        for index in 0..valid.len() {
            for bit in [1u8, 0x80] {
                let mut mutant = valid.clone();
                mutant[index] ^= bit;
                let _ = decode(&mutant, 0);
            }
        }
        for end in 0..valid.len() {
            assert!(decode(&valid[..end], 0).is_err());
        }
    }

    #[test]
    fn stacks_and_slots_are_validated() {
        let mut encoder = Encoder::new();
        encoder.command(Command::PopClip);
        assert_eq!(
            decode(&encoder.finish().unwrap(), 0),
            Err(Error::ClipStackUnderflow)
        );
        let mut encoder = Encoder::new();
        encoder.command(Command::DrawTextLayout {
            layout_slot: 1,
            x: 0.0,
            y: 0.0,
            color: Color::rgba(0, 0, 0, 0),
        });
        assert_eq!(
            decode(&encoder.finish().unwrap(), 1),
            Err(Error::ResourceSlotOutOfBounds {
                slot: 1,
                resources: 1
            })
        );
    }
}
