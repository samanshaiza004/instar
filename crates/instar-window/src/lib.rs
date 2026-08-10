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

use core::fmt;

pub mod driver;
pub mod winit_adapter;

pub use driver::{HostDecision, WindowDriver};

/// Identifies a window, without exposing winit's type for it.
///
/// `instar-window` is the only crate whose public vocabulary may contain winit
/// types, and this is what keeps that true: an opaque token the host and a
/// future alternate window backend can both use, and that headless tests can
/// construct without a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(u64);

impl WindowId {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "window{}", self.0)
    }
}

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

/// How far a wheel or touchpad asked to scroll, and in what units.
///
/// The two are kept apart because they are genuinely different facts. A pixel
/// delta is a distance and can be converted at the window boundary like any
/// other. A line delta is a *count* — how far a line is is a UI policy
/// question, and answering it here would put typography in the windowing
/// layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollDelta {
    /// Logical pixels, already converted from physical at this boundary.
    Logical { x: f64, y: f64 },
    /// Whole lines, as a wheel notch reports them. `instar-ui` decides how
    /// far a line is.
    Lines { x: f64, y: f64 },
}

/// A wheel or touchpad scroll, in logical coordinates.
///
/// # The sign convention is settled here and nowhere else
///
/// > `+y` means **increase the scroll offset**, which reveals content further
/// > down.
///
/// Platform wheel direction, natural-scrolling preferences, and winit's own
/// conventions are resolved in this crate and never travel inward. Retained UI
/// that has to ask which way `+y` points on this operating system is retained
/// UI with a platform leak in it.
///
/// Like [`RawPointerEvent`], this carries no interpretation: no target node,
/// no viewport, no offset. Which `Scroll` this reaches — if any — is
/// `instar-ui`'s to decide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawScrollEvent {
    pub window_id: WindowId,
    pub logical_pos: LogicalPoint,
    pub delta: ScrollDelta,
}

/// The keys Instar's retained UI vocabulary responds to.
///
/// Deliberately not a general keyboard mapping. These are the keys that mean
/// something to a button and to focus traversal; character input, editing
/// shortcuts and IME belong to Phase 3's text service, which needs a far
/// richer contract than an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Tab,
    Enter,
    Space,
    Escape,
    /// Anything else. Carried so the host can see that a key happened without
    /// this crate growing an opinion about what it was.
    Other,
}

/// A key going down or coming up.
///
/// Like [`RawPointerEvent`], it carries no interpretation: no focused node, no
/// activation. Which control this reaches, if any, is `instar-ui`'s to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawKeyEvent {
    pub window_id: WindowId,
    pub key: Key,
    /// `true` on press, `false` on release. Both are delivered, because a
    /// button held with Space is pressed-looking for as long as it is held.
    pub pressed: bool,
    pub shift: bool,
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
    Scroll(RawScrollEvent),
    Key(RawKeyEvent),
    MetricsChanged(WindowMetricsChanged),
    /// The window's presentation geometry is temporarily unusable.
    ///
    /// Emitted when the scale factor changes, before a coherent
    /// scale-and-size pair is known. It deliberately **carries no size and no
    /// scale**: its entire meaning is *"whatever you last computed from
    /// geometry is now invalid"*. Adding values would invite a host to use
    /// them, which is precisely what this exists to prevent.
    ///
    /// It opens a barrier that the matching [`WindowOutput::MetricsChanged`]
    /// closes. While it is open, a host must not:
    ///
    /// - render, or
    /// - hit-test or activate anything from pointer input.
    ///
    /// The second half matters as much as the first and is easier to miss: a
    /// cursor position converted with the *new* scale is still meaningless
    /// against a layout computed for the *old* logical viewport, so a click
    /// during the barrier would resolve to the wrong node rather than to
    /// nothing. Retaining the latest pointer position is fine; acting on it is
    /// not.
    ///
    /// Close requests and other native lifecycle events are unaffected —
    /// none of them depend on geometry, and a window that cannot be closed
    /// during a monitor switch would be a worse bug than the one this
    /// prevents.
    ///
    /// Enforcement belongs to the host: this crate signals, `instar-host`
    /// obeys. `instar-window` has no idea what a render or a hit-test is.
    MetricsInvalidated {
        window_id: WindowId,
    },
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
    /// Set when the scale factor changed but the matching physical size has
    /// not arrived yet. See [`WindowState::on_scale_factor_changed`].
    metrics_pending: bool,
}

impl WindowState {
    pub fn new(window_id: WindowId, scale_factor: f64, physical_size: PhysicalSize) -> Self {
        Self {
            window_id,
            scale_factor: sanitize_scale(scale_factor),
            physical_size,
            last_cursor: None,
            metrics_pending: false,
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
    ///
    /// Also resolves any metrics left pending by a scale change: this is the
    /// authoritative new size, so the pair is now coherent.
    pub fn on_resized(&mut self, physical_size: PhysicalSize) -> WindowMetricsChanged {
        self.physical_size = physical_size;
        self.metrics_pending = false;
        self.metrics()
    }

    /// Whether a scale change is still waiting for its physical size.
    pub fn metrics_pending(&self) -> bool {
        self.metrics_pending
    }

    /// Resolves pending metrics against a size queried from the window.
    ///
    /// Called at the end of an event cycle. Winit applies the OS-suggested
    /// size after the scale-change callback unless the application overrides
    /// it, so querying `inner_size()` once the cycle settles yields a size that
    /// genuinely matches the new scale — even on a platform where no separate
    /// `Resized` arrives.
    ///
    /// Returns `None` when nothing is pending, so the caller can invoke this
    /// unconditionally.
    pub fn take_pending_metrics(
        &mut self,
        physical_size: PhysicalSize,
    ) -> Option<WindowMetricsChanged> {
        if !self.metrics_pending {
            return None;
        }
        self.physical_size = physical_size;
        self.metrics_pending = false;
        Some(self.metrics())
    }

    /// The scale factor changed — a monitor switch or a display-settings
    /// change.
    ///
    /// The stored factor is updated *before* this returns, so every subsequent
    /// translation uses the new one.
    ///
    /// Deliberately emits **nothing**. Winit reports the new scale alongside an
    /// `InnerSizeWriter` rather than the resulting physical size, and a
    /// following `Resized` is not a documented cross-platform guarantee.
    /// Emitting here would therefore mean publishing a new scale paired with a
    /// stale size — metrics that are individually true and jointly wrong, which
    /// a renderer sizing a surface would act on. Instead the metrics are marked
    /// pending and flushed once a coherent size is known, by either
    /// [`WindowState::on_resized`] or [`WindowState::take_pending_metrics`].
    pub fn on_scale_factor_changed(&mut self, scale_factor: f64) {
        self.scale_factor = sanitize_scale(scale_factor);
        self.metrics_pending = true;

        // The cursor has not moved in physical space, but the logical position
        // recorded for it was computed under the old factor and is now wrong.
        // Discard it rather than rescale it: winit sends a fresh `CursorMoved`
        // when the pointer next moves, and attributing a click to a
        // back-computed position is the kind of "nearly right" behaviour that
        // hides DPI bugs instead of surfacing them.
        self.last_cursor = None;
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

    /// A wheel notch or touchpad gesture, converted and sign-corrected.
    ///
    /// `physical_delta` is what the platform reported, in its own direction.
    /// A platform where wheeling away from you scrolls *up* reports a positive
    /// value there and is negated here — the one place that question is asked.
    ///
    /// `None` when the cursor position is not yet known: a scroll has to
    /// happen *somewhere* for `instar-ui` to find a viewport under it, and
    /// inventing a position would put the event on whatever is at the origin.
    pub fn on_wheel(&self, delta: ScrollDelta, natural: bool) -> Option<RawScrollEvent> {
        let delta = match delta {
            // Physical to logical, like every other distance crossing this
            // boundary.
            ScrollDelta::Logical { x, y } => ScrollDelta::Logical {
                x: x / self.scale_factor,
                y: y / self.scale_factor,
            },
            // A count, not a distance, so the scale factor does not apply.
            ScrollDelta::Lines { x, y } => ScrollDelta::Lines { x, y },
        };
        let delta = if natural { delta } else { negate(delta) };
        Some(RawScrollEvent {
            window_id: self.window_id,
            logical_pos: self.last_cursor?,
            delta,
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

fn negate(delta: ScrollDelta) -> ScrollDelta {
    match delta {
        ScrollDelta::Logical { x, y } => ScrollDelta::Logical { x: -x, y: -y },
        ScrollDelta::Lines { x, y } => ScrollDelta::Lines { x: -x, y: -y },
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
        WindowState::new(
            WindowId::from_raw(1),
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

        state.on_scale_factor_changed(2.0);
        assert_eq!(state.scale_factor(), 2.0);

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
        state.on_scale_factor_changed(2.0);
        // Rather than report a position computed under the old factor, there
        // is simply no position until the next move.
        assert_eq!(
            state.on_mouse_input(PointerButton::Primary, PointerState::Pressed),
            None,
            "a click must not be attributed to a position computed at the old scale"
        );
    }

    /// The barrier carries no geometry. If it did, a host could use those
    /// values -- which is the exact mistake it exists to prevent.
    #[test]
    fn the_invalidation_signal_carries_no_geometry() {
        let output = WindowOutput::MetricsInvalidated {
            window_id: WindowId::from_raw(1),
        };
        match output {
            WindowOutput::MetricsInvalidated { window_id } => {
                assert_eq!(window_id, WindowId::from_raw(1));
            }
            other => panic!("expected an invalidation, got {other:?}"),
        }
    }

    /// The barrier stays open until coherent metrics close it, and
    /// `metrics_pending` is the state a host can read to know that.
    #[test]
    fn the_barrier_stays_open_until_coherent_metrics_arrive() {
        let mut state = state(1.0);
        assert!(!state.metrics_pending(), "no barrier before a scale change");

        state.on_scale_factor_changed(2.0);
        assert!(
            state.metrics_pending(),
            "the barrier opens on a scale change"
        );

        // Pointer input still converts -- the host may retain the position --
        // but the barrier is still open, so it must not be acted on.
        state.on_cursor_moved(200.0, 100.0);
        assert_eq!(state.last_cursor(), Some(LogicalPoint::new(100.0, 50.0)));
        assert!(
            state.metrics_pending(),
            "receiving pointer input must not close the barrier: a position in \
             the new scale is still meaningless against the old layout"
        );

        state.on_resized(PhysicalSize {
            width: 1600,
            height: 1200,
        });
        assert!(
            !state.metrics_pending(),
            "coherent metrics close the barrier"
        );
    }

    /// A scale change alone must publish nothing: the new scale paired with
    /// the old size would be individually true and jointly wrong, and a
    /// renderer would size a surface from it.
    #[test]
    fn a_scale_change_alone_publishes_no_metrics() {
        let mut state = state(1.0);
        state.on_scale_factor_changed(2.0);

        assert!(
            state.metrics_pending(),
            "a scale change should leave metrics pending, not publish them"
        );
        assert_eq!(
            state.scale_factor(),
            2.0,
            "the scale itself still applies immediately, for coordinate conversion"
        );
    }

    /// The `Resized` that usually follows resolves the pending metrics.
    #[test]
    fn a_following_resize_publishes_coherent_metrics() {
        let mut state = state(1.0);
        state.on_scale_factor_changed(2.0);

        let metrics = state.on_resized(PhysicalSize {
            width: 1600,
            height: 1200,
        });
        assert_eq!(metrics.scale_factor, 2.0);
        assert_eq!(
            metrics.physical_size,
            PhysicalSize {
                width: 1600,
                height: 1200
            }
        );
        assert_eq!(
            metrics.logical_size,
            LogicalSize {
                width: 800.0,
                height: 600.0
            },
            "logical size must be derived from the new scale and the new size together"
        );
        assert!(!state.metrics_pending(), "the resize resolved it");
    }

    /// And where no `Resized` arrives -- which winit does not guarantee across
    /// platforms -- the end-of-cycle flush publishes the same coherent pair.
    #[test]
    fn a_pending_flush_publishes_coherent_metrics_without_a_resize() {
        let mut state = state(1.0);
        state.on_scale_factor_changed(2.0);

        let metrics = state
            .take_pending_metrics(PhysicalSize {
                width: 1600,
                height: 1200,
            })
            .expect("metrics were pending");
        assert_eq!(metrics.scale_factor, 2.0);
        assert_eq!(
            metrics.logical_size,
            LogicalSize {
                width: 800.0,
                height: 600.0
            }
        );

        assert_eq!(
            state.take_pending_metrics(PhysicalSize {
                width: 1600,
                height: 1200
            }),
            None,
            "flushing twice must not publish the same change again"
        );
    }

    #[test]
    fn nothing_is_pending_without_a_scale_change() {
        let mut state = state(2.0);
        assert!(!state.metrics_pending());
        assert_eq!(
            state.take_pending_metrics(PhysicalSize {
                width: 800,
                height: 600
            }),
            None,
            "an unconditional end-of-cycle flush must be a no-op when idle"
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
