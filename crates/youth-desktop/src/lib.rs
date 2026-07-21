//! Provisional native presentation for Youth semantic trees.

#![forbid(unsafe_code)]

pub mod controller;
pub mod geometry;
pub mod input;
pub mod raster;

pub use controller::{
    AppErrorSummary, Controller, ControllerCommand, DesktopEvent, DesktopEventSink,
    RuntimeErrorSummary,
};
pub use geometry::{
    GeometryError, InteractionKind, LayoutNode, LayoutSnapshot, LogicalPoint, LogicalRect,
    LogicalSize, RendererMirror, hit_test, layout,
};
pub use input::{InputChange, PointerState};
pub use raster::{FrameBuffer, Palette, RasterError, RenderState, frame_hash, render};
