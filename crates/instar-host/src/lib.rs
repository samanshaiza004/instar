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
use instar_ui::{
    FocusMove, FocusState, Interaction, KeyLedger, ScrollOffset, ScrollState, ScrollbarPart,
    TextContext, TreeError, UiAction, Viewport,
};
use instar_window::{
    LogicalPoint, PointerState, RawPointerEvent, RawScrollEvent, ScrollDelta, WindowId,
    WindowMetricsChanged, WindowOutput,
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
    /// What the keyboard is pointed at. Host-owned, like every other
    /// transient interaction state.
    focus: FocusState,
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

/// Every viewport's scrollable extent: content size less viewport size, floored
/// at zero.
///
/// Computed once per event rather than per viewport visited, because the
/// bubbling walk may ask about several and each answer is two lookups.
fn scroll_extents(
    tree: &Tree,
    layout: &LayoutSnapshot,
) -> HashMap<NodeKey, instar_ui::ScrollOffset> {
    let mut extents = HashMap::new();
    for node in tree.iter() {
        let Some(content) = instar_ui::scroll::content_of(node) else {
            continue;
        };
        let (Some(viewport), Some(content)) = (layout.get(node.key), layout.get(content.key))
        else {
            continue;
        };
        extents.insert(
            node.key,
            instar_ui::ScrollOffset::new(
                (content.width - viewport.width).max(0),
                (content.height - viewport.height).max(0),
            ),
        );
    }
    extents
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
            WindowOutput::Scroll(event) => self.on_scroll(event),
            WindowOutput::Key(event) => self.on_key(event),
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
            // Focus joins the same site, and covers removal, hiding,
            // Display::None and disabling with one question: can the keyboard
            // still reach it? A generational key makes a reused id answer no
            // without a rule saying so.
            window.focus.retire(tree);
        }
        // Only when something that can move a rectangle changed. A paint-only
        // commit -- a colour, a border, a corner radius -- keeps the geometry
        // and the shaped text it already has.
        //
        // This is what makes the guarantee structural rather than lucky. A
        // relayout would call `finalize` on every text node, and while an
        // unchanged width happens to reuse rather than re-extract today, that
        // is a property of the cache's internals rather than of this path.
        // Not entering it at all cannot regress.
        if changes.needs_layout() {
            window.recompute_layout(&mut self.text);
        }
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
        // A thumb drag is the same argument. Its arithmetic runs from the
        // pointer position and the track geometry it began against, and a
        // resize or scale change replaces both -- so the drag is cancelled
        // before the replacement geometry becomes interactive, exactly as a
        // press is. Hover goes too: it describes a scrollbar that is about to
        // be somewhere else.
        window.scroll.cancel_drag();
        window.scroll.set_hovered(None);
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
        if window.metrics.usable().is_none() {
            return Vec::new();
        }

        let (x, y) = event.logical_pos.round();

        // Scrollbar chrome is consulted before the content, and consumes the
        // event when it answers. A thumb sits over the viewport it belongs to,
        // so letting the content see the same press would activate whatever is
        // underneath the scrollbar.
        if let Some(effects) = self.on_scrollbar_pointer(event, x, y) {
            return effects;
        }

        let window = self.windows.entry(event.window_id).or_default();
        let (Some(_), Some(tree), Some(layout)) = (
            window.metrics.usable(),
            window.tree.as_ref(),
            window.layout.as_ref(),
        ) else {
            return Vec::new();
        };
        let held = window.interaction.pressed();
        let scroll = &window.scroll;
        let mut effects = match event.state {
            PointerState::Pressed => {
                window.interaction.on_press(tree, layout, scroll, x, y);
                // A click moves focus to whatever it landed on, or clears it.
                // `focus_visible` stays false: a keyboard-style ring after
                // every mouse click is noise, and deciding that here keeps the
                // guest from having to track input modality.
                let hit = tree
                    .hit_test_scrolled(layout, scroll, x, y)
                    .map(|node| node.key);
                window.focus.focus_by_pointer(hit);
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

    /// A wheel or touchpad scroll.
    ///
    /// The whole response is host-local: find the viewports under the pointer,
    /// move their offsets, redraw. There is deliberately no branch here that
    /// can reach the guest — Stage 3's acceptance is that ordinary scrolling
    /// produces zero `SendToGuest`, and the way to make that true is for the
    /// path not to exist rather than for a test to keep watch over one that
    /// does.
    fn on_scroll(&mut self, event: RawScrollEvent) -> Vec<HostEffect> {
        let Some(window) = self.windows.get_mut(&event.window_id) else {
            return Vec::new();
        };
        // The metrics barrier applies: geometry computed for a scale that has
        // since changed is exactly what must not be scrolled against.
        let (Some(_), Some(tree), Some(layout)) = (
            window.metrics.usable(),
            window.tree.as_ref(),
            window.layout.as_ref(),
        ) else {
            return Vec::new();
        };

        let (x, y) = event.logical_pos.round();
        let delta = match event.delta {
            ScrollDelta::Logical { x, y } => instar_ui::ScrollDeltaPixels::new(x, y),
            // A count becomes a distance here, where how far a line is is a UI
            // policy question rather than a windowing fact.
            ScrollDelta::Lines { x, y } => instar_ui::ScrollDeltaPixels::new(
                x * instar_ui::scroll::LOGICAL_PIXELS_PER_LINE,
                y * instar_ui::scroll::LOGICAL_PIXELS_PER_LINE,
            ),
        };

        let extents = scroll_extents(tree, layout);
        let outcome = instar_ui::scroll::apply_wheel(
            tree,
            layout,
            &mut window.scroll,
            &|key| extents.get(&key).copied(),
            x,
            y,
            delta,
        );

        // Nothing moved -- a viewport already at its limit, or no viewport
        // under the pointer at all. No frame: a redraw that changes no pixel
        // is still a frame somebody paid for, and a wheel held at the end of a
        // list would produce a stream of them.
        if !outcome.consumed {
            return Vec::new();
        }

        self.rebuild_scene(event.window_id);
        vec![HostEffect::Render {
            window: event.window_id,
        }]
    }

    /// Pointer handling for scrollbar chrome. `None` means "not ours".
    ///
    /// Entirely host-local, like the wheel: there is no branch here that can
    /// reach the guest, which is what makes the zero-`SendToGuest` property
    /// structural rather than something a test has to keep watch over.
    fn on_scrollbar_pointer(
        &mut self,
        event: RawPointerEvent,
        x: i32,
        y: i32,
    ) -> Option<Vec<HostEffect>> {
        let bars = self.scrollbars(event.window_id);
        let window = self.windows.get_mut(&event.window_id)?;
        window.metrics.usable()?;

        // A drag in progress owns the pointer wherever it goes. Without this,
        // a fast drag outruns the thumb between events, lands on the track,
        // and turns into a page-step.
        if window.scroll.dragging().is_some() {
            match event.state {
                PointerState::Released => {
                    window.scroll.cancel_drag();
                    // The thumb is drawn differently while held, so letting go
                    // is a visual change even though nothing moved.
                    self.rebuild_scene(event.window_id);
                    return Some(vec![HostEffect::Render {
                        window: event.window_id,
                    }]);
                }
                PointerState::Pressed => return Some(Vec::new()),
            }
        }

        let (viewport, bar, extent) = bars
            .iter()
            .rev()
            .find_map(|(key, bar, extent)| bar.part_at(x, y).map(|_| (*key, *bar, *extent)))?;
        let part = bar.part_at(x, y)?;

        match event.state {
            PointerState::Pressed => {
                let offset = window.scroll.get(viewport);
                match part {
                    ScrollbarPart::Thumb => {
                        window.scroll.begin_drag(instar_ui::ThumbDrag {
                            viewport,
                            origin_pointer_y: y,
                            origin_offset_y: offset.y,
                        });
                    }
                    ScrollbarPart::Track => {
                        // A page, towards the click. Deterministic rather than
                        // animated: an animation is a frame loop, and this
                        // host does not have one.
                        let viewport_height = bar.track.height;
                        let step = if y < bar.thumb.y {
                            -viewport_height
                        } else {
                            viewport_height
                        };
                        let moved = ScrollOffset::new(offset.x, offset.y + step)
                            .clamped(ScrollOffset::new(0, extent));
                        window.scroll.set(viewport, moved);
                    }
                }
                self.rebuild_scene(event.window_id);
                Some(vec![HostEffect::Render {
                    window: event.window_id,
                }])
            }
            PointerState::Released => Some(Vec::new()),
        }
    }

    /// Every viewport's scrollbar, outermost first, in absolute logical
    /// coordinates.
    fn scrollbars(&self, window_id: WindowId) -> Vec<(NodeKey, instar_ui::Scrollbar, i32)> {
        let Some(window) = self.windows.get(&window_id) else {
            return Vec::new();
        };
        let (Some(tree), Some(layout)) = (window.tree.as_ref(), window.layout.as_ref()) else {
            return Vec::new();
        };
        let mut bars = Vec::new();
        for node in tree.iter() {
            if !instar_ui::is_presented(node) {
                continue;
            }
            let Some(content) = instar_ui::scroll::content_of(node) else {
                continue;
            };
            let (Some(viewport), Some(content_rect)) =
                (layout.get(node.key), layout.get(content.key))
            else {
                continue;
            };
            let extent = (content_rect.height - viewport.height).max(0);
            if let Some(bar) = instar_ui::Scrollbar::for_viewport(
                viewport,
                content_rect.height,
                window.scroll.get(node.key).y,
            ) {
                bars.push((node.key, bar, extent));
            }
        }
        bars
    }

    /// A pointer move: continues a thumb drag, or updates hover.
    ///
    /// Separate from [`Host::on_pointer`] because a move is not a button
    /// event, and because both of the things it can do are pure presentation.
    pub fn on_pointer_moved(&mut self, window_id: WindowId, x: i32, y: i32) -> Vec<HostEffect> {
        let bars = self.scrollbars(window_id);
        let Some(window) = self.windows.get_mut(&window_id) else {
            return Vec::new();
        };
        if window.metrics.usable().is_none() {
            return Vec::new();
        }

        if let Some(drag) = window.scroll.dragging() {
            let Some((_, bar, extent)) = bars.iter().find(|(key, _, _)| *key == drag.viewport)
            else {
                // The viewport stopped having a scrollbar underneath a live
                // drag -- content shrank, or it was hidden. Cancel rather than
                // keep computing against geometry that is gone.
                window.scroll.cancel_drag();
                return Vec::new();
            };
            // From the drag's origin, never accumulated from the last event:
            // accumulating drifts with rounding and lags the pointer after any
            // clamped movement.
            let travel = y - drag.origin_pointer_y;
            let origin_thumb_top = bar.track.y
                + if *extent > 0 {
                    ((bar.track.height - bar.thumb.height) as i64 * drag.origin_offset_y as i64
                        / *extent as i64) as i32
                } else {
                    0
                };
            let wanted = bar.offset_for_thumb_top(origin_thumb_top + travel, *extent);
            let before = window.scroll.get(drag.viewport);
            if before.y == wanted {
                return Vec::new();
            }
            window
                .scroll
                .set(drag.viewport, ScrollOffset::new(before.x, wanted));
            self.rebuild_scene(window_id);
            return vec![HostEffect::Render { window: window_id }];
        }

        let hovered = bars
            .iter()
            .rev()
            .find_map(|(key, bar, _)| bar.part_at(x, y).map(|part| (*key, part)));
        if !window.scroll.set_hovered(hovered) {
            return Vec::new();
        }
        self.rebuild_scene(window_id);
        vec![HostEffect::Render { window: window_id }]
    }

    /// A key went down or came up.
    ///
    /// E1 handles traversal only. Activation is E2, and character input is
    /// Phase 3's — this deliberately does not grow an opinion about any key it
    /// has not been given a retained-UI meaning for.
    ///
    /// Entirely host-local: moving focus is presentation, and there is no
    /// branch here that can reach the guest.
    fn on_key(&mut self, event: instar_window::RawKeyEvent) -> Vec<HostEffect> {
        if !event.pressed {
            return Vec::new();
        }
        let Some(window) = self.windows.get_mut(&event.window_id) else {
            return Vec::new();
        };
        if window.metrics.usable().is_none() {
            return Vec::new();
        }
        let Some(tree) = window.tree.as_ref() else {
            return Vec::new();
        };

        let moved = match event.key {
            instar_window::Key::Tab => {
                let direction = if event.shift {
                    FocusMove::Previous
                } else {
                    FocusMove::Next
                };
                window.focus.traverse(tree, direction)
            }
            _ => false,
        };
        if !moved {
            return Vec::new();
        }

        // Focus is drawn, so moving it is a visual change -- and *only* a
        // visual one. The scene is re-lowered; layout and shaping are not
        // touched, which is the structural invariant E1 asserts.
        self.rebuild_scene(event.window_id);
        vec![HostEffect::Render {
            window: event.window_id,
        }]
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

    // --- E1: focus lifecycle and traversal. ---

    fn key(k: instar_window::Key, shift: bool) -> WindowOutput {
        WindowOutput::Key(instar_window::RawKeyEvent {
            window_id: WINDOW,
            key: k,
            pressed: true,
            shift,
        })
    }

    fn focus_fixture() -> Tree {
        use instar_ui::Node;
        Tree::new(Node::root(
            0,
            vec![
                Node::text(90, "label"),
                Node::button(91, "first"),
                Node::button(92, "second"),
            ],
        ))
    }

    fn focused(host: &Host) -> Option<NodeKey> {
        host.window(WINDOW).and_then(|w| w.focus.focused())
    }

    #[test]
    fn tab_moves_focus_and_tells_the_guest_nothing() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");

        let effects = host.handle(key(instar_window::Key::Tab, false));
        assert_eq!(focused(&host), Some(NodeKey::first(91)));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, HostEffect::Render { .. })),
            "focus is drawn, so moving it asks for a frame"
        );
        assert!(
            to_guest(&effects).is_empty(),
            "traversal is presentation and must not reach the guest: {effects:?}"
        );

        host.handle(key(instar_window::Key::Tab, false));
        assert_eq!(focused(&host), Some(NodeKey::first(92)));
        host.handle(key(instar_window::Key::Tab, true));
        assert_eq!(
            focused(&host),
            Some(NodeKey::first(91)),
            "Shift+Tab goes back"
        );
    }

    /// The E1 structural invariant, modelled on C5: focus movement is paint.
    ///
    /// `reused` is in the tuple for the same reason it is there — the other
    /// three counters stay at zero even when layout re-runs and the cache
    /// hits, so only `reused` distinguishes "nothing asked the text system a
    /// question" from "it answered cheaply".
    #[test]
    fn moving_focus_enters_neither_layout_nor_shaping() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        let before = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey::first(91)));

        host.reset_text_stats();
        host.handle(key(instar_window::Key::Tab, false));
        host.handle(key(instar_window::Key::Tab, false));

        let stats = host.text_stats();
        assert_eq!(
            (
                stats.rebuilt,
                stats.relinebroken,
                stats.extracted,
                stats.reused
            ),
            (0, 0, 0, 0),
            "a focus ring is paint; if traversal starts running Parley or \
             Taffy that is architectural, not slow: {stats:?}"
        );
        assert_eq!(
            host.window(WINDOW)
                .and_then(HostWindow::layout)
                .and_then(|l| l.get(NodeKey::first(91))),
            before,
            "and nothing moved"
        );
    }

    #[test]
    fn a_click_moves_focus_without_showing_the_keyboard_ring() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        host.handle(key(instar_window::Key::Tab, false));
        assert!(host.window(WINDOW).unwrap().focus.focus_visible());

        let rect = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey::first(92)))
            .unwrap();
        host.handle(pointer(
            PointerState::Pressed,
            f64::from(rect.x + 1),
            f64::from(rect.y + 1),
        ));

        assert_eq!(
            focused(&host),
            Some(NodeKey::first(92)),
            "the click focused it"
        );
        assert!(
            !host.window(WINDOW).unwrap().focus.focus_visible(),
            "but a keyboard-style ring after every mouse click is noise"
        );
    }

    #[test]
    fn focus_is_retired_when_its_node_stops_being_reachable() {
        use instar_ui::Node;
        for (what, tree) in [
            (
                "removed",
                Tree::new(Node::root(0, vec![Node::button(92, "second")])),
            ),
            (
                "disabled",
                Tree::new(Node::root(
                    0,
                    vec![
                        Node::button(91, "first").disabled(),
                        Node::button(92, "second"),
                    ],
                )),
            ),
            (
                "hidden",
                Tree::new(Node::root(
                    0,
                    vec![
                        Node::button(91, "first").hidden(),
                        Node::button(92, "second"),
                    ],
                )),
            ),
        ] {
            let mut host = ready_host();
            host.apply_tree(WINDOW, focus_fixture()).expect("valid");
            host.handle(key(instar_window::Key::Tab, false));
            assert_eq!(focused(&host), Some(NodeKey::first(91)));

            host.apply_tree(WINDOW, tree).expect("valid");
            assert_eq!(
                focused(&host),
                None,
                "{what}: focus must not survive on a node the keyboard cannot \
                 reach"
            );
        }
    }

    /// The regression generational keys exist for, end to end through a
    /// commit rather than against `FocusState` directly.
    #[test]
    fn a_reused_id_does_not_inherit_focus_through_a_commit() {
        use instar_ui::{Node, NodeKind, WireLayout};

        let with_generation = |generation: u32| {
            Tree::new(Node::root(
                0,
                vec![Node {
                    key: NodeKey::new(93, generation),
                    kind: NodeKind::Button {
                        label: "reused".into(),
                        enabled: true,
                    },
                    layout: WireLayout::default(),
                    style: Default::default(),
                    children: Vec::new(),
                }],
            ))
        };

        let mut host = ready_host();
        host.apply_tree(WINDOW, with_generation(0)).expect("valid");
        host.handle(key(instar_window::Key::Tab, false));
        assert_eq!(focused(&host), Some(NodeKey::new(93, 0)));

        // Gone, then back at a new generation.
        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(0, vec![Node::text(94, "gap")])),
        )
        .expect("valid");
        host.apply_tree(WINDOW, with_generation(1)).expect("valid");

        assert_eq!(
            focused(&host),
            None,
            "a button that happens to reuse id 93 must not inherit the \
             keyboard from the one that had it"
        );
    }

    // --- D: scrollbar chrome, all of it host-local. ---

    fn scroll_fixture() -> Tree {
        use instar_ui::{Node, WireLayout, WireSize};
        Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    50,
                    Node::column(
                        51,
                        vec![
                            Node::text(52, "spacer").with_layout(WireLayout {
                                height: WireSize::Fixed(300),
                                ..WireLayout::default()
                            }),
                            Node::button(53, "target").with_layout(WireLayout {
                                height: WireSize::Fixed(40),
                                ..WireLayout::default()
                            }),
                            Node::text(54, "tail").with_layout(WireLayout {
                                height: WireSize::Fixed(60),
                                ..WireLayout::default()
                            }),
                        ],
                    ),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(100),
                    align_self: Some(instar_ui::WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        ))
    }

    fn scrolled_host() -> Host {
        let mut host = ready_host();
        host.apply_tree(WINDOW, scroll_fixture()).expect("valid");
        host
    }

    fn bar_of(host: &Host) -> instar_ui::Scrollbar {
        let window = host.window(WINDOW).unwrap();
        let layout = window.layout().unwrap();
        instar_ui::Scrollbar::for_viewport(
            layout.get(NodeKey::first(50)).unwrap(),
            layout.get(NodeKey::first(51)).unwrap().height,
            window.scroll.get(NodeKey::first(50)).y,
        )
        .expect("the fixture overflows and therefore has a scrollbar")
    }

    #[test]
    fn a_thumb_is_proportional_and_tracks_the_offset() {
        let mut host = scrolled_host();
        let at_top = bar_of(&host);

        // 100 of viewport over 400 of content is a quarter.
        assert_eq!(at_top.track.height, 100);
        assert_eq!(at_top.thumb.height, 25);
        assert_eq!(at_top.thumb.y, at_top.track.y, "at rest it sits at the top");

        host.windows
            .get_mut(&WINDOW)
            .unwrap()
            .scroll
            .set(NodeKey::first(50), ScrollOffset::new(0, 300));
        let at_bottom = bar_of(&host);
        assert_eq!(
            at_bottom.thumb.y + at_bottom.thumb.height,
            at_bottom.track.y + at_bottom.track.height,
            "at maximum offset the thumb is flush with the bottom of the track"
        );
    }

    #[test]
    fn content_that_fits_has_no_scrollbar_at_all() {
        use instar_ui::{Node, WireLayout, WireSize};
        let mut host = ready_host();
        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(
                0,
                vec![
                    Node::scroll(
                        60,
                        Node::text(61, "short").with_layout(WireLayout {
                            height: WireSize::Fixed(20),
                            ..WireLayout::default()
                        }),
                    )
                    .with_layout(WireLayout {
                        height: WireSize::Fixed(100),
                        ..WireLayout::default()
                    }),
                ],
            )),
        )
        .expect("valid");

        let window = host.window(WINDOW).unwrap();
        assert_eq!(
            instar_ui::Scrollbar::for_viewport(
                window.layout().unwrap().get(NodeKey::first(60)).unwrap(),
                20,
                0
            ),
            None,
            "a viewport with nothing to scroll gets no chrome, rather than a \
             full-length thumb that cannot move"
        );
    }

    #[test]
    fn hovering_the_thumb_repaints_and_tells_the_guest_nothing() {
        let mut host = scrolled_host();
        let bar = bar_of(&host);

        let effects = host.on_pointer_moved(WINDOW, bar.thumb.x + 2, bar.thumb.y + 2);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, HostEffect::Render { .. })),
            "hover is drawn, so it asks for a frame"
        );
        assert!(
            to_guest(&effects).is_empty(),
            "and says nothing to the guest"
        );
        assert_eq!(
            host.window(WINDOW).unwrap().scroll.hovered(),
            Some((NodeKey::first(50), ScrollbarPart::Thumb))
        );

        // Moving within the same part changes nothing, so no frame.
        assert!(
            host.on_pointer_moved(WINDOW, bar.thumb.x + 3, bar.thumb.y + 3)
                .is_empty(),
            "an unchanged hover must not ask for a redraw that changes no pixel"
        );
    }

    #[test]
    fn clicking_the_track_pages_towards_the_click() {
        let mut host = scrolled_host();
        let bar = bar_of(&host);

        // Below the thumb, which is at the top.
        let effects = host.handle(pointer(
            PointerState::Pressed,
            f64::from(bar.track.x + 2),
            f64::from(bar.track.y + bar.track.height - 2),
        ));
        assert!(to_guest(&effects).is_empty());
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(50))
                .y,
            100,
            "one viewport-height page down"
        );
    }

    /// The whole architectural claim, stated the way a user would see it.
    ///
    /// The guest is blocked for 100 ms across the entire drag. Asserting on
    /// `ScrollState` would be weaker: this checks the offset moved, the thumb
    /// moved, the *content* paints somewhere new, and hit-testing follows it —
    /// all of it while nothing can possibly have reached the guest, because
    /// the guest is not running.
    #[test]
    fn a_thumb_drag_stays_responsive_while_the_guest_is_blocked() {
        let mut host = scrolled_host();
        let bar = bar_of(&host);
        let target = NodeKey::first(53);

        let painted_y = |host: &Host| {
            host.window(WINDOW)
                .and_then(HostWindow::scene)
                .and_then(|scene| {
                    scene.commands.iter().find_map(|command| match command {
                        instar_paint::PaintCommand::FillRect { rect, .. } if rect.height == 40 => {
                            Some(rect.y)
                        }
                        _ => None,
                    })
                })
        };

        // The guest is stalled for the whole gesture. Nothing below touches
        // it, and the assertions hold regardless of when it wakes.
        let stalled_until = Instant::now() + Duration::from_millis(100);

        let grab = host.handle(pointer(
            PointerState::Pressed,
            f64::from(bar.thumb.x + 2),
            f64::from(bar.thumb.y + 2),
        ));
        assert!(
            to_guest(&grab).is_empty(),
            "grabbing says nothing to the guest"
        );
        assert!(
            host.window(WINDOW).unwrap().scroll.dragging().is_some(),
            "the thumb is held"
        );

        // Far enough to reach the end. 75px of thumb travel maps to 300 of
        // offset, so a 50px drag would leave the target at viewport y = 100 --
        // painted, but one pixel past the bottom edge and therefore not
        // hittable. Dragging to the limit puts it at the top instead.
        let mut effects = Vec::new();
        for step in 1..=20 {
            effects.extend(host.on_pointer_moved(
                WINDOW,
                bar.thumb.x + 2,
                bar.thumb.y + 2 + step * 5,
            ));
        }
        assert!(
            Instant::now() < stalled_until,
            "the whole drag completed inside the guest's stall window, which \
             is the point: the guest could not have participated"
        );
        assert!(
            to_guest(&effects).is_empty(),
            "dragging must never reach the guest: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, HostEffect::Render { .. })),
            "and must repaint"
        );

        // What a user sees: the offset moved, the thumb moved, the content
        // moved, and the button is clickable where it now appears.
        let window = host.window(WINDOW).unwrap();
        let offset = window.scroll.get(NodeKey::first(50)).y;
        assert!(offset > 0, "the drag scrolled something");

        let moved_bar = bar_of(&host);
        assert!(
            moved_bar.thumb.y > bar.thumb.y,
            "the thumb followed the pointer"
        );

        let content_y = painted_y(&host).expect("the target button is painted");
        assert!(
            content_y < 300 - offset + 1 && content_y >= 300 - offset - 1,
            "the content paints at its scrolled position: {content_y} for an \
             offset of {offset}"
        );

        let hit = window
            .tree()
            .unwrap()
            .hit_test_scrolled(window.layout().unwrap(), &window.scroll, 5, content_y + 2)
            .map(|node| node.key);
        assert_eq!(hit, Some(target), "and is clickable where it is drawn");
    }

    #[test]
    fn a_drag_keeps_the_pointer_even_when_it_leaves_the_scrollbar() {
        let mut host = scrolled_host();
        let bar = bar_of(&host);
        host.handle(pointer(
            PointerState::Pressed,
            f64::from(bar.thumb.x + 2),
            f64::from(bar.thumb.y + 2),
        ));

        // Far to the left of the scrollbar, and past the bottom of the window.
        host.on_pointer_moved(WINDOW, -500, bar.thumb.y + 40);
        assert!(
            host.window(WINDOW).unwrap().scroll.dragging().is_some(),
            "a drag survives the pointer leaving the thumb -- otherwise a fast \
             drag drops control the moment the pointer outruns it"
        );
        assert!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(50))
                .y
                > 0
        );

        host.handle(pointer(PointerState::Released, -500.0, 0.0));
        assert!(
            host.window(WINDOW).unwrap().scroll.dragging().is_none(),
            "release ends it wherever the pointer is"
        );
    }

    /// Direct manipulation of one container. Unlike the wheel, reaching the
    /// end must not start moving an ancestor.
    #[test]
    fn a_thumb_at_its_limit_does_not_scroll_an_ancestor() {
        use instar_ui::{Node, WireLayout, WireSize};
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    70,
                    Node::column(
                        71,
                        vec![
                            Node::scroll(
                                72,
                                Node::text(73, "inner").with_layout(WireLayout {
                                    height: WireSize::Fixed(200),
                                    ..WireLayout::default()
                                }),
                            )
                            .with_layout(WireLayout {
                                height: WireSize::Fixed(50),
                                align_self: Some(instar_ui::WireAlign::Stretch),
                                ..WireLayout::default()
                            }),
                            Node::text(74, "outer tail").with_layout(WireLayout {
                                height: WireSize::Fixed(400),
                                ..WireLayout::default()
                            }),
                        ],
                    ),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(100),
                    align_self: Some(instar_ui::WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        ));
        let mut host = ready_host();
        host.apply_tree(WINDOW, tree).expect("valid");

        let window = host.window(WINDOW).unwrap();
        let layout = window.layout().unwrap();
        let inner = instar_ui::Scrollbar::for_viewport(
            layout.get(NodeKey::first(72)).unwrap(),
            layout.get(NodeKey::first(73)).unwrap().height,
            0,
        )
        .expect("the inner viewport overflows");

        host.handle(pointer(
            PointerState::Pressed,
            f64::from(inner.thumb.x + 2),
            f64::from(inner.thumb.y + 2),
        ));
        // Far past the end of the inner track.
        host.on_pointer_moved(WINDOW, inner.thumb.x + 2, inner.thumb.y + 5_000);

        let window = host.window(WINDOW).unwrap();
        assert!(
            window.scroll.get(NodeKey::first(72)).y > 0,
            "the inner viewport scrolled to its end"
        );
        assert_eq!(
            window.scroll.get(NodeKey::first(70)).y,
            0,
            "and the outer one did not move: a thumb is a handle on one \
             container, and one that scrolled its parent at the end would be \
             lying about what it controls"
        );
    }

    #[test]
    fn invalidating_geometry_cancels_a_live_drag() {
        let mut host = scrolled_host();
        let bar = bar_of(&host);
        host.handle(pointer(
            PointerState::Pressed,
            f64::from(bar.thumb.x + 2),
            f64::from(bar.thumb.y + 2),
        ));
        assert!(host.window(WINDOW).unwrap().scroll.dragging().is_some());

        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });
        assert!(
            host.window(WINDOW).unwrap().scroll.dragging().is_none(),
            "the drag's arithmetic depends on geometry that no longer \
             describes the window"
        );
    }

    #[test]
    fn deleting_a_viewport_destroys_its_drag_and_hover() {
        use instar_ui::Node;
        let mut host = scrolled_host();
        let bar = bar_of(&host);
        host.on_pointer_moved(WINDOW, bar.thumb.x + 2, bar.thumb.y + 2);
        host.handle(pointer(
            PointerState::Pressed,
            f64::from(bar.thumb.x + 2),
            f64::from(bar.thumb.y + 2),
        ));
        assert!(host.window(WINDOW).unwrap().scroll.dragging().is_some());

        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(0, vec![Node::text(80, "gone")])),
        )
        .expect("valid");

        let window = host.window(WINDOW).unwrap();
        assert!(window.scroll.dragging().is_none(), "the drag went with it");
        assert!(window.scroll.hovered().is_none(), "so did the hover");
        assert!(window.scroll.is_empty(), "and the offset");
    }

    // --- C5: a paint-only change must not touch the text cache. ---

    /// The Stage 1 regression test, made mandatory.
    ///
    /// Both directions matter, and each catches a different mistake:
    ///
    /// - `rebuilt`/`relinebroken`/`extracted` at zero catches a colour change
    ///   being routed into shaping dirtiness. Nothing would fail if it were —
    ///   the picture would be right and the frame would just get slower, which
    ///   is precisely the failure `TextStats` exists to see.
    /// - the scene actually carrying the new colour catches the opposite
    ///   mistake: treating a paint-only change as a no-op and drawing nothing.
    ///
    /// A test asserting only the first would pass against a host that ignored
    /// style entirely.
    ///
    /// `reused` is in the tuple and is the one doing the work. The first three
    /// counters stay at zero even when layout *does* re-run, because the cache
    /// simply hits — so asserting only those proves nothing about whether the
    /// layout pass was skipped. `reused` counts finalize consulting the cache,
    /// which happens if and only if layout ran. This was written the weaker
    /// way first, and injecting the exact mistake it was meant to catch
    /// produced a green run.
    #[test]
    fn a_foreground_change_repaints_without_touching_the_text_cache() {
        use instar_ui::{Node, WireColor};

        let build = |foreground: Option<WireColor>| {
            let mut label = Node::text(30, "steady text");
            if let Some(color) = foreground {
                label = label.with_foreground(color);
            }
            Tree::new(Node::root(0, vec![label]))
        };

        let mut host = ready_host();
        host.apply_tree(WINDOW, build(None)).expect("valid tree");

        // From here on, nothing about the text itself changes.
        host.reset_text_stats();
        let red = WireColor::opaque(255, 0, 0);
        host.apply_tree(WINDOW, build(Some(red)))
            .expect("valid tree");

        let stats = host.text_stats();
        assert_eq!(
            (
                stats.rebuilt,
                stats.relinebroken,
                stats.extracted,
                stats.reused
            ),
            (0, 0, 0, 0),
            "a foreground change must not reshape, re-line-break, or \
             re-extract anything: {stats:?}"
        );

        // And the other direction: the new colour reached the scene.
        let scene = host
            .window(WINDOW)
            .and_then(HostWindow::scene)
            .expect("a paint-only commit still lowers a scene");
        let inks: Vec<instar_paint::Color> = scene
            .commands
            .iter()
            .filter_map(|command| match command {
                instar_paint::PaintCommand::GlyphRun { run } => Some(run.color),
                _ => None,
            })
            .collect();
        assert!(
            inks.contains(&instar_paint::Color::opaque(255, 0, 0)),
            "the glyphs should be painted in the requested foreground, got {inks:?}"
        );
    }

    /// The control for the test above: a font-size change *is* shaping work,
    /// so the same instrument must report it. Without this, a host that
    /// stopped shaping entirely would pass the zero-cost assertion.
    #[test]
    fn a_font_size_change_does_reshape() {
        use instar_ui::Node;

        let build = |size: u16| {
            Tree::new(Node::root(
                0,
                vec![Node::text(31, "steady text").with_font_size(size)],
            ))
        };

        let mut host = ready_host();
        host.apply_tree(WINDOW, build(14)).expect("valid tree");

        host.reset_text_stats();
        host.apply_tree(WINDOW, build(24)).expect("valid tree");

        let stats = host.text_stats();
        assert!(
            stats.rebuilt > 0,
            "a different font size must re-shape: {stats:?}"
        );
    }

    /// Cursor is interaction-only: it changes nothing measured and nothing
    /// drawn, so it must not reshape either.
    #[test]
    fn a_cursor_change_touches_neither_the_text_cache_nor_layout() {
        use instar_ui::{Node, WireCursor};

        let build = |cursor: WireCursor| {
            Tree::new(Node::root(
                0,
                vec![Node::text(32, "steady text").with_cursor(cursor)],
            ))
        };

        let mut host = ready_host();
        host.apply_tree(WINDOW, build(WireCursor::Default))
            .expect("valid tree");
        let before = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey::first(32)));

        host.reset_text_stats();
        host.apply_tree(WINDOW, build(WireCursor::Pointer))
            .expect("valid tree");

        let stats = host.text_stats();
        assert_eq!(
            (
                stats.rebuilt,
                stats.relinebroken,
                stats.extracted,
                stats.reused
            ),
            (0, 0, 0, 0),
            "a cursor change is not shaping work: {stats:?}"
        );
        assert_eq!(
            host.window(WINDOW)
                .and_then(HostWindow::layout)
                .and_then(|l| l.get(NodeKey::first(32))),
            before,
            "nor does it move anything"
        );
    }

    // --- C6: border composition. ---

    /// A thickly bordered node, big enough that a centred stroke would be
    /// unmistakable.
    fn bordered_tree() -> Tree {
        use instar_ui::{Node, WireColor, WireLayout, WireSize};
        Tree::new(Node::root(
            0,
            vec![
                Node::button(40, "bordered")
                    .with_layout(WireLayout {
                        width: WireSize::Fixed(80),
                        height: WireSize::Fixed(40),
                        ..WireLayout::default()
                    })
                    .with_border(6, WireColor::opaque(255, 0, 0))
                    .with_background(WireColor::opaque(0, 0, 255))
                    .with_corner_radius(5),
            ],
        ))
    }

    /// Every rectangle the host emits for a node lies within that node's
    /// laid-out rect.
    ///
    /// Asserted on the scene rather than on pixels, because this is the
    /// host's half of the contract: it must not *ask* for anything outside
    /// the rect. `instar-render-vello-cpu` proves the primitives then honour
    /// it. A centred stroke would fail at whichever layer invented it.
    #[test]
    fn nothing_a_bordered_node_paints_leaves_its_layout_rect() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, bordered_tree())
            .expect("valid tree");

        let bounds = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey::first(40)))
            .expect("the bordered node is laid out");
        let scene = host.window(WINDOW).and_then(HostWindow::scene).unwrap();

        let mut checked = 0;
        for command in &scene.commands {
            let rect = match command {
                instar_paint::PaintCommand::FillRect { rect, .. }
                | instar_paint::PaintCommand::StrokeRect { rect, .. }
                | instar_paint::PaintCommand::FillRoundedRect { rect, .. }
                | instar_paint::PaintCommand::StrokeRoundedRect { rect, .. } => *rect,
                _ => continue,
            };
            checked += 1;
            assert!(
                rect.x >= bounds.x
                    && rect.y >= bounds.y
                    && rect.x + rect.width as i32 <= bounds.x + bounds.width
                    && rect.y + rect.height as i32 <= bounds.y + bounds.height,
                "{rect:?} escapes the node's layout rect {bounds:?}"
            );
        }
        assert!(
            checked >= 2,
            "the fixture should emit at least a background and a border, got {checked}"
        );
    }

    /// Hit-testing uses the node's outer bounds, not the area inside its
    /// border.
    ///
    /// Stated as a test so nobody later decides the inset geometry is the
    /// "real" one. A 6px border on an 80x40 node would move every edge by
    /// six pixels, and a control whose clickable area is smaller than the
    /// control is a bug users report as "the button doesn't work near the
    /// edge".
    #[test]
    fn hit_test_bounds_are_the_visible_outer_bounds() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, bordered_tree())
            .expect("valid tree");

        let window = host.window(WINDOW).unwrap();
        let layout = window.layout().unwrap();
        let tree = window.tree().unwrap();
        let bounds = layout.get(NodeKey::first(40)).unwrap();
        let target = Some(NodeKey::first(40));

        // One pixel inside each outer edge, including the corners the radius
        // rounds -- hit-testing is rectangular and does not follow the curve.
        for (x, y, edge) in [
            (bounds.x, bounds.y, "top-left"),
            (bounds.x + bounds.width - 1, bounds.y, "top-right"),
            (bounds.x, bounds.y + bounds.height - 1, "bottom-left"),
            (
                bounds.x + bounds.width - 1,
                bounds.y + bounds.height - 1,
                "bottom-right",
            ),
        ] {
            assert_eq!(
                tree.hit_test(layout, x, y).map(|node| node.key),
                target,
                "the {edge} pixel is inside the control and must hit it"
            );
        }

        // And one pixel outside each edge.
        for (x, y, edge) in [
            (bounds.x - 1, bounds.y + 1, "left"),
            (bounds.x + bounds.width, bounds.y + 1, "right"),
            (bounds.x + 1, bounds.y - 1, "top"),
            (bounds.x + 1, bounds.y + bounds.height, "bottom"),
        ] {
            assert_ne!(
                tree.hit_test(layout, x, y).map(|node| node.key),
                target,
                "the pixel past the {edge} edge is outside the control"
            );
        }
    }

    #[test]
    fn a_zero_width_border_changes_neither_paint_nor_bounds() {
        use instar_ui::{Node, WireColor, WireLayout, WireSize};

        let build = |width: u16| {
            Tree::new(Node::root(
                0,
                vec![
                    Node::text(41, "x")
                        .with_layout(WireLayout {
                            width: WireSize::Fixed(40),
                            height: WireSize::Fixed(20),
                            ..WireLayout::default()
                        })
                        .with_border(width, WireColor::opaque(255, 0, 0)),
                ],
            ))
        };

        let mut host = ready_host();
        host.apply_tree(WINDOW, build(0)).expect("valid tree");
        let bounds = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey::first(41)));
        let strokes = |host: &Host| {
            host.window(WINDOW)
                .and_then(HostWindow::scene)
                .map(|scene| {
                    scene
                        .commands
                        .iter()
                        .filter(|command| {
                            matches!(
                                command,
                                instar_paint::PaintCommand::StrokeRect { .. }
                                    | instar_paint::PaintCommand::StrokeRoundedRect { .. }
                            )
                        })
                        .count()
                })
                .unwrap_or_default()
        };
        assert_eq!(strokes(&host), 0, "a zero-width border emits no stroke");

        host.apply_tree(WINDOW, build(4)).expect("valid tree");
        assert_eq!(strokes(&host), 1, "a real one does");
        assert_eq!(
            host.window(WINDOW)
                .and_then(HostWindow::layout)
                .and_then(|l| l.get(NodeKey::first(41))),
            bounds,
            "and a border never affects layout either way"
        );
    }

    // --- B2: the wheel, and the guest's absence from it. ---

    fn wheel(x: f64, y: f64, dy: f64) -> WindowOutput {
        WindowOutput::Scroll(instar_window::RawScrollEvent {
            window_id: WINDOW,
            logical_pos: LogicalPoint::new(x, y),
            delta: instar_window::ScrollDelta::Logical { x: 0.0, y: dy },
        })
    }

    /// A 100-tall viewport whose content is 500 tall, with a button at content
    /// y = 200 — below the fold until something scrolls.
    fn scrollable_tree() -> Tree {
        use instar_ui::{Node, WireLayout, WireSize};
        Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    10,
                    Node::column(
                        11,
                        vec![
                            Node::text(12, "spacer").with_layout(WireLayout {
                                height: WireSize::Fixed(200),
                                ..WireLayout::default()
                            }),
                            Node::button(13, "target").with_layout(WireLayout {
                                height: WireSize::Fixed(40),
                                ..WireLayout::default()
                            }),
                            Node::text(14, "tail").with_layout(WireLayout {
                                height: WireSize::Fixed(260),
                                ..WireLayout::default()
                            }),
                        ],
                    ),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(100),
                    ..WireLayout::default()
                }),
            ],
        ))
    }

    /// The acceptance test for the whole stage: a wheel moves the interface,
    /// and the guest never hears about it.
    ///
    /// Deliberately stronger than "the offset changed". The offset is an
    /// implementation detail; what a user experiences is that the button is
    /// *drawn* somewhere new and *clicks* somewhere new. Asserting only the
    /// offset would pass against an implementation that moved the number and
    /// forgot to re-lower the scene.
    #[test]
    fn a_wheel_scrolls_the_view_without_the_guest_hearing_anything() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, scrollable_tree())
            .expect("valid tree");

        let target = NodeKey::first(13);
        assert_eq!(
            host.window(WINDOW)
                .and_then(HostWindow::layout)
                .and_then(|l| l.get(target))
                .map(|rect| rect.y),
            Some(200),
            "the target starts at content y = 200, below a 100-tall viewport"
        );
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .tree()
                .unwrap()
                .hit_test_scrolled(
                    host.window(WINDOW).and_then(HostWindow::layout).unwrap(),
                    &host.window(WINDOW).unwrap().scroll,
                    5,
                    50,
                ),
            None,
            "and nothing is at viewport y = 50 before scrolling"
        );

        let effects = host.handle(wheel(5.0, 50.0, 150.0));

        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(10))
                .y,
            150,
            "the host-owned offset moved by the delta"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, HostEffect::Render { .. })),
            "and the window was asked to redraw"
        );
        assert!(
            to_guest(&effects).is_empty(),
            "ordinary scrolling must never reach the guest: {effects:?}"
        );

        // Painted somewhere new.
        let scene = host.window(WINDOW).and_then(HostWindow::scene).unwrap();
        // By height, because a scrolled viewport now also fills a scrollbar
        // track: chrome, not content, and it does not move with the offset.
        let filled: Vec<i32> = scene
            .commands
            .iter()
            .filter_map(|command| match command {
                instar_paint::PaintCommand::FillRect { rect, .. } if rect.height == 40 => {
                    Some(rect.y)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            filled,
            vec![50],
            "200 minus an offset of 150 puts the button at viewport y = 50"
        );

        // And clickable where it is drawn.
        let window = host.window(WINDOW).unwrap();
        assert_eq!(
            window
                .tree()
                .unwrap()
                .hit_test_scrolled(window.layout().unwrap(), &window.scroll, 5, 50)
                .map(|node| node.key),
            Some(target),
            "the same button is now reachable where it appears"
        );
    }

    /// A viewport already at its limit costs nothing.
    #[test]
    fn scrolling_past_the_end_produces_neither_a_guest_event_nor_a_frame() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, scrollable_tree())
            .expect("valid tree");

        // 500 of content in a 100 viewport leaves 400 to scroll.
        host.handle(wheel(5.0, 50.0, 400.0));
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(10))
                .y,
            400,
            "the fixture is now scrolled to its end"
        );

        let effects = host.handle(wheel(5.0, 50.0, 120.0));
        assert!(
            effects.is_empty(),
            "a wheel that moves nothing must produce no effect at all -- not a \
             guest event, and not a frame that would redraw identical pixels: \
             {effects:?}"
        );
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(10))
                .y,
            400,
            "and the offset is unchanged"
        );
    }

    /// The nested-scroll trap, which "the nearest scroll owns the event" walks
    /// straight into.
    #[test]
    fn an_inner_viewport_at_its_limit_hands_the_rest_to_its_ancestor() {
        use instar_ui::{Node, WireLayout, WireSize};

        // Outer viewport 100 tall over 400 of content; inner 50 tall over 100.
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    20,
                    Node::column(
                        21,
                        vec![
                            Node::scroll(
                                22,
                                Node::text(23, "inner").with_layout(WireLayout {
                                    height: WireSize::Fixed(100),
                                    ..WireLayout::default()
                                }),
                            )
                            .with_layout(WireLayout {
                                height: WireSize::Fixed(50),
                                ..WireLayout::default()
                            }),
                            Node::text(24, "outer tail").with_layout(WireLayout {
                                height: WireSize::Fixed(350),
                                ..WireLayout::default()
                            }),
                        ],
                    ),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(100),
                    ..WireLayout::default()
                }),
            ],
        ));

        let mut host = ready_host();
        host.apply_tree(WINDOW, tree).expect("valid tree");

        let inner = NodeKey::first(22);
        let outer = NodeKey::first(20);

        // Pointer inside the inner viewport. It can take 50; the rest is the
        // outer one's.
        host.handle(wheel(5.0, 10.0, 120.0));

        let window = host.window(WINDOW).unwrap();
        assert_eq!(
            window.scroll.get(inner).y,
            50,
            "the inner viewport takes what it has room for"
        );
        assert_eq!(
            window.scroll.get(outer).y,
            70,
            "and the remaining 70 scrolls the ancestor rather than vanishing"
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
