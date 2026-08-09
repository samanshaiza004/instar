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
use instar_ui::{Interaction, KeyLedger, ScrollState, TextContext, TreeError, UiAction, Viewport};
use instar_window::{
    LogicalPoint, PointerState, RawPointerEvent, WindowId, WindowMetricsChanged, WindowOutput,
};

pub use present::{PresentationState, SceneBuilder, Theme};

/// The `instar-ui` vocabulary this crate's own API already speaks.
///
/// Re-exported so a consumer can name what [`HostWindow::layout`] and
/// [`HostWindow::tree`] hand back without taking a direct dependency on the UI
/// layer. That edge would not be *wrong* — this crate is above it — but a
/// caller adding a dependency purely to spell a return type is a caller being
/// made to know something it does not need to.
pub use instar_ui::{LayoutSnapshot, NodeKey, Rect, Tree};

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
    /// The version of the retained UI state.
    tree_revision: u64,
    layout: Option<LayoutSnapshot>,
    /// Paint intent for the current frame, lowered when the interface or the
    /// geometry changes rather than when a frame is asked for. See
    /// [`present`]: a redraw callback is the worst place to discover work.
    scene: Option<PaintScene>,
    interaction: Interaction,
    /// The id lifecycle for the guest currently owning this window.
    ledger: KeyLedger,
    /// Where each retained viewport is scrolled to. Host-owned: no guest sets
    /// one, and none can read one.
    scroll: ScrollState,
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

    /// The version of the retained UI state.
    ///
    /// Incremented only when an accepted snapshot actually changed the tree;
    /// an identical re-commit is a guest event, not a new tree. Layout, paint,
    /// and accessibility caches key off this value, which is why it is kept
    /// apart from the guest-visible commit sequence that advances on every
    /// accepted commit.
    pub fn tree_revision(&self) -> u64 {
        self.tree_revision
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
    fn recompute_layout(&mut self, text: &mut TextContext) {
        let (Some(metrics), Some(tree)) = (self.metrics.usable(), self.tree.as_ref()) else {
            return;
        };
        let viewport = Viewport::new(
            metrics.logical_size.width as f32,
            metrics.logical_size.height as f32,
        );
        self.layout = Some(tree.layout(text, viewport));
    }

    /// Confines every retained offset to what the current layout leaves
    /// scrollable.
    ///
    /// Called after layout and before the snapshot becomes interactive.
    /// Content that shrank must not leave a viewport showing a region that no
    /// longer exists, and a hit-test against a stale offset resolves to the
    /// wrong node rather than to nothing — which is worse, because it looks
    /// like it worked.
    ///
    /// The scrollable extent is the content's size less the viewport's, floored
    /// at zero: content that fits has nothing to scroll. A viewport that is not
    /// laid out right now — hidden, or under a `Display::None` ancestor — has
    /// no extent to clamp against, and its offset is left alone rather than
    /// zeroed, because hiding retains.
    fn clamp_scroll(&mut self) {
        let (Some(tree), Some(layout)) = (self.tree.as_ref(), self.layout.as_ref()) else {
            return;
        };
        self.scroll.clamp_to(tree, &|key| {
            let viewport = layout.get(key)?;
            let content = instar_ui::scroll::content_of(tree.find(key)?)?;
            let content_rect = layout.get(content.key)?;
            Some(instar_ui::ScrollOffset::new(
                (content_rect.width - viewport.width).max(0),
                (content_rect.height - viewport.height).max(0),
            ))
        });
    }
}

/// Orchestrates windows, the UI layer, and the guest runtime.
#[derive(Debug)]
pub struct Host {
    windows: HashMap<WindowId, HostWindow>,
    /// What the window is showing — the guest's interface, or the host's own
    /// account of why it no longer can. Not per-window: Phase 1 is one guest
    /// and one window, and a dead guest is a fact about the runtime rather
    /// than about a surface.
    presentation: PresentationState,
    scenes: SceneBuilder,
    /// The long-lived Parley shaping cache. Created once and reused for every
    /// layout pass; see `instar_ui::TextContext`.
    text: TextContext,
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Host {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            presentation: PresentationState::default(),
            scenes: SceneBuilder::new(),
            text: TextContext::new(),
        }
    }

    /// A host whose Parley font context has the shipped monospace face.
    pub fn with_monospace_face(face: Arc<[u8]>) -> Self {
        Self {
            text: TextContext::with_monospace_face(face),
            ..Self::new()
        }
    }

    pub fn window(&self, id: WindowId) -> Option<&HostWindow> {
        self.windows.get(&id)
    }

    /// Shaping work done since [`Host::reset_text_stats`].
    ///
    /// Exposed so a warm click can be traced: the question "did one changed
    /// label rebuild one layout, or all of them?" is not answerable from a
    /// duration.
    pub fn text_stats(&self) -> instar_ui::text::TextStats {
        self.text.stats()
    }

    pub fn reset_text_stats(&mut self) {
        self.text.reset_stats();
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
            } => self
                .scenes
                .crash_scene(&mut self.text, *generation, message, metrics),
            PresentationState::App => match (window.tree.as_ref(), window.layout.as_ref()) {
                (Some(tree), Some(layout)) => self.scenes.app_scene(
                    tree,
                    layout,
                    &window.scroll,
                    metrics,
                    window.interaction.pressed(),
                ),
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
        // The id *history* dies with the Wasm runtime generation: retired ids
        // are forgotten and the observed-id count resets, so a fresh
        // generation is not billed for its predecessor's churn. This sits
        // above the clean-exit early return, because a guest whose `run`
        // returned has ended just as surely as one that trapped.
        //
        // What is reseeded rather than forgotten is the tree still on screen:
        //
        // > Retained UI surviving a guest generation change keeps its exact
        // > `NodeKey`s and repopulates the new generation's ledger before that
        // > tree can become interactive.
        //
        // A cleared ledger beside a retained `window.tree` is a desync: an
        // identical re-commit takes the no-op path in `apply_tree` and never
        // reaches `ledger.apply`, so those ids would stay unknown to the
        // ledger while remaining live, and the first removal-then-reuse of one
        // would be accepted at generation 0 — the hole the ledger exists to
        // close, reopened by the ledger's own reset. Only the dead
        // generation's *history* is discarded. This is not a bare
        // `ledger.clear()` on purpose; see
        // `a_dead_generation_leaves_the_ledger_agreeing_with_the_tree_on_screen`.
        for window in self.windows.values_mut() {
            window.ledger.clear();
            if let Some(tree) = window.tree.as_ref() {
                window.ledger.apply(tree);
            }
        }

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
        self.apply_tree(window_id, Tree::decode(batch)?)
    }

    /// Installs an already-decoded, already-validated snapshot.
    ///
    /// Split from [`Host::on_guest_commit`] because the two-thread bridge must
    /// decode at a specific point in a normative sequence — after the
    /// generation check, before anything is mutated — and so cannot use a
    /// function that does both at once.
    ///
    /// # The snapshot is diffed, not swapped
    ///
    /// The guest sends a whole interface every time; the host keeps the one it
    /// already has and works out what differs (see [`instar_ui::diff`]). Nodes
    /// are not destroyed and recreated because another snapshot arrived — the
    /// retained tree is the interaction and layout object, and the snapshot is
    /// only a description of what it should now look like.
    ///
    /// # Atomicity
    ///
    /// The diff runs **before** anything is mutated, and can refuse
    /// ([`TreeError::KindChanged`]). A refused diff therefore leaves the
    /// previous interface standing exactly as a refused decode does, which is
    /// the property the whole commit path is built around. The promotion below
    /// is still one assignment after all validation.
    pub fn apply_tree(
        &mut self,
        window_id: WindowId,
        tree: Tree,
    ) -> Result<Vec<HostEffect>, TreeError> {
        let window = self.windows.entry(window_id).or_default();

        // Before the mutation, so a refusal costs the previous interface
        // nothing. The ledger check sits beside the diff for the same reason:
        // a snapshot that reuses a retired id must leave the previous
        // interface standing exactly as a refused diff does.
        let changes = instar_ui::diff(window.tree.as_ref(), &tree)?;
        window.ledger.validate(&tree)?;

        // A guest re-committing an identical interface is an ordinary shape —
        // it is what an event the guest decided to ignore looks like from
        // here. It should cost the decode and nothing else: no layout, no
        // scene, no frame.
        // (An opening commit always reports its nodes as created, so this
        // cannot swallow a guest's first interface.)
        if changes.is_empty() {
            return Ok(Vec::new());
        }

        // Validation ran above the early return, so a no-op commit cannot
        // dodge the lifecycle rules. `apply` sits after it: an identical
        // snapshot has identical live keys, so applying would only redo a
        // no-op, and the no-op commit keeps costing the decode and nothing
        // else.
        window.ledger.apply(&tree);

        // A snapshot that survived the diff is a new version of the retained
        // tree. The guest-visible commit sequence advances on every accepted
        // commit; this is the host's separate value for whether the state
        // actually changed, and the one caches key off.
        window.tree_revision += 1;

        // Before the new snapshot becomes interactive: any transient state
        // referring to a node the guest removed is retired. A press that
        // outlived its node would otherwise be completable against whatever
        // reused its key. See `Interaction::retire`.
        window.interaction.retire(&changes.removed);
        self.text.retire(&changes.removed);
        // Deletion destroys a viewport's offset; hiding does not. This is the
        // deletion half, and it sits with the other retirements for the same
        // reason -- state that outlives the node it describes eventually
        // lands on something else.
        window.scroll.retire(&changes.removed);

        window.tree = Some(tree);
        // And any state referring to a node the guest *hid*, which the diff
        // does not report as removed because it is still in the tree. Runs
        // after the promotion rather than before it, because the question is
        // about the new snapshot -- what can still be reached now -- whereas
        // `retire` above asks about keys the new snapshot no longer contains.
        // Both are before the interface becomes interactive, which is the
        // property that matters. See `Interaction::retire_hidden`.
        if let Some(tree) = window.tree.as_ref() {
            window.interaction.retire_hidden(tree);
        }
        window.recompute_layout(&mut self.text);
        // After layout, because the scrollable extent is a layout answer, and
        // before the scene is lowered and the commit is acknowledged, because
        // both of those are the interface becoming interactive. Content that
        // shrank must not leave a viewport showing a region that is no longer
        // there.
        window.clamp_scroll();
        // Lowered here rather than on the next frame callback: the caller is
        // about to tell a guest its interface was accepted, and "accepted"
        // should mean the host has everything it needs to show it.
        self.rebuild_scene(window_id);

        let window = self.windows.entry(window_id).or_default();
        if window.metrics.is_ready() {
            Ok(vec![HostEffect::Render { window: window_id }])
        } else {
            // Nothing to draw against yet; remember that something wants
            // drawing once there is.
            window.redraw_pending = true;
            Ok(Vec::new())
        }
    }

    fn on_metrics_changed(&mut self, metrics: WindowMetricsChanged) -> Vec<HostEffect> {
        let window_id = metrics.window_id;
        let window = self.windows.entry(window_id).or_default();
        window.metrics.ready(metrics);

        // Order matters and is the barrier's exit rule: layout first, then the
        // snapshot is replaced, then the scene is lowered against it, and only
        // then may anything be rendered.
        window.recompute_layout(&mut self.text);
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
        let scroll = &window.scroll;
        let mut effects = match event.state {
            PointerState::Pressed => {
                window.interaction.on_press(tree, layout, scroll, x, y);
                Vec::new()
            }
            PointerState::Released => window
                .interaction
                .on_release(tree, layout, scroll, x, y)
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
    use crate::bridge::{HostBridge, HostUserEvent, Wake};
    use instar_kernel::bridge::commit_request;
    use instar_ui::protocol::{BatchEncoder, NodeKey, WireAlign, WireLayout, flags, opcode};
    use instar_window::{LogicalSize, PhysicalSize, PointerButton};
    use std::time::{Duration, Instant};

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
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey::first(1), 0, None, fill, 2)
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(2),
                0,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey::first(3),
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
            .and_then(|layout| layout.get(NodeKey::first(3)))
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
            vec![&UiAction::ButtonActivated(NodeKey::first(3)).encode()],
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

    // --- Snapshot diffing (Stage 0) ---

    /// A batch whose key 2 is a *button* where [`counter_batch`] makes it text.
    fn kind_swapped_batch() -> Vec<u8> {
        let fill = WireLayout {
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey::first(1), 0, None, fill, 1)
            .node(
                opcode::NODE_BUTTON,
                NodeKey::first(2),
                flags::ENABLED,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    // --- Commit sequence vs tree revision ---
    //
    // The bridge carries two counters now. `commit_sequence` is the guest's:
    // it advances on every accepted commit, no-op or not. `tree_revision` is
    // the host's: it advances only when the diff found something. These three
    // tests drive a real bridge directly, so the divergence is observable at
    // the same place a consumer would read it.

    fn component() -> Vec<u8> {
        std::fs::read(env!("HOSTILE_WASM")).expect("the hostile guest is built by build.rs")
    }

    /// A bridge whose guest has finished its opening commit, so the manual
    /// commits below start from a known retained tree.
    fn ready_bridge() -> HostBridge {
        let wake: Wake = Arc::new(|| {});
        let mut bridge = HostBridge::spawn(component(), WINDOW, wake).expect("the guest starts");
        wait_for_commit(&mut bridge);
        bridge
    }

    /// Waits for one more accepted guest commit.
    fn wait_for_commit(bridge: &mut HostBridge) {
        let target = bridge.commit_sequence() + 1;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            bridge.wait(Duration::from_millis(50));
            if bridge.commit_sequence() >= target {
                return;
            }
        }
        panic!("the guest never committed");
    }

    /// Delivers a batch as if the guest had just committed it.
    fn deliver(bridge: &mut HostBridge, batch: Vec<u8>) {
        let (request, _reply) = commit_request(bridge.generation(), batch);
        bridge.on_user_event(HostUserEvent::UiCommit {
            generation: bridge.generation(),
            request,
        });
    }

    #[test]
    fn an_accepted_change_bumps_both_counters() {
        let mut bridge = ready_bridge();
        let sequence = bridge.commit_sequence();
        let tree = bridge.tree_revision();

        deliver(&mut bridge, counter_batch());

        assert_eq!(
            bridge.commit_sequence(),
            sequence + 1,
            "an accepted commit is a new event for the guest"
        );
        assert_eq!(
            bridge.tree_revision(),
            tree + 1,
            "and a changed snapshot is a new version of the retained tree"
        );
        bridge.shutdown();
    }

    #[test]
    fn an_identical_recommit_bumps_the_commit_sequence_but_not_the_tree_revision() {
        let mut bridge = ready_bridge();
        deliver(&mut bridge, counter_batch());
        let sequence = bridge.commit_sequence();
        let tree = bridge.tree_revision();

        deliver(&mut bridge, counter_batch());

        assert_eq!(
            bridge.commit_sequence(),
            sequence + 1,
            "the guest still gets a new sequence number for its accepted commit"
        );
        assert_eq!(
            bridge.tree_revision(),
            tree,
            "an identical re-commit must not claim a new tree exists"
        );
        bridge.shutdown();
    }

    #[test]
    fn a_refused_commit_bumps_neither_counter() {
        let mut bridge = ready_bridge();
        deliver(&mut bridge, counter_batch());
        let sequence = bridge.commit_sequence();
        let tree = bridge.tree_revision();

        deliver(&mut bridge, kind_swapped_batch());

        assert_eq!(
            bridge.stats().rejected_commits,
            1,
            "the kind swap should actually be refused"
        );
        assert_eq!(bridge.commit_sequence(), sequence);
        assert_eq!(bridge.tree_revision(), tree);
        bridge.shutdown();
    }

    /// A guest re-committing an interface it did not change is an ordinary
    /// shape — it is what an event the guest chose to ignore looks like from
    /// here. It should cost the decode and nothing else.
    #[test]
    fn recommitting_an_identical_snapshot_asks_for_no_frame() {
        let mut host = ready_host();
        let before = host.window(WINDOW).unwrap().scene().cloned();

        let effects = host
            .on_guest_commit(WINDOW, &counter_batch())
            .expect("valid batch");

        assert!(
            effects.is_empty(),
            "an unchanged interface should not ask for a frame, got {effects:?}"
        );
        assert_eq!(
            host.window(WINDOW).unwrap().scene().cloned(),
            before,
            "and the scene it was already showing should be untouched"
        );
    }

    /// The host holds transient state against keys — focus, scroll offset, an
    /// in-flight press. Silently swapping the node behind a key would move that
    /// state onto an unrelated control, so a guest that reuses a key for a
    /// different kind of node is refused rather than accommodated.
    #[test]
    fn reusing_a_key_for_a_different_kind_of_node_is_refused() {
        let mut host = ready_host();
        let before = host.window(WINDOW).unwrap().tree().cloned();

        let refused = host.on_guest_commit(WINDOW, &kind_swapped_batch());

        assert!(
            matches!(refused, Err(TreeError::KindChanged { .. })),
            "expected a KindChanged refusal, got {refused:?}"
        );
        assert_eq!(
            host.window(WINDOW).unwrap().tree().cloned(),
            before,
            "a refused diff must leave the previous interface standing, exactly \
             as a refused decode does"
        );
    }

    /// The diff runs before anything is mutated, so a refusal cannot leave the
    /// host half-updated — and the interface it was already showing keeps
    /// working afterwards.
    #[test]
    fn a_refused_snapshot_leaves_a_working_interface_behind() {
        let mut host = ready_host();
        assert!(host.on_guest_commit(WINDOW, &kind_swapped_batch()).is_err());

        let (x, y) = button_centre(&host);
        host.handle(pointer(PointerState::Pressed, x, y));
        assert_eq!(
            to_guest(&host.handle(pointer(PointerState::Released, x, y))).len(),
            1,
            "the surviving interface should still be clickable"
        );
    }

    /// A batch with the counter's button removed, keeping every other key.
    fn batch_without_the_button() -> Vec<u8> {
        let fill = WireLayout {
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey::first(1), 0, None, fill, 1)
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(2),
                0,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    /// A batch with the counter's shape plus a button under id 7 at
    /// `generation`.
    fn batch_with_button_7(generation: u32) -> Vec<u8> {
        let fill = WireLayout {
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey::first(1), 0, None, fill, 2)
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(2),
                0,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey::new(7, generation),
                flags::ENABLED,
                Some("Press me"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    /// The counter batch with button 3 at `generation`.
    fn counter_batch_with_button_generation(generation: u32) -> Vec<u8> {
        let fill = WireLayout {
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey::first(1), 0, None, fill, 2)
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(2),
                0,
                Some("Clicked 0 times"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey::new(3, generation),
                flags::ENABLED,
                Some("Press me"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    /// press -> the guest removes the node -> the guest reuses the id at a
    /// new generation -> release. Nothing else catches this: the kind is
    /// unchanged, so `KindChanged` does not fire, and the scale never moved,
    /// so the geometry barrier does not either.
    #[test]
    fn a_press_cannot_be_completed_against_a_node_that_reused_its_key() {
        let mut host = ready_host();
        let (x, y) = button_centre(&host);
        host.handle(pointer(PointerState::Pressed, x, y));

        // The guest drops the button, then brings it back under the same id
        // at generation 1.
        host.on_guest_commit(WINDOW, &batch_without_the_button())
            .expect("valid batch");
        host.on_guest_commit(WINDOW, &counter_batch_with_button_generation(1))
            .expect("valid batch");

        let release = host.handle(pointer(PointerState::Released, x, y));
        assert!(
            to_guest(&release).is_empty(),
            "the press belonged to a node that no longer exists; completing it \
             against whatever reused the id would activate a control the user \
             never touched"
        );
    }

    /// A viewport whose content shrank must not keep pointing past the end.
    ///
    /// Driven through `apply_tree` rather than by calling the clamp directly,
    /// because the property is about *ordering* — the offset is confined
    /// before the new snapshot is lowered and acknowledged, not at some later
    /// convenient moment.
    #[test]
    fn shrinking_content_clamps_a_retained_offset() {
        use instar_ui::{Node, ScrollOffset, WireLayout, WireSize};

        let build = |content_height: u16| {
            Tree::new(Node::root(
                0,
                vec![
                    Node::scroll(
                        10,
                        Node::text(11, "content").with_layout(WireLayout {
                            height: WireSize::Fixed(content_height),
                            ..WireLayout::default()
                        }),
                    )
                    .with_layout(WireLayout {
                        height: WireSize::Fixed(100),
                        ..WireLayout::default()
                    }),
                ],
            ))
        };

        let mut host = ready_host();
        host.apply_tree(WINDOW, build(500)).expect("valid tree");

        let window = host.windows.get_mut(&WINDOW).unwrap();
        window
            .scroll
            .set(NodeKey::first(10), ScrollOffset::new(0, 300));
        assert_eq!(
            window.scroll.get(NodeKey::first(10)).y,
            300,
            "400 of scrollable extent leaves 300 reachable"
        );

        // The same viewport over content that now barely overflows it.
        host.apply_tree(WINDOW, build(150)).expect("valid tree");
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(10))
                .y,
            50,
            "150 of content in a 100 viewport leaves 50, and the offset is \
             pulled back to it"
        );

        host.apply_tree(WINDOW, build(80)).expect("valid tree");
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(10))
                .y,
            0,
            "content that fits leaves nothing to scroll"
        );
    }

    /// Deletion destroys the offset; a commit that keeps the viewport does not.
    #[test]
    fn a_deleted_viewport_loses_its_offset_and_a_surviving_one_keeps_it() {
        use instar_ui::{Node, ScrollOffset, WireLayout, WireSize};

        let with_scroll = || {
            Tree::new(Node::root(
                0,
                vec![
                    Node::scroll(
                        10,
                        Node::text(11, "content").with_layout(WireLayout {
                            height: WireSize::Fixed(500),
                            ..WireLayout::default()
                        }),
                    )
                    .with_layout(WireLayout {
                        height: WireSize::Fixed(100),
                        ..WireLayout::default()
                    }),
                ],
            ))
        };

        let mut host = ready_host();
        host.apply_tree(WINDOW, with_scroll()).expect("valid tree");
        host.windows
            .get_mut(&WINDOW)
            .unwrap()
            .scroll
            .set(NodeKey::first(10), ScrollOffset::new(0, 200));

        // A commit that changes something else entirely leaves the offset be.
        let mut kept = with_scroll();
        kept.root.children.push(Node::text(12, "sibling"));
        host.apply_tree(WINDOW, kept).expect("valid tree");
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(10))
                .y,
            200,
            "a commit that leaves the viewport alive preserves its offset"
        );

        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(0, vec![Node::text(12, "gone")])),
        )
        .expect("valid tree");
        assert_eq!(
            host.window(WINDOW).unwrap().scroll.len(),
            0,
            "deleting the viewport destroys the offset rather than orphaning it"
        );
    }

    /// The ledger closes the queued-event hole end to end: an id that was
    /// live, then removed, cannot come back at the same generation even when
    /// the snapshot itself is otherwise valid.
    #[test]
    fn a_removed_id_cannot_come_back_at_the_same_generation() {
        let mut host = Host::new();
        host.on_guest_commit(WINDOW, &batch_with_button_7(0))
            .expect("the first lifetime of id 7 is accepted");
        host.on_guest_commit(WINDOW, &counter_batch())
            .expect("removing id 7 is accepted");

        let before = host.window(WINDOW).unwrap().tree().cloned();
        assert_eq!(
            host.on_guest_commit(WINDOW, &batch_with_button_7(0)),
            Err(TreeError::GenerationNotAdvanced {
                key: NodeKey::first(7),
                retired: 0,
            }),
            "a stale event for id 7 names a node the ledger has retired"
        );
        assert_eq!(
            host.window(WINDOW).unwrap().tree().cloned(),
            before,
            "the refusal must leave the previous interface standing"
        );

        host.on_guest_commit(WINDOW, &batch_with_button_7(1))
            .expect("the same id at a higher generation is a new node");
    }

    /// A cleared ledger must not desync from the tree still on screen.
    ///
    /// The window keeps showing the last interface after a guest exits, so the
    /// ids in it are still live as far as everything else is concerned. If
    /// `on_guest_gone` merely emptied the ledger, an identical re-commit would
    /// take the no-op path in `apply_tree` and never reach `ledger.apply` —
    /// leaving those ids unknown, and the first removal-then-reuse accepted at
    /// generation 0, which is the exact hole the ledger exists to close.
    #[test]
    fn a_dead_generation_leaves_the_ledger_agreeing_with_the_tree_on_screen() {
        let mut host = Host::new();
        host.on_guest_commit(WINDOW, &batch_with_button_7(0))
            .expect("the first lifetime of id 7 is accepted");

        host.on_guest_gone(WINDOW, GenerationId(1), None);

        // The identical snapshot: a no-op commit that never reaches `apply`.
        host.on_guest_commit(WINDOW, &batch_with_button_7(0))
            .expect("re-committing the interface on screen is a no-op, not a violation");
        host.on_guest_commit(WINDOW, &counter_batch())
            .expect("removing id 7 is accepted");

        assert_eq!(
            host.on_guest_commit(WINDOW, &batch_with_button_7(0)),
            Err(TreeError::GenerationNotAdvanced {
                key: NodeKey::first(7),
                retired: 0,
            }),
            "id 7 was live in the tree the dead generation left on screen, so \
             reusing it must still require a higher generation"
        );
    }

    /// The same shape, but the node never leaves. Retirement must not become a
    /// blanket "any commit cancels the press" — that would break click-through
    /// on any interface that updates while the button is held.
    #[test]
    fn a_press_survives_a_commit_that_leaves_its_node_alone() {
        let mut host = ready_host();
        let (x, y) = button_centre(&host);
        host.handle(pointer(PointerState::Pressed, x, y));

        // A commit that changes the label text but keeps the button.
        host.on_guest_commit(WINDOW, &foreign_text_batch("Clicked 1 times"))
            .expect("valid batch");

        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::first(3)),
            "an unrelated commit must not cancel a press in progress"
        );
    }

    /// The counter batch with different label text, so a commit can change
    /// something without touching the button.
    fn foreign_text_batch(text: &str) -> Vec<u8> {
        let fill = WireLayout {
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, fill, 1)
            .node(opcode::NODE_COLUMN, NodeKey::first(1), 0, None, fill, 2)
            .node(
                opcode::NODE_TEXT,
                NodeKey::first(2),
                0,
                Some(text),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NodeKey::first(3),
                flags::ENABLED,
                Some("Press me"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
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
            .and_then(|l| l.get(NodeKey::first(1)))
            .unwrap();

        // Same logical size, double the physical size: layout must not move.
        host.handle(WindowOutput::MetricsChanged(metrics(2.0)));
        let after = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey::first(1)))
            .unwrap();

        assert_eq!(
            wide, after,
            "doubling the scale factor while the logical size is unchanged must \
             not move anything: instar-ui never sees DPI"
        );
    }
}
