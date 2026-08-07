//! Instar's OS windowing layer: windows in, logical-coordinate events out.
//!
//! # What this crate is not allowed to know
//!
//! It does not know what a `NodeKey` is, what a tree revision means, or how to
//! hit-test. It never depends on `instar-ui` — there is a test that fails if
//! that edge ever appears. Winit is window and event infrastructure; widget
//! routing belongs above it, and letting windowing creep upward is how a
//! windowing layer slowly becomes a GUI framework.
//!
//! ```text
//! winit WindowEvent
//!       |
//! instar-window        <- you are here: normalize OS coordinates
//!       |
//! instar-host          <- bridges logical presentation to physical rendering
//!       |
//! instar-ui            <- hit-test, disabled rules, interaction state
//! ```
//!
//! # DPI: converted here, but not hidden from the host
//!
//! Winit reports cursor positions in *physical* pixels and expects the
//! application to convert using the window's current scale factor, which can
//! change at runtime when a window moves between monitors or the user changes
//! display settings. This crate owns that conversion and emits logical
//! coordinates.
//!
//! It does **not** follow that scale factor is a secret. The host needs it:
//! a renderer rasterizes into a physical target, and text quality depends on
//! knowing the real device pixels. So scale travels upward explicitly in
//! [`WindowMetricsChanged`], and the division of labour is:
//!
//! | Layer | Sees |
//! |---|---|
//! | `instar-window` | physical and logical; owns the conversion |
//! | `instar-host` | logical viewport for UI, physical target + scale for the renderer |
//! | `instar-ui` | logical only, never a scale factor |
//!
//! UI semantics and hit-testing stay scale-free. Presentation does not.
//!
//! # Scale changes are applied before the next event is translated
//!
//! [`WindowState`] updates its stored scale factor as part of handling a scale
//! change, so any pointer event translated afterwards uses the new factor.
//! Getting this wrong produces a rare, miserable bug: a handful of clicks
//! landing at stale coordinates right after a monitor switch.

#![forbid(unsafe_code)]

pub mod driver;
pub mod winit_adapter;

pub use driver::WindowDriver;

pub use winit::window::WindowId;

/// A point in logical pixels — device-independent, scale-free, and the only
/// coordinate space anything above this crate should be using for input.
///
/// `f64` rather than integers because that is what the conversion actually
/// produces, and rounding is the consumer's decision: hit-testing wants
/// integers, but a future drag or gesture would want the precision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalPoint {
    pub x: f64,
    pub y: f64,
}

impl LogicalPoint {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Rounds to integer logical pixels, for consumers like hit-testing that
    /// work in whole pixels.
    pub fn round(self) -> (i32, i32) {
        (self.x.round() as i32, self.y.round() as i32)
    }
}

/// A size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalSize {
    pub width: f64,
    pub height: f64,
}

/// A size in physical device pixels — what a renderer actually targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

/// Which pointer button, named by role rather than by handedness.
///
/// Instar's own enum rather than winit's, so nothing above this crate depends
/// on winit types. "Primary" also survives a left-handed mouse configuration
/// in a way "Left" does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
    Other(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerState {
    Pressed,
    Released,
}

/// A pointer event, already converted to logical coordinates.
///
/// Raw in the sense that it carries no interpretation: no target node, no
/// notion of what was clicked. Resolving that is `instar-ui`'s job, reached
/// through `instar-host`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawPointerEvent {
    pub window_id: WindowId,
    pub logical_pos: LogicalPoint,
    pub button: PointerButton,
    pub state: PointerState,
}

/// The window's geometry changed: resized, moved between monitors, or the
/// user changed display scaling.
///
/// Carries all three facts together on purpose. A host that learned about a
/// resize and a scale change separately would have a window in between where
/// its viewport and its render target disagree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowMetricsChanged {
    pub window_id: WindowId,
    pub logical_size: LogicalSize,
    pub physical_size: PhysicalSize,
    pub scale_factor: f64,
}

/// Everything this crate emits upward.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowOutput {
    Pointer(RawPointerEvent),
    MetricsChanged(WindowMetricsChanged),
    /// The user asked to close the window; the host decides what that means.
    CloseRequested {
        window_id: WindowId,
    },
    RedrawRequested {
        window_id: WindowId,
    },
}

/// Per-window translation state.
///
/// Deliberately free of winit event types so it can be tested without a
/// display server, which matters: this is where the DPI bugs live, and CI has
/// no monitors. The winit plumbing in [`winit_adapter`] is a thin shell over
/// these methods.
#[derive(Debug, Clone)]
pub struct WindowState {
    window_id: WindowId,
    scale_factor: f64,
    physical_size: PhysicalSize,
    /// Winit's `MouseInput` does not carry a position, so the last position
    /// from `CursorMoved` is what a press or release is attributed to. That is
    /// how winit expects this to work, not a shortcut.
    last_cursor: Option<LogicalPoint>,
}

impl WindowState {
    pub fn new(window_id: WindowId, scale_factor: f64, physical_size: PhysicalSize) -> Self {
        Self {
            window_id,
            scale_factor: sanitize_scale(scale_factor),
            physical_size,
            last_cursor: None,
        }
    }

    pub fn window_id(&self) -> WindowId {
        self.window_id
    }

    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    pub fn physical_size(&self) -> PhysicalSize {
        self.physical_size
    }

    pub fn logical_size(&self) -> LogicalSize {
        LogicalSize {
            width: f64::from(self.physical_size.width) / self.scale_factor,
            height: f64::from(self.physical_size.height) / self.scale_factor,
        }
    }

    pub fn last_cursor(&self) -> Option<LogicalPoint> {
        self.last_cursor
    }

    /// Converts a physical position to logical using the *current* scale.
    pub fn to_logical(&self, physical_x: f64, physical_y: f64) -> LogicalPoint {
        LogicalPoint::new(
            physical_x / self.scale_factor,
            physical_y / self.scale_factor,
        )
    }

    /// The window was resized. Scale is unchanged.
    pub fn on_resized(&mut self, physical_size: PhysicalSize) -> WindowMetricsChanged {
        self.physical_size = physical_size;
        self.metrics()
    }

    /// The scale factor changed — a monitor switch or a display-settings
    /// change.
    ///
    /// The stored factor is updated *before* this returns, so every subsequent
    /// translation uses the new one. Winit also supplies the new physical size
    /// alongside, because the two change together and applying one without the
    /// other leaves a frame of inconsistent geometry.
    pub fn on_scale_factor_changed(
        &mut self,
        scale_factor: f64,
        physical_size: PhysicalSize,
    ) -> WindowMetricsChanged {
        self.scale_factor = sanitize_scale(scale_factor);
        self.physical_size = physical_size;

        // The cursor has not moved in physical space, but the logical position
        // recorded for it was computed under the old factor and is now wrong.
        // Discard it rather than rescale it: winit sends a fresh `CursorMoved`
        // when the pointer next moves, and attributing a click to a
        // back-computed position is the kind of "nearly right" behaviour that
        // hides DPI bugs instead of surfacing them.
        self.last_cursor = None;

        self.metrics()
    }

    /// The cursor moved, in physical coordinates.
    pub fn on_cursor_moved(&mut self, physical_x: f64, physical_y: f64) -> LogicalPoint {
        let logical = self.to_logical(physical_x, physical_y);
        self.last_cursor = Some(logical);
        logical
    }

    pub fn on_cursor_left(&mut self) {
        self.last_cursor = None;
    }

    /// A button was pressed or released at the last known cursor position.
    ///
    /// Returns `None` when no position is known — a button event with no
    /// cursor is not something to guess a location for.
    pub fn on_mouse_input(
        &self,
        button: PointerButton,
        state: PointerState,
    ) -> Option<RawPointerEvent> {
        Some(RawPointerEvent {
            window_id: self.window_id,
            logical_pos: self.last_cursor?,
            button,
            state,
        })
    }

    pub fn metrics(&self) -> WindowMetricsChanged {
        WindowMetricsChanged {
            window_id: self.window_id,
            logical_size: self.logical_size(),
            physical_size: self.physical_size,
            scale_factor: self.scale_factor,
        }
    }
}

/// Guards against a scale factor that would make conversion meaningless.
///
/// A zero or negative factor would divide coordinates into infinity or flip
/// them; a NaN would poison every comparison downstream. None should ever
/// arrive from a sane platform, which is exactly why silently producing
/// garbage instead of falling back would be so hard to diagnose.
fn sanitize_scale(scale_factor: f64) -> f64 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(scale: f64) -> WindowState {
        // `WindowId` cannot be constructed without a window on all platforms,
        // so tests go through winit's documented dummy id.
        WindowState::new(
            WindowId::dummy(),
            scale,
            PhysicalSize {
                width: 800,
                height: 600,
            },
        )
    }

    #[test]
    fn converts_physical_to_logical() {
        let state = state(2.0);
        assert_eq!(
            state.to_logical(200.0, 100.0),
            LogicalPoint::new(100.0, 50.0)
        );
        assert_eq!(
            state.logical_size(),
            LogicalSize {
                width: 400.0,
                height: 300.0
            }
        );
    }

    #[test]
    fn a_scale_of_one_is_the_identity() {
        let state = state(1.0);
        assert_eq!(state.to_logical(37.0, 91.0), LogicalPoint::new(37.0, 91.0));
    }

    #[test]
    fn fractional_scaling_round_trips_sensibly() {
        // 1.5 is the common Windows and GNOME case, and the one where naive
        // integer maths goes wrong.
        let state = state(1.5);
        assert_eq!(
            state.to_logical(150.0, 75.0),
            LogicalPoint::new(100.0, 50.0)
        );
        assert_eq!(state.to_logical(151.0, 0.0).round().0, 101);
    }

    /// The invariant: a scale change is applied before anything else is
    /// translated.
    #[test]
    fn scale_changes_apply_to_the_very_next_event() {
        let mut state = state(1.0);
        assert_eq!(
            state.on_cursor_moved(200.0, 100.0),
            LogicalPoint::new(200.0, 100.0)
        );

        let metrics = state.on_scale_factor_changed(
            2.0,
            PhysicalSize {
                width: 1600,
                height: 1200,
            },
        );
        assert_eq!(metrics.scale_factor, 2.0);
        assert_eq!(
            metrics.logical_size,
            LogicalSize {
                width: 800.0,
                height: 600.0
            }
        );

        // The very next pointer event must use 2.0, not the old 1.0.
        assert_eq!(
            state.on_cursor_moved(200.0, 100.0),
            LogicalPoint::new(100.0, 50.0),
            "a pointer event after a scale change must use the new factor"
        );
    }

    #[test]
    fn a_stale_cursor_is_dropped_across_a_scale_change() {
        let mut state = state(1.0);
        state.on_cursor_moved(200.0, 100.0);
        state.on_scale_factor_changed(
            2.0,
            PhysicalSize {
                width: 1600,
                height: 1200,
            },
        );
        // Rather than report a position computed under the old factor, there
        // is simply no position until the next move.
        assert_eq!(
            state.on_mouse_input(PointerButton::Primary, PointerState::Pressed),
            None,
            "a click must not be attributed to a position computed at the old scale"
        );
    }

    #[test]
    fn resizing_keeps_the_scale_factor() {
        let mut state = state(2.0);
        let metrics = state.on_resized(PhysicalSize {
            width: 400,
            height: 200,
        });
        assert_eq!(metrics.scale_factor, 2.0);
        assert_eq!(
            metrics.logical_size,
            LogicalSize {
                width: 200.0,
                height: 100.0
            }
        );
        assert_eq!(
            metrics.physical_size,
            PhysicalSize {
                width: 400,
                height: 200
            }
        );
    }

    #[test]
    fn button_events_use_the_last_cursor_position() {
        let mut state = state(2.0);
        state.on_cursor_moved(60.0, 40.0);
        let event = state
            .on_mouse_input(PointerButton::Primary, PointerState::Pressed)
            .expect("cursor position is known");
        assert_eq!(event.logical_pos, LogicalPoint::new(30.0, 20.0));
        assert_eq!(event.button, PointerButton::Primary);
        assert_eq!(event.state, PointerState::Pressed);
    }

    #[test]
    fn a_button_without_a_known_cursor_is_dropped() {
        let state = state(1.0);
        assert_eq!(
            state.on_mouse_input(PointerButton::Primary, PointerState::Pressed),
            None
        );
    }

    #[test]
    fn the_cursor_leaving_clears_the_position() {
        let mut state = state(1.0);
        state.on_cursor_moved(10.0, 10.0);
        state.on_cursor_left();
        assert_eq!(
            state.on_mouse_input(PointerButton::Primary, PointerState::Released),
            None
        );
    }

    #[test]
    fn nonsensical_scale_factors_fall_back_to_one() {
        for bad in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            let state = state(bad);
            assert_eq!(
                state.scale_factor(),
                1.0,
                "a scale factor of {bad} should fall back to 1.0 rather than \
                 produce infinite or mirrored coordinates"
            );
        }
    }
}
