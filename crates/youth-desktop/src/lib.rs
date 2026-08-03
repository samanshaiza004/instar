//! Provisional native presentation for Youth semantic trees.

#![forbid(unsafe_code)]

pub mod access;
pub mod controller;
pub mod document_picker;
pub mod geometry;
pub mod input;
pub mod native;
pub mod raster;

pub use controller::{
    AppErrorSummary, Controller, ControllerCommand, DesktopEvent, DesktopEventSink,
    RuntimeErrorSummary,
};
pub use document_picker::{
    DialogError, DocumentPicker, DocumentPickerFuture, DocumentPickerResult, FixedDocumentPicker,
    RecordingDocumentPicker, RfdDocumentPicker,
};
pub use geometry::{
    GeometryError, InteractionKind, LayoutNode, LayoutSnapshot, LogicalPoint, LogicalRect,
    LogicalSize, RendererMirror, hit_test, layout,
};
pub use input::{InputChange, PointerState};
pub use native::{
    CapsuleLaunchOptions, DesktopError, DesktopOptions, document_picker_smoke, run,
    run_capsule_launch, run_with_shutdown, window_smoke,
};
pub use raster::{FrameBuffer, Palette, RasterError, RenderState, frame_hash, render};
pub use youth_interaction::{
    EditorInput, EditorMovement, InteractionChange, InteractionSnapshot, InteractionState,
    LogicalKey, Modifiers, SemanticAction,
};
