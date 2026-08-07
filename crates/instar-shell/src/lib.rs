//! Instar's desktop shell (WP7B2).
//!
//! ```text
//! winit Window ── softbuffer Surface        instar-host
//!      │                   ▲                     │
//!  WindowOutput            │                PaintScene
//!      │              pack 0x00RRGGBB            │
//!      └──> HostBridge ────┴──── Presenter <─────┘
//!                          (Vello CPU)
//! ```
//!
//! This is the only crate that links a window, a renderer, and a font at the
//! same time, and it is deliberately the topmost one: everything below it is
//! testable without a display server, and stays that way because none of it
//! can reach these types.
//!
//! What the shell decides, and nothing below it does:
//!
//! - which paint backend rasterizes a scene, and which face draws its text;
//! - when a frame reaches the screen, which is not the same question as when
//!   the host decided one was needed;
//! - that a frame which cannot be represented is *not presented*, rather than
//!   presented wrong.
//!
//! The last of those is why [`Presenter::render`] hands back a slice instead
//! of drawing: the caller has to be able to fail between rasterizing and
//! presenting, because a partially packed buffer is a torn frame.

#![forbid(unsafe_code)]

pub mod glyphs;
pub mod surface;

use instar_paint::{PaintBackend, PaintError, PaintScene, PhysicalSize, RenderTarget};
use instar_render_vello_cpu::VelloCpuBackend;

pub use glyphs::{FontError, MonoFont, default_font};
pub use surface::SurfaceError;

/// Why a frame did not reach the screen.
#[derive(Debug)]
pub enum PresentError {
    /// The scene could not be rasterized.
    Paint(PaintError),
    /// The pixels could not be handed to the window.
    Surface(SurfaceError),
}

impl std::fmt::Display for PresentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paint(error) => write!(f, "the frame could not be rendered: {error}"),
            Self::Surface(error) => write!(f, "the frame could not be presented: {error}"),
        }
    }
}

impl std::error::Error for PresentError {}

impl From<PaintError> for PresentError {
    fn from(error: PaintError) -> Self {
        Self::Paint(error)
    }
}

impl From<SurfaceError> for PresentError {
    fn from(error: SurfaceError) -> Self {
        Self::Surface(error)
    }
}

/// Rasterizes scenes, reusing everything it can between frames.
///
/// Kept across frames rather than built per redraw: the backend caches its
/// render context, scratch buffers, and converted font handles by size, so a
/// steady stream of same-size frames does no allocation. Rebuilding it each
/// time would throw the glyph atlas away on every click.
pub struct Presenter {
    backend: VelloCpuBackend,
    target: RenderTarget,
}

impl std::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presenter").finish_non_exhaustive()
    }
}

impl Presenter {
    pub fn new(size: PhysicalSize) -> Result<Self, PaintError> {
        Ok(Self {
            backend: VelloCpuBackend::new(),
            target: RenderTarget::new(size)?,
        })
    }

    /// Rasterizes `scene` and returns its premultiplied RGBA8 pixels.
    ///
    /// The opacity check happens *first*, before any rasterization: a scene
    /// that cannot be represented in the window's format is refused while
    /// refusing is still free, and while the previous frame is still intact on
    /// screen.
    pub fn render(&mut self, scene: &PaintScene) -> Result<&[u8], PresentError> {
        surface::check_opacity(scene)?;
        // The scene's own size is authoritative: `instar-host` built it for
        // the metrics it was given, and rendering it against anything else
        // would draw one window's geometry into another's buffer.
        self.backend
            .render_into(scene.size, scene, &mut self.target)?;
        Ok(self.target.pixels())
    }

    /// Rasterizes `scene` and packs it into a window buffer.
    ///
    /// Nothing is presented if this returns an error, and the caller must not
    /// present either — `dst` may hold a torn mix of two frames. See
    /// [`surface::pack`].
    pub fn render_into_window(
        &mut self,
        scene: &PaintScene,
        dst: &mut [u32],
    ) -> Result<(), PresentError> {
        surface::check_opacity(scene)?;
        self.backend
            .render_into(scene.size, scene, &mut self.target)?;
        surface::pack(self.target.pixels(), dst, scene.size)?;
        Ok(())
    }
}
