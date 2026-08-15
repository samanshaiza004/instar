//! Opaque bridge vocabulary for host-owned layouts and Surface scenes.
//!
//! The kernel transports generation-scoped capabilities. It cannot inspect a
//! shaped layout or decode a scene; those operations belong to the main-thread
//! presentation owner behind [`PresentationSink`].

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::oneshot;

use crate::runtime::GenerationId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpaqueLayoutKey {
    pub slot: u32,
    pub incarnation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestTextLayout {
    pub key: OpaqueLayoutKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeAffinity {
    Downstream,
    Upstream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeCursor {
    pub index: u32,
    pub affinity: BridgeAffinity,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BridgeRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BridgeMetrics {
    pub width: f32,
    pub height: f32,
    pub lines: u32,
    pub clusters: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeFontRole {
    SystemUi,
    Monospace,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeAlignment {
    Start,
    Center,
    End,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BridgeLineHeight {
    MetricsRelative(f32),
    FontSizeRelative(f32),
    Absolute(f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BridgeLayoutStyle {
    pub role: BridgeFontRole,
    pub size: f32,
    pub weight: u16,
    pub wrap: bool,
    pub line_height: BridgeLineHeight,
    pub width: Option<f32>,
    pub alignment: BridgeAlignment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutQuery {
    Metrics,
    CursorFromPoint {
        x_bits: u32,
        y_bits: u32,
    },
    CaretRect {
        cursor: BridgeCursor,
        width_bits: u32,
    },
    SelectionRects {
        anchor: BridgeCursor,
        focus: BridgeCursor,
    },
    PreviousVisual(BridgeCursor),
    NextVisual(BridgeCursor),
    VisualLineStart(BridgeCursor),
    VisualLineEnd(BridgeCursor),
    HardLineStart(BridgeCursor),
    HardLineEnd(BridgeCursor),
    PreviousWord(BridgeCursor),
    NextWord(BridgeCursor),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PresentationOperation {
    CreateLayout {
        text: String,
        style: BridgeLayoutStyle,
    },
    QueryLayout {
        key: OpaqueLayoutKey,
        query: LayoutQuery,
    },
    ReleaseLayout {
        key: OpaqueLayoutKey,
    },
    UpdateSurface {
        target: (u32, u32),
        scene: Vec<u8>,
        layouts: Vec<OpaqueLayoutKey>,
    },
    CapturePointer {
        target: (u32, u32),
    },
    ReleasePointer {
        target: (u32, u32),
    },
    RequestFocus {
        target: (u32, u32),
    },
    ConfigureTextInput {
        target: (u32, u32),
        enabled: bool,
        rect: BridgeRect,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PresentationAnswer {
    Layout(OpaqueLayoutKey),
    Metrics(BridgeMetrics),
    Cursor(BridgeCursor),
    Rect(BridgeRect),
    Rects(Vec<BridgeRect>),
    Revision(u64),
    Unit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentationRefusal {
    StaleGeneration,
    HostUnavailable,
    TextTooLarge(u32),
    InvalidStyle,
    TooManyLines(u32),
    TooManyClusters(u32),
    InvalidCursor(u32),
    TooManySelectionRects(u32),
    TooManyLiveLayouts(u32),
    NoSuchLayout,
    NoSuchSurface,
    UpdateInProgress,
    SceneTooLarge(u32),
    TooManyLayouts(u32),
    InvalidScene(String),
    NotFocusable,
    NotInterested,
}

pub struct PresentationRequest {
    generation: GenerationId,
    operation: PresentationOperation,
    reply: oneshot::Sender<Result<PresentationAnswer, PresentationRefusal>>,
    in_flight: Option<SurfaceGuard>,
}

impl std::fmt::Debug for PresentationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresentationRequest")
            .field("generation", &self.generation)
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

pub type SurfaceInFlight = Arc<Mutex<HashSet<(GenerationId, u32, u32)>>>;

struct SurfaceGuard {
    set: SurfaceInFlight,
    key: (GenerationId, u32, u32),
}
pub struct RequestGuard(Option<SurfaceGuard>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}

impl Drop for SurfaceGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .expect("surface in-flight set poisoned")
            .remove(&self.key);
    }
}

impl PresentationRequest {
    pub fn generation(&self) -> GenerationId {
        self.generation
    }
    pub fn try_mark_surface(&mut self, set: SurfaceInFlight) -> bool {
        let PresentationOperation::UpdateSurface { target, .. } = &self.operation else {
            return true;
        };
        let key = (self.generation, target.0, target.1);
        let inserted = set
            .lock()
            .expect("surface in-flight set poisoned")
            .insert(key);
        if inserted {
            self.in_flight = Some(SurfaceGuard { set, key });
        }
        inserted
    }
    pub fn refuse(self, refusal: PresentationRefusal) {
        let _ = self.reply.send(Err(refusal));
    }
    pub fn into_parts(
        self,
    ) -> (
        GenerationId,
        PresentationOperation,
        oneshot::Sender<Result<PresentationAnswer, PresentationRefusal>>,
        RequestGuard,
    ) {
        (
            self.generation,
            self.operation,
            self.reply,
            RequestGuard(self.in_flight),
        )
    }
}

pub fn request(
    generation: GenerationId,
    operation: PresentationOperation,
) -> (
    PresentationRequest,
    oneshot::Receiver<Result<PresentationAnswer, PresentationRefusal>>,
) {
    let (reply, answer) = oneshot::channel();
    (
        PresentationRequest {
            generation,
            operation,
            reply,
            in_flight: None,
        },
        answer,
    )
}

pub trait PresentationSink: Send + Sync + 'static {
    fn submit(&self, request: PresentationRequest) -> Result<(), PresentationRequest>;
}

pub type SharedPresentationSink = Arc<dyn PresentationSink>;
