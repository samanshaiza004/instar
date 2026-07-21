//! Provisional native presentation for Youth semantic trees.

#![forbid(unsafe_code)]

pub mod geometry;

pub use geometry::{
    GeometryError, InteractionKind, LayoutNode, LayoutSnapshot, LogicalPoint, LogicalRect,
    LogicalSize, RendererMirror, hit_test, layout,
};
