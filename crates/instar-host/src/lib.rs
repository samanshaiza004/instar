//! Instar's orchestration layer.
//!
//! ```text
//! instar-window  -->  instar-host  <--  instar-kernel
//!                          |
//!                      instar-ui
//!                  layout / interaction
//! ```
//!
//! This is the only crate that sees every layer, and therefore the only one
//! that may bridge logical presentation to physical rendering. It holds no
//! layout algorithm, no wire format, and no windowing code — it decides *what
//! happens next*, and delegates everything else.
//!
//! # Routing is expressed as effects, not side effects
//!
//! [`Host::handle`] returns a list of [`HostEffect`] rather than performing
//! I/O. That keeps the routing rules — which are the substance of this crate —
//! testable without a window, a GPU, or a running guest, and it is why the
//! tests below can drive scale changes and clicks in a headless CI job.
//!
//! # The metrics barrier
//!
//! The rule this crate exists to enforce (docs/PHASE-1.md):
//!
//! | While [`MetricsState::Blocked`] | While [`MetricsState::Ready`] |
//! |---|---|
//! | no layout | recompute layout *first* |
//! | no UI hit-testing | then replace the snapshot |
//! | no UI activation | then process actionable input |
//! | no app-content render | then service any pending redraw |
//! | redraw becomes pending | |
//! | pointer position may update | |
//! | close/lifecycle still works | |

#![forbid(unsafe_code)]

use std::collections::HashMap;

use instar_ui::{Interaction, LayoutSnapshot, Tree, TreeError, UiAction, Viewport};
use instar_window::{
    LogicalPoint, PointerState, RawPointerEvent, WindowId, WindowMetricsChanged, WindowOutput,
};

/// Whether a window's geometry can be used right now.
///
/// Deliberately not `Option<Metrics> + bool`, and deliberately not
/// `Invalid(Metrics)`: both let a caller reach stale geometry and use it. Here
/// the only way to obtain usable metrics is [`MetricsState::usable`], which
/// returns `None` unless the state is [`MetricsState::Ready`]. Stale values
/// still exist — a renderer wants to know what the window *was* — but they are
/// reachable only through a name that says so.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricsState {
    /// Geometry is unusable: either nothing is known yet, or a scale change
    /// invalidated what was.
    Blocked {
        /// The last coherent metrics, for diagnostics and caching only.
        ///
        /// **Never lay out, hit-test, or render from this.** It is retained so
        /// a host can log what changed or size a placeholder, not so it can
        /// pretend the barrier is not there.
        last_valid: Option<WindowMetricsChanged>,
    },
    /// Geometry is coherent and may be used.
    Ready(WindowMetricsChanged),
}

impl Default for MetricsState {
    fn default() -> Self {
        Self::Blocked { last_valid: None }
    }
}

impl MetricsState {
    /// The metrics, if and only if they may be acted on.
    pub fn usable(&self) -> Option<&WindowMetricsChanged> {
        match self {
            Self::Ready(metrics) => Some(metrics),
            Self::Blocked { .. } => None,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// The last coherent metrics, whether or not they are currently usable.
    /// Diagnostics only.
    pub fn last_known(&self) -> Option<&WindowMetricsChanged> {
        match self {
            Self::Ready(metrics) => Some(metrics),
            Self::Blocked { last_valid } => last_valid.as_ref(),
        }
    }

    /// Coherent metrics arrived.
    pub fn ready(&mut self, metrics: WindowMetricsChanged) {
        *self = Self::Ready(metrics);
    }

    /// Geometry was invalidated. Idempotent: invalidating while already
    /// blocked keeps the original `last_valid` rather than forgetting it.
    pub fn block(&mut self) {
        if let Self::Ready(metrics) = self {
            *self = Self::Blocked {
                last_valid: Some(*metrics),
            };
        }
    }
}

/// What the host wants done as a result of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEffect {
    /// Deliver these encoded bytes to the guest.
    SendToGuest(Vec<u8>),
    /// Render this window's current layout.
    Render { window: WindowId },
    /// Exit the application.
    Exit,
}

/// Per-window host state.
#[derive(Debug, Default)]
pub struct HostWindow {
    metrics: MetricsState,
    /// The most recent tree the guest committed. Survives invalidation — it is
    /// the *geometry* that goes stale, not the interface description.
    tree: Option<Tree>,
    layout: Option<LayoutSnapshot>,
    interaction: Interaction,
    /// A redraw asked for while blocked, to be serviced once ready.
    redraw_pending: bool,
    /// Updated even while blocked; acted on only when ready.
    last_pointer: Option<LogicalPoint>,
}

impl HostWindow {
    pub fn metrics(&self) -> &MetricsState {
        &self.metrics
    }

    pub fn layout(&self) -> Option<&LayoutSnapshot> {
        // Gated on readiness, not merely on presence: a snapshot computed for
        // the old scale is exactly the thing the barrier exists to withhold.
        self.metrics
            .is_ready()
            .then_some(self.layout.as_ref())
            .flatten()
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    pub fn redraw_pending(&self) -> bool {
        self.redraw_pending
    }

    pub fn last_pointer(&self) -> Option<LogicalPoint> {
        self.last_pointer
    }

    /// Recomputes layout from the current tree and metrics.
    ///
    /// Does nothing while blocked, which is the barrier's "no layout" rule
    /// enforced at the only place layout is produced.
    fn recompute_layout(&mut self) {
        let (Some(metrics), Some(tree)) = (self.metrics.usable(), self.tree.as_ref()) else {
            return;
        };
        let viewport = Viewport::new(
            metrics.logical_size.width as f32,
            metrics.logical_size.height as f32,
        );
        self.layout = Some(tree.layout(viewport));
    }
}

/// Orchestrates windows, the UI layer, and the guest runtime.
#[derive(Debug, Default)]
pub struct Host {
    windows: HashMap<WindowId, HostWindow>,
}

impl Host {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn window(&self, id: WindowId) -> Option<&HostWindow> {
        self.windows.get(&id)
    }

    /// Routes one window event, returning what should happen as a result.
    pub fn handle(&mut self, event: WindowOutput) -> Vec<HostEffect> {
        match event {
            WindowOutput::MetricsChanged(metrics) => self.on_metrics_changed(metrics),
            WindowOutput::MetricsInvalidated { window_id } => {
                self.on_metrics_invalidated(window_id)
            }
            WindowOutput::Pointer(event) => self.on_pointer(event),
            WindowOutput::RedrawRequested { window_id } => self.on_redraw_requested(window_id),
            // Close policy lives here, not in the window layer. A host with a
            // guest to consult could ask it first; this one exits.
            WindowOutput::CloseRequested { .. } => vec![HostEffect::Exit],
        }
    }

    /// The guest committed a new interface.
    ///
    /// Decoding and semantic validation are `instar-ui`'s; a rejected batch is
    /// returned as an error rather than applied, so a malformed commit leaves
    /// the previous interface standing instead of blanking the window.
    pub fn on_guest_commit(
        &mut self,
        window_id: WindowId,
        batch: &[u8],
    ) -> Result<Vec<HostEffect>, TreeError> {
        let tree = Tree::decode(batch)?;
        let window = self.windows.entry(window_id).or_default();
        window.tree = Some(tree);
        window.recompute_layout();

        Ok(if window.metrics.is_ready() {
            vec![HostEffect::Render { window: window_id }]
        } else {
            // Nothing to draw against yet; remember that something wants
            // drawing once there is.
            window.redraw_pending = true;
            Vec::new()
        })
    }

    fn on_metrics_changed(&mut self, metrics: WindowMetricsChanged) -> Vec<HostEffect> {
        let window = self.windows.entry(metrics.window_id).or_default();
        window.metrics.ready(metrics);

        // Order matters and is the barrier's exit rule: layout first, then the
        // snapshot is replaced, and only then may anything be rendered.
        window.recompute_layout();

        let mut effects = Vec::new();
        if window.redraw_pending || window.layout.is_some() {
            window.redraw_pending = false;
            effects.push(HostEffect::Render {
                window: metrics.window_id,
            });
        }
        effects
    }

    fn on_metrics_invalidated(&mut self, window_id: WindowId) -> Vec<HostEffect> {
        let window = self.windows.entry(window_id).or_default();
        window.metrics.block();
        // A press recorded against the old geometry must not be completable
        // against the new: the node under the pointer may have moved.
        window.interaction.cancel();
        Vec::new()
    }

    fn on_pointer(&mut self, event: RawPointerEvent) -> Vec<HostEffect> {
        let window = self.windows.entry(event.window_id).or_default();

        // Allowed while blocked: the position is just a coordinate, and
        // dropping it would lose the pointer's whereabouts across a monitor
        // switch for no benefit.
        window.last_pointer = Some(event.logical_pos);

        // Not allowed while blocked: acting on it. A position converted with
        // the new scale against a layout computed for the old viewport
        // resolves to the *wrong* node, which is worse than resolving to none.
        let (Some(_metrics), Some(tree), Some(layout)) = (
            window.metrics.usable(),
            window.tree.as_ref(),
            window.layout.as_ref(),
        ) else {
            return Vec::new();
        };

        let (x, y) = event.logical_pos.round();
        match event.state {
            PointerState::Pressed => {
                window.interaction.on_press(tree, layout, x, y);
                Vec::new()
            }
            PointerState::Released => window
                .interaction
                .on_release(tree, layout, x, y)
                .map(|action: UiAction| vec![HostEffect::SendToGuest(action.encode())])
                .unwrap_or_default(),
        }
    }

    fn on_redraw_requested(&mut self, window_id: WindowId) -> Vec<HostEffect> {
        let window = self.windows.entry(window_id).or_default();
        if window.metrics.is_ready() {
            vec![HostEffect::Render { window: window_id }]
        } else {
            // Deferred, not dropped: the compositor asked for a frame and will
            // not necessarily ask again.
            window.redraw_pending = true;
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_ui::protocol::{BatchEncoder, NodeKey, WireDimension, WireLayout, flags, opcode};
    use instar_window::{LogicalSize, PhysicalSize, PointerButton};

    const WINDOW: WindowId = WindowId::from_raw(1);

    fn metrics(scale: f64) -> WindowMetricsChanged {
        WindowMetricsChanged {
            window_id: WINDOW,
            logical_size: LogicalSize {
                width: 400.0,
                height: 300.0,
            },
            physical_size: PhysicalSize {
                width: (400.0 * scale) as u32,
                height: (300.0 * scale) as u32,
            },
            scale_factor: scale,
        }
    }

    /// root > column > (text, button).
    fn counter_batch() -> Vec<u8> {
        let fill = WireLayout {
            width: WireDimension::Fill,
            height: WireDimension::Content,
            padding: 0,
            gap: 0,
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey(1), 0, None, fill, 2)
            .node(
                opcode::NODE_TEXT,
                NodeKey(2),
                0,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey(3),
                flags::ENABLED,
                Some("Press me"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    fn pointer(state: PointerState, x: f64, y: f64) -> WindowOutput {
        WindowOutput::Pointer(RawPointerEvent {
            window_id: WINDOW,
            logical_pos: LogicalPoint::new(x, y),
            button: PointerButton::Primary,
            state,
        })
    }

    /// A host with a laid-out counter, ready for input.
    fn ready_host() -> Host {
        let mut host = Host::new();
        host.handle(WindowOutput::MetricsChanged(metrics(1.0)));
        host.on_guest_commit(WINDOW, &counter_batch())
            .expect("the fixture batch is valid");
        host
    }

    fn button_centre(host: &Host) -> (f64, f64) {
        let rect = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|layout| layout.get(NodeKey(3)))
            .expect("the button should be laid out");
        (
            f64::from(rect.x + rect.width / 2),
            f64::from(rect.y + rect.height / 2),
        )
    }

    // --- MetricsState: the states themselves ---

    #[test]
    fn a_host_starts_blocked_with_nothing_known() {
        let mut host = Host::new();
        host.handle(WindowOutput::RedrawRequested { window_id: WINDOW });
        let window = host.window(WINDOW).unwrap();
        assert_eq!(
            window.metrics(),
            &MetricsState::Blocked { last_valid: None }
        );
        assert!(window.metrics().usable().is_none());
    }

    #[test]
    fn invalidation_retains_the_last_valid_metrics_but_not_as_usable() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });

        let window = host.window(WINDOW).unwrap();
        assert!(
            window.metrics().usable().is_none(),
            "blocked metrics must never be obtainable as usable"
        );
        assert!(
            window.metrics().last_known().is_some(),
            "the last coherent metrics are still available for diagnostics"
        );
    }

    #[test]
    fn invalidating_twice_does_not_forget_the_last_valid_metrics() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        assert!(
            host.window(WINDOW)
                .unwrap()
                .metrics()
                .last_known()
                .is_some(),
            "a second invalidation must not overwrite last_valid with None"
        );
    }

    // --- The barrier: what is forbidden while blocked ---

    #[test]
    fn no_layout_is_exposed_while_blocked() {
        let mut host = ready_host();
        assert!(host.window(WINDOW).unwrap().layout().is_some());

        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        assert!(
            host.window(WINDOW).unwrap().layout().is_none(),
            "a snapshot computed for the old scale must not be reachable"
        );
    }

    #[test]
    fn no_activation_happens_while_blocked() {
        let mut host = ready_host();
        let (x, y) = button_centre(&host);

        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });

        assert!(host.handle(pointer(PointerState::Pressed, x, y)).is_empty());
        assert!(
            host.handle(pointer(PointerState::Released, x, y))
                .is_empty(),
            "a click during the barrier must not activate anything -- it would \
             resolve against a layout that no longer describes the screen"
        );
    }

    #[test]
    fn no_render_happens_while_blocked_and_the_request_is_deferred() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });

        assert!(
            host.handle(WindowOutput::RedrawRequested { window_id: WINDOW })
                .is_empty(),
            "no app content may be rendered while blocked"
        );
        assert!(
            host.window(WINDOW).unwrap().redraw_pending(),
            "the redraw must be deferred, not dropped: the compositor may not ask again"
        );
    }

    #[test]
    fn a_pointer_position_still_updates_while_blocked() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        host.handle(pointer(PointerState::Pressed, 12.0, 34.0));

        assert_eq!(
            host.window(WINDOW).unwrap().last_pointer(),
            Some(LogicalPoint::new(12.0, 34.0)),
            "position tracking is harmless and useful; acting on it is what is barred"
        );
    }

    #[test]
    fn closing_still_works_while_blocked() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        assert_eq!(
            host.handle(WindowOutput::CloseRequested { window_id: WINDOW }),
            vec![HostEffect::Exit],
            "a window that cannot be closed during a monitor switch would be a \
             worse bug than the one the barrier prevents"
        );
    }

    // --- The barrier: leaving it ---

    #[test]
    fn becoming_ready_relayouts_and_services_the_pending_redraw() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        host.handle(WindowOutput::RedrawRequested { window_id: WINDOW });

        let effects = host.handle(WindowOutput::MetricsChanged(metrics(2.0)));

        let window = host.window(WINDOW).unwrap();
        assert!(
            window.layout().is_some(),
            "layout must be recomputed before anything is rendered"
        );
        assert_eq!(
            effects,
            vec![HostEffect::Render { window: WINDOW }],
            "the deferred redraw should be serviced once geometry is coherent"
        );
        assert!(!window.redraw_pending(), "the pending redraw was consumed");
    }

    #[test]
    fn interaction_resumes_after_the_barrier_lifts() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        host.handle(WindowOutput::MetricsChanged(metrics(1.0)));

        let (x, y) = button_centre(&host);
        host.handle(pointer(PointerState::Pressed, x, y));
        assert_eq!(
            host.handle(pointer(PointerState::Released, x, y)).len(),
            1,
            "clicks should work again once metrics are coherent"
        );
    }

    #[test]
    fn a_press_started_before_invalidation_cannot_complete_after_it() {
        let mut host = ready_host();
        let (x, y) = button_centre(&host);

        host.handle(pointer(PointerState::Pressed, x, y));
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        host.handle(WindowOutput::MetricsChanged(metrics(1.0)));

        assert!(
            host.handle(pointer(PointerState::Released, x, y))
                .is_empty(),
            "the press was recorded against geometry that no longer exists; \
             completing it could activate a node that has since moved"
        );
    }

    // --- Ordinary routing ---

    #[test]
    fn a_click_becomes_a_guest_event() {
        let mut host = ready_host();
        let (x, y) = button_centre(&host);

        assert!(
            host.handle(pointer(PointerState::Pressed, x, y)).is_empty(),
            "a press alone activates nothing"
        );

        let effects = host.handle(pointer(PointerState::Released, x, y));
        assert_eq!(
            effects,
            vec![HostEffect::SendToGuest(
                UiAction::ButtonActivated(NodeKey(3)).encode()
            )],
            "a completed click should be routed to the guest as an encoded event"
        );
    }

    #[test]
    fn releasing_away_from_the_press_target_activates_nothing() {
        let mut host = ready_host();
        let (x, y) = button_centre(&host);

        host.handle(pointer(PointerState::Pressed, x, y));
        assert!(
            host.handle(pointer(PointerState::Released, x + 5_000.0, y))
                .is_empty(),
            "dragging off a button before releasing must cancel it"
        );
    }

    #[test]
    fn clicking_nothing_produces_nothing() {
        let mut host = ready_host();
        host.handle(pointer(PointerState::Pressed, 5_000.0, 5_000.0));
        assert!(
            host.handle(pointer(PointerState::Released, 5_000.0, 5_000.0))
                .is_empty()
        );
    }

    #[test]
    fn a_guest_commit_relayouts_and_asks_for_a_frame() {
        let mut host = Host::new();
        host.handle(WindowOutput::MetricsChanged(metrics(1.0)));

        let effects = host
            .on_guest_commit(WINDOW, &counter_batch())
            .expect("valid batch");
        assert_eq!(effects, vec![HostEffect::Render { window: WINDOW }]);
        assert!(host.window(WINDOW).unwrap().layout().is_some());
    }

    #[test]
    fn a_commit_while_blocked_defers_its_frame() {
        let mut host = Host::new();
        let effects = host
            .on_guest_commit(WINDOW, &counter_batch())
            .expect("valid batch");
        assert!(effects.is_empty(), "nothing to draw against yet");
        assert!(host.window(WINDOW).unwrap().redraw_pending());
    }

    /// A malformed commit must not blank the window.
    #[test]
    fn a_rejected_commit_leaves_the_previous_interface_standing() {
        let mut host = ready_host();
        assert!(host.on_guest_commit(WINDOW, b"not a batch").is_err());

        let window = host.window(WINDOW).unwrap();
        assert!(
            window.tree().is_some() && window.layout().is_some(),
            "a rejected commit should leave the last good interface in place"
        );
    }

    /// The viewport handed to layout is logical. Nothing in this crate's path
    /// to `instar-ui` carries a scale factor.
    #[test]
    fn layout_follows_the_logical_viewport_not_the_physical_one() {
        let mut host = ready_host();
        let wide = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey(1)))
            .unwrap();

        // Same logical size, double the physical size: layout must not move.
        host.handle(WindowOutput::MetricsChanged(metrics(2.0)));
        let after = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey(1)))
            .unwrap();

        assert_eq!(
            wide, after,
            "doubling the scale factor while the logical size is unchanged must \
             not move anything: instar-ui never sees DPI"
        );
    }
}
