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

//!
//! # Two threads
//!
//! Winit's event loop must own the main thread and its `EventLoop` is not
//! `Send`; Wasmtime ships no executor and expects the embedder to poll. Those
//! two facts do not fit on one thread, so [`bridge`] puts the guest on a
//! runtime thread of its own and marshals between them. Everything in *this*
//! module is the main thread's half, and stays synchronous.

#![forbid(unsafe_code)]

pub mod bridge;
pub mod present;

use std::collections::HashMap;
use std::sync::Arc;

use instar_kernel::runtime::GenerationId;
use instar_paint::PaintScene;
use instar_ui::{Interaction, LayoutSnapshot, Tree, TreeError, UiAction, Viewport};
use instar_window::{
    LogicalPoint, PointerState, RawPointerEvent, WindowId, WindowMetricsChanged, WindowOutput,
};

pub use present::{GlyphSource, PresentationState, SceneBuilder, Theme};

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
    ///
    /// [`bridge::HostBridge`] consumes this one rather than returning it: on
    /// the two-thread arrangement it becomes a
    /// [`bridge::RuntimeCommand::DeliverEvent`] on the queue to the runtime
    /// thread. It stays in the vocabulary because the routing rules are worth
    /// testing without a runtime attached.
    SendToGuest(Vec<u8>),
    /// Render this window's current layout.
    Render { window: WindowId },
    /// The guest generation ended — it trapped, or its `run` returned.
    ///
    /// WP7B2 turns this into a crash screen. WP7B1's only obligation is that
    /// nothing the dead generation committed can still be applied afterwards.
    GuestGone {
        generation: GenerationId,
        /// `None` if the guest exited cleanly.
        error: Option<String>,
    },
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
    /// Paint intent for the current frame, lowered when the interface or the
    /// geometry changes rather than when a frame is asked for. See
    /// [`present`]: a redraw callback is the worst place to discover work.
    scene: Option<PaintScene>,
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

    /// The paint intent to present, if there is any that may be shown.
    ///
    /// Gated on readiness for the same reason [`HostWindow::layout`] is, and
    /// more sharply: a scene carries *physical* rectangles built for a
    /// specific window size and scale, so presenting one across an
    /// invalidation would draw the last frame's geometry into the new
    /// window's buffer.
    pub fn scene(&self) -> Option<&PaintScene> {
        self.metrics
            .is_ready()
            .then_some(self.scene.as_ref())
            .flatten()
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
    /// What the window is showing — the guest's interface, or the host's own
    /// account of why it no longer can. Not per-window: Phase 1 is one guest
    /// and one window, and a dead guest is a fact about the runtime rather
    /// than about a surface.
    presentation: PresentationState,
    scenes: SceneBuilder,
}

impl Host {
    pub fn new() -> Self {
        Self::default()
    }

    /// A host that can draw text.
    ///
    /// Without one, scenes come out with every rectangle in place and no
    /// glyphs — which is what the headless tests want, and is why the font is
    /// injected rather than reached for.
    pub fn with_glyphs(glyphs: Arc<dyn GlyphSource>) -> Self {
        Self {
            scenes: SceneBuilder::with_glyphs(glyphs),
            ..Self::default()
        }
    }

    pub fn window(&self, id: WindowId) -> Option<&HostWindow> {
        self.windows.get(&id)
    }

    pub fn presentation(&self) -> &PresentationState {
        &self.presentation
    }

    pub fn theme(&self) -> &Theme {
        self.scenes.theme()
    }

    /// Re-lowers a window's paint intent from whatever is current.
    ///
    /// The single place a [`PaintScene`] is produced, so the rule that a scene
    /// is only ever built against usable metrics has one site to hold at —
    /// and so the crash screen's precedence over the guest's interface is
    /// stated once instead of at every caller.
    fn rebuild_scene(&mut self, window_id: WindowId) {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return;
        };
        let Some(metrics) = window.metrics.usable() else {
            // Blocked. Discard rather than keep: this scene's rectangles were
            // computed for geometry that no longer describes the window.
            window.scene = None;
            return;
        };

        window.scene = Some(match &self.presentation {
            // First, and unconditionally. A crashed guest's last interface is
            // still in `window.tree` and must not come back on screen because
            // something asked for a frame.
            PresentationState::Crashed {
                generation,
                message,
            } => self.scenes.crash_scene(*generation, message, metrics),
            PresentationState::App => match (window.tree.as_ref(), window.layout.as_ref()) {
                (Some(tree), Some(layout)) => {
                    self.scenes
                        .app_scene(tree, layout, metrics, window.interaction.pressed())
                }
                // Ready, but the guest has not committed anything yet. An
                // empty background beats an unpainted buffer, which on most
                // platforms is whatever was in memory.
                _ => self.scenes.blank_scene(metrics),
            },
        });
    }

    /// The guest generation ended.
    ///
    /// A trap becomes [`PresentationState::Crashed`] and a frame; a clean exit
    /// does not. A guest whose `run` returned did what it meant to, and
    /// replacing its last interface with an error screen would be the host
    /// reporting a failure that did not happen.
    ///
    /// The message is clamped *here*, where the state is built, rather than
    /// where it is drawn. A trap message is guest-influenced and unbounded, and
    /// bounding it at the point of storage means nothing downstream can be
    /// handed an unbounded one — not the scene builder, not a `Debug` print,
    /// not whatever reads `presentation()` next. The full diagnostic still
    /// travels on [`HostEffect::GuestGone`] for the log.
    pub fn on_guest_gone(
        &mut self,
        window_id: WindowId,
        generation: GenerationId,
        error: Option<String>,
    ) -> Vec<HostEffect> {
        let Some(message) = error else {
            return Vec::new();
        };
        self.presentation = PresentationState::Crashed {
            generation,
            message: present::clamp_diagnostic(&message),
        };
        self.rebuild_scene(window_id);

        let window = self.windows.entry(window_id).or_default();
        if window.metrics.is_ready() {
            vec![HostEffect::Render { window: window_id }]
        } else {
            // The crash screen waits for coherent geometry like anything else;
            // it is still the thing that will be shown when one arrives.
            window.redraw_pending = true;
            Vec::new()
        }
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
        Ok(self.apply_tree(window_id, Tree::decode(batch)?))
    }

    /// Installs an already-decoded, already-validated tree.
    ///
    /// Split from [`Host::on_guest_commit`] because the two-thread bridge must
    /// decode at a specific point in a normative sequence — after the
    /// generation check, before anything is mutated — and so cannot use a
    /// function that does both at once. The swap below is the "apply
    /// atomically" step: one assignment, after all validation, so a rejected
    /// commit can never leave the tree half-updated.
    pub fn apply_tree(&mut self, window_id: WindowId, tree: Tree) -> Vec<HostEffect> {
        let window = self.windows.entry(window_id).or_default();
        window.tree = Some(tree);
        window.recompute_layout();
        // Lowered here rather than on the next frame callback: the caller is
        // about to tell a guest its interface was accepted, and "accepted"
        // should mean the host has everything it needs to show it.
        self.rebuild_scene(window_id);

        let window = self.windows.entry(window_id).or_default();
        if window.metrics.is_ready() {
            vec![HostEffect::Render { window: window_id }]
        } else {
            // Nothing to draw against yet; remember that something wants
            // drawing once there is.
            window.redraw_pending = true;
            Vec::new()
        }
    }

    fn on_metrics_changed(&mut self, metrics: WindowMetricsChanged) -> Vec<HostEffect> {
        let window_id = metrics.window_id;
        let window = self.windows.entry(window_id).or_default();
        window.metrics.ready(metrics);

        // Order matters and is the barrier's exit rule: layout first, then the
        // snapshot is replaced, then the scene is lowered against it, and only
        // then may anything be rendered.
        window.recompute_layout();
        let wanted = window.redraw_pending || window.layout.is_some();
        self.rebuild_scene(window_id);

        let window = self.windows.entry(window_id).or_default();
        let mut effects = Vec::new();
        if wanted {
            window.redraw_pending = false;
            effects.push(HostEffect::Render { window: window_id });
        }
        effects
    }

    fn on_metrics_invalidated(&mut self, window_id: WindowId) -> Vec<HostEffect> {
        let window = self.windows.entry(window_id).or_default();
        window.metrics.block();
        // A press recorded against the old geometry must not be completable
        // against the new: the node under the pointer may have moved.
        window.interaction.cancel();
        // And the lowered scene goes with it, for the same reason: its
        // rectangles are physical, and they were computed for a window that
        // has since changed size or scale.
        window.scene = None;
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
        let held = window.interaction.pressed();
        let mut effects = match event.state {
            PointerState::Pressed => {
                window.interaction.on_press(tree, layout, x, y);
                Vec::new()
            }
            PointerState::Released => window
                .interaction
                .on_release(tree, layout, x, y)
                .map(|action: UiAction| vec![HostEffect::SendToGuest(action.encode())])
                .unwrap_or_default(),
        };

        // Press state is drawn, so changing it is a visual change, and it is
        // the host's to show: the guest hears about a *completed* click and
        // would be far too late to provide the feedback for one in progress.
        let window = self.windows.entry(event.window_id).or_default();
        if window.interaction.pressed() != held {
            self.rebuild_scene(event.window_id);
            effects.push(HostEffect::Render {
                window: event.window_id,
            });
        }
        effects
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

    /// Just the events bound for the guest.
    ///
    /// Pointer handling also emits [`HostEffect::Render`] when the pressed
    /// look changes, and that is a different question from what the guest
    /// hears about — a press is visible immediately and is *not* reported,
    /// because only a completed click is an interaction.
    fn to_guest(effects: &[HostEffect]) -> Vec<&Vec<u8>> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                HostEffect::SendToGuest(bytes) => Some(bytes),
                _ => None,
            })
            .collect()
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
            to_guest(&host.handle(pointer(PointerState::Released, x, y))).len(),
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

        let press = host.handle(pointer(PointerState::Pressed, x, y));
        assert!(
            to_guest(&press).is_empty(),
            "a press alone activates nothing"
        );

        let release = host.handle(pointer(PointerState::Released, x, y));
        assert_eq!(
            to_guest(&release),
            vec![&UiAction::ButtonActivated(NodeKey(3)).encode()],
            "a completed click should be routed to the guest as an encoded event"
        );
    }

    /// Press feedback is the host's, and is immediate. Waiting for the guest
    /// to describe a held button would put a runtime round-trip between the
    /// finger and the pixel, for a state the guest is never even told about.
    #[test]
    fn pressing_and_releasing_each_ask_for_a_frame() {
        let mut host = ready_host();
        let (x, y) = button_centre(&host);

        assert!(
            host.handle(pointer(PointerState::Pressed, x, y))
                .contains(&HostEffect::Render { window: WINDOW }),
            "a button that does not visibly depress has no feedback at all"
        );
        assert!(
            host.handle(pointer(PointerState::Released, x, y))
                .contains(&HostEffect::Render { window: WINDOW }),
            "and it has to come back up"
        );
    }

    #[test]
    fn releasing_away_from_the_press_target_activates_nothing() {
        let mut host = ready_host();
        let (x, y) = button_centre(&host);

        host.handle(pointer(PointerState::Pressed, x, y));
        let release = host.handle(pointer(PointerState::Released, x + 5_000.0, y));
        assert!(
            to_guest(&release).is_empty(),
            "dragging off a button before releasing must cancel it"
        );
    }

    #[test]
    fn clicking_nothing_produces_nothing() {
        let mut host = ready_host();
        host.handle(pointer(PointerState::Pressed, 5_000.0, 5_000.0));
        assert!(
            host.handle(pointer(PointerState::Released, 5_000.0, 5_000.0))
                .is_empty(),
            "nothing was hit, so there is neither an event nor anything to redraw"
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

    // --- Presentation (WP7B2) ---

    const GEN1: GenerationId = GenerationId(1);

    fn scene_clear(host: &Host) -> Option<instar_paint::Color> {
        match host.window(WINDOW)?.scene()?.commands.first() {
            Some(instar_paint::PaintCommand::Clear { color }) => Some(*color),
            _ => None,
        }
    }

    #[test]
    fn a_ready_window_has_something_to_present_before_the_guest_commits() {
        let mut host = Host::new();
        host.handle(WindowOutput::MetricsChanged(metrics(1.0)));
        assert_eq!(
            scene_clear(&host),
            Some(host.theme().background),
            "an unpainted buffer shows whatever was in that memory; a window \
             with no interface yet should look deliberately blank"
        );
    }

    #[test]
    fn a_commit_lowers_a_scene_rather_than_leaving_it_for_the_frame_callback() {
        let host = ready_host();
        let scene = host.window(WINDOW).unwrap().scene().expect("lowered");
        assert!(
            scene
                .commands
                .iter()
                .any(|command| matches!(command, instar_paint::PaintCommand::FillRect { .. })),
            "the fixture has a button, so its paint intent should contain one"
        );
    }

    #[test]
    fn a_trap_shows_the_host_crash_screen_and_asks_for_a_frame() {
        let mut host = ready_host();
        let effects = host.on_guest_gone(WINDOW, GEN1, Some("guest trapped".to_string()));

        assert_eq!(effects, vec![HostEffect::Render { window: WINDOW }]);
        assert_eq!(
            host.presentation(),
            &PresentationState::Crashed {
                generation: GEN1,
                message: "guest trapped".to_string()
            }
        );
        assert_eq!(scene_clear(&host), Some(host.theme().crash_background));
    }

    /// The rule the crash screen exists to keep: it is *presentation*, not an
    /// interface. Nothing the host shows after a trap may pass itself off as
    /// something the guest committed.
    #[test]
    fn the_crash_screen_is_not_written_into_the_guests_tree() {
        let mut host = ready_host();
        let before = host.window(WINDOW).unwrap().tree().cloned();

        host.on_guest_gone(WINDOW, GEN1, Some("guest trapped".to_string()));

        assert_eq!(
            host.window(WINDOW).unwrap().tree(),
            before.as_ref(),
            "the retained tree must still say exactly what the guest last said"
        );
        assert!(
            host.window(WINDOW).unwrap().layout().is_some(),
            "and it is still laid out -- the guest died, the geometry did not"
        );
    }

    /// A dead generation's commits are screened out upstream, but presentation
    /// must not depend on that being the only guard: once the host has taken
    /// over the window, a tree arriving from anywhere does not take it back.
    #[test]
    fn a_commit_after_a_trap_cannot_put_the_app_back_on_screen() {
        let mut host = ready_host();
        host.on_guest_gone(WINDOW, GEN1, Some("guest trapped".to_string()));

        host.on_guest_commit(WINDOW, &counter_batch())
            .expect("valid batch");

        assert_eq!(
            scene_clear(&host),
            Some(host.theme().crash_background),
            "the crash screen outranks anything a tree can say"
        );
    }

    /// The crash surface exists because something already went wrong, so it is
    /// the last place that may be overwhelmed by guest-influenced input. The
    /// clamp lives at the point the state is built, which is what makes it
    /// impossible for anything downstream to be handed the unbounded version.
    #[test]
    fn a_huge_trap_message_is_bounded_before_it_is_ever_stored() {
        let flood = "frame\n".repeat(200_000);
        assert!(flood.len() > present::MAX_CRASH_MESSAGE_BYTES);

        let mut host = ready_host();
        host.on_guest_gone(WINDOW, GEN1, Some(flood.clone()));

        let PresentationState::Crashed { message, .. } = host.presentation() else {
            panic!("a trap should have crashed the presentation");
        };
        assert!(
            message.len() <= present::MAX_CRASH_MESSAGE_BYTES + 64,
            "a {}-byte trap message was retained as {} bytes",
            flood.len(),
            message.len()
        );
        assert!(
            message.lines().count() <= present::MAX_CRASH_MESSAGE_LINES + 1,
            "the line cap should have bound first for a backtrace shape"
        );
        // And it still draws.
        assert_eq!(scene_clear(&host), Some(host.theme().crash_background));
    }

    #[test]
    fn a_clean_exit_leaves_the_last_interface_standing() {
        let mut host = ready_host();
        let effects = host.on_guest_gone(WINDOW, GEN1, None);

        assert!(effects.is_empty());
        assert_eq!(
            host.presentation(),
            &PresentationState::App,
            "a guest whose run returned did what it meant to; an error screen \
             would report a failure that did not happen"
        );
        assert_eq!(scene_clear(&host), Some(host.theme().background));
    }

    #[test]
    fn a_crash_while_blocked_waits_for_geometry_like_anything_else() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });

        let effects = host.on_guest_gone(WINDOW, GEN1, Some("guest trapped".to_string()));
        assert!(effects.is_empty(), "no render may happen while blocked");
        assert!(host.window(WINDOW).unwrap().redraw_pending());

        host.handle(WindowOutput::MetricsChanged(metrics(1.0)));
        assert_eq!(
            scene_clear(&host),
            Some(host.theme().crash_background),
            "and the crash screen is what the deferred frame shows"
        );
    }

    /// The barrier applies to paint intent too, and more sharply than to
    /// layout: a scene's rectangles are physical, so presenting one across an
    /// invalidation draws the old window's geometry into the new buffer.
    #[test]
    fn no_scene_is_presentable_while_blocked() {
        let mut host = ready_host();
        assert!(host.window(WINDOW).unwrap().scene().is_some());

        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        assert!(host.window(WINDOW).unwrap().scene().is_none());
    }

    #[test]
    fn leaving_the_barrier_lowers_a_scene_for_the_new_geometry() {
        let mut host = ready_host();
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        host.handle(WindowOutput::MetricsChanged(metrics(2.0)));

        let scene = host.window(WINDOW).unwrap().scene().expect("re-lowered");
        assert_eq!(
            scene.size,
            instar_paint::PhysicalSize {
                width: 800,
                height: 600
            },
            "the new scene must be built for the window that exists now"
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
