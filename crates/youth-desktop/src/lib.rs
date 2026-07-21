//! Provisional native presentation for Youth semantic trees.

#![forbid(unsafe_code)]

pub mod geometry;
pub mod raster;

pub use geometry::{
    GeometryError, InteractionKind, LayoutNode, LayoutSnapshot, LogicalPoint, LogicalRect,
    LogicalSize, RendererMirror, hit_test, layout,
};
pub use raster::{FrameBuffer, Palette, RasterError, RenderState, frame_hash, render};
