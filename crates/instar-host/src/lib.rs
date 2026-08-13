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

mod attachment;
pub mod bridge;
pub mod present;
pub mod text_host;
pub mod text_view;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::attachment::{StagedUiCommit, ValidatedUiCommit};
use instar_kernel::runtime::GenerationId;
use instar_kernel::text_bridge::{AttachmentRefusal, OpaqueResourceKey};
use instar_paint::PaintScene;
use instar_text::TextViewId;
use instar_ui::{
    DecodedUiSnapshot, FocusMove, FocusState, Interaction, KeyLedger, ScrollOffset, ScrollState,
    ScrollbarPart, ScrollbarStyle, TextAttachmentRef, TextContext, TreeError, UiAction, Viewport,
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
    /// Which `TextViewId` each text-view node in the retained tree shows.
    ///
    /// Resolved during admission and held beside the tree: the tree itself
    /// deliberately carries no resource identity (see
    /// [`instar_ui::NodeKind::TextView`]), and the slots a commit used are
    /// commit-local and gone by the time this map exists.
    text_attachments: BTreeMap<NodeKey, TextViewId>,
    /// A redraw asked for while blocked, to be serviced once ready.
    redraw_pending: bool,
    /// Updated even while blocked; acted on only when ready.
    last_pointer: Option<LogicalPoint>,
    /// What the platform accessibility adapter has already been told.
    a11y: instar_ui::A11yProjection,
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

    /// The text-view attachments of the retained tree, by node.
    pub fn text_attachments(&self) -> &BTreeMap<NodeKey, TextViewId> {
        &self.text_attachments
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

    /// Installs a staged snapshot and advances the tree revision.
    ///
    /// Infallible by construction: a [`StagedUiCommit`] has already passed
    /// every check that can refuse, so promotion has nothing left to report.
    /// It owns the two assignments that make the retained UI state — the tree
    /// and the attachment map — so no other path can update one without the
    /// other.
    pub(crate) fn promote_ui_commit(&mut self, staged: StagedUiCommit) {
        self.tree_revision += 1;
        self.tree = Some(staged.tree);
        self.text_attachments = staged.attachments;
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

    /// Where each viewport is scrolled to, and any live thumb drag.
    ///
    /// Read-only on purpose: scroll position is host-owned, and a caller that
    /// could set it would be a second author of the same state. Exposed
    /// because the shell's integration tests have to observe the *effect* of a
    /// gesture rather than call the function that performs it.
    pub fn scroll(&self) -> &instar_ui::ScrollState {
        &self.scroll
    }

    /// What has focus, and whether the ring is being shown.
    ///
    /// Read-only for the same reason as [`Self::scroll`].
    pub fn focus(&self) -> &instar_ui::FocusState {
        &self.focus
    }

    /// Recomputes layout from the current tree and metrics.
    ///
    /// Does nothing while blocked, which is the barrier's "no layout" rule
    /// enforced at the only place layout is produced.
    fn recompute_layout(&mut self, text: &mut TextContext, scrollbars: ScrollbarStyle) {
        let (Some(metrics), Some(tree)) = (self.metrics.usable(), self.tree.as_ref()) else {
            return;
        };
        let viewport = Viewport::new(
            metrics.logical_size.width as f32,
            metrics.logical_size.height as f32,
        );
        self.layout = Some(tree.layout_with(text, viewport, scrollbars));
    }

    /// Re-finalizes text against geometry that has not moved.
    ///
    /// The alignment-only path; see [`instar_ui::layout::refinalize_text`].
    fn refinalize_text(&mut self, text: &mut TextContext) {
        let (Some(tree), Some(layout)) = (self.tree.as_ref(), self.layout.as_mut()) else {
            return;
        };
        instar_ui::layout::refinalize_text(text, tree, layout);
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

/// A semantic thing to do to a node, independent of what asked for it.
///
/// Pointer, keyboard and accessibility are three *input adapters*, not three
/// implementations of interaction. Everything they can ask for is expressible
/// here, and [`Host::dispatch`] is the only place any of it happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionIntent {
    Activate(NodeKey),
    Focus(NodeKey),
    /// Clears focus, but only if this node is the one holding it. An
    /// unconditional clear would let a stale accessibility blur take focus
    /// away from an unrelated control.
    Blur(NodeKey),
    Reveal(NodeKey, instar_ui::RevealAlignment),
}

/// Which adapter asked. **Diagnostic, never semantic.**
///
/// It decides one thing — whether focus is drawn, since a mouse click should
/// not paint a keyboard ring — and is otherwise there so a test can prove all
/// three routes entered the same seam. If this ever starts changing what an
/// activation *means*, the convergence it exists to demonstrate has already
/// been lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionSource {
    Pointer,
    Keyboard,
    Accessibility,
}

/// How many times each intent has been dispatched.
///
/// The instrument F3 asserts on. Convergence cannot be inferred from the guest
/// receiving the right event — an accessibility handler that constructed
/// `ButtonActivated` itself would produce an identical guest-visible result
/// while having forked a second interaction system. Only counting entries into
/// the seam can tell those apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InteractionStats {
    pub activate: u64,
    pub focus: u64,
    pub blur: u64,
    pub reveal: u64,
}

/// Orchestrates windows, the UI layer, and the guest runtime.
#[derive(Debug)]
pub struct Host {
    windows: HashMap<WindowId, HostWindow>,
    /// The text *resource* subsystem and the guest leases onto it.
    ///
    /// Distinct from `text`, which is the shared Parley font stack: that is
    /// presentation machinery, this is documents and who may name them.
    ///
    /// Host-global, never per window. A `HostWindow` owns presentation state
    /// whose meaning is attached to a surface, and a document's lifetime has
    /// nothing to do with whether a native window exists.
    text_resources: text_host::TextHost,
    /// How many times Taffy has been entered since the last reset.
    ///
    /// Instrumentation, and the only way to state the H2 acceptance criterion
    /// honestly. The text counters cannot express it: folding alignment into
    /// `text_style_changed` runs a whole layout pass and *still* reports
    /// `rebuilt 0, relinebroken 0, realigned 1, reused 0`, because the shaping
    /// hash did not change and the width did not move. The forbidden work is
    /// invisible in the counters that measure its consequences.
    ///
    /// C5's rule, one subsystem along: a performance-invariant test must
    /// observe *entry* into the forbidden work, not merely the expensive
    /// work's cache misses.
    layout_passes: u64,
    /// Where scrollbars live relative to the content they scroll.
    ///
    /// Host policy, one choice for the whole application, and deliberately not
    /// on the wire: a guest describes *that* something scrolls, never how the
    /// chrome for it is presented. See [`instar_ui::ScrollbarStyle`].
    scrollbars: ScrollbarStyle,
    /// What the window is showing — the guest's interface, or the host's own
    /// account of why it no longer can. Not per-window: Phase 1 is one guest
    /// and one window, and a dead guest is a fact about the runtime rather
    /// than about a surface.
    presentation: PresentationState,
    scenes: SceneBuilder,
    /// Entries into the interaction seam, by intent. Diagnostics and F3.
    interaction_stats: InteractionStats,
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
            layout_passes: 0,
            scrollbars: ScrollbarStyle::default(),
            presentation: PresentationState::default(),
            interaction_stats: InteractionStats::default(),
            scenes: SceneBuilder::new(),
            text: TextContext::new(),
            text_resources: text_host::TextHost::new(),
        }
    }

    /// The text resource subsystem and its guest leases.
    pub fn text_resources(&self) -> &text_host::TextHost {
        &self.text_resources
    }

    pub fn text_resources_mut(&mut self) -> &mut text_host::TextHost {
        &mut self.text_resources
    }

    /// Resolves a commit's attachment side table, positionally.
    ///
    /// Each key resolves through
    /// [`TextHost::resolve_view_lease`](text_host::TextHost::resolve_view_lease),
    /// which checks ownership before identity — so a live view this
    /// generation does not lease, or a stale incarnation, is refused here
    /// exactly as it would be for a text operation. The result is positional
    /// scratch: slot `i` names `resolved[i]`. Duplicate entries and
    /// unreferenced entries are both legal; policing them would turn a WIT
    /// argument representation into semantics for no correctness gain.
    pub fn resolve_attachment_table(
        &self,
        generation: GenerationId,
        keys: &[OpaqueResourceKey],
    ) -> Result<Vec<TextViewId>, AttachmentRefusal> {
        keys.iter()
            .map(|key| {
                self.text_resources
                    .resolve_view_lease(generation, *key)
                    .map_err(|_| AttachmentRefusal::UnavailableTextView)
            })
            .collect()
    }

    /// Turns the decoded attachment refs into the retained map, against an
    /// already-resolved side table.
    ///
    /// Steps 4 and 5 of the frozen order, and they live here rather than in
    /// the bridge for a reason the tests make sharp: the permutation
    /// regression has to prove that *this* loop ignores slot numbers, and a
    /// test that rebuilt the map itself would prove only that its own
    /// arithmetic ignores them.
    ///
    /// Uniqueness compares resolved [`TextViewId`]s and nothing else. The same
    /// `NodeKey` may name the same view while the opaque lease representation
    /// differs purely because of how the guest supplied the borrow — WIT
    /// resource handles are opaque, carry dynamic borrow state, and are not
    /// retained equality tokens.
    pub fn resolve_attachments(
        refs: &[TextAttachmentRef],
        resolved: &[TextViewId],
    ) -> Result<BTreeMap<NodeKey, TextViewId>, AttachmentRefusal> {
        let mut attachments = BTreeMap::new();
        let mut seen = HashSet::new();
        for attachment in refs {
            let Some(view) = resolved.get(attachment.slot as usize).copied() else {
                return Err(AttachmentRefusal::AttachmentOutOfRange);
            };
            if !seen.insert(view) {
                return Err(AttachmentRefusal::TextViewAlreadyAttached);
            }
            attachments.insert(attachment.node, view);
        }
        Ok(attachments)
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

    /// Layout passes since [`Host::reset_text_stats`].
    pub fn layout_passes(&self) -> u64 {
        self.layout_passes
    }

    pub fn reset_text_stats(&mut self) {
        self.layout_passes = 0;
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
                (Some(tree), Some(layout)) => self.scenes.app_scene_focused(
                    tree,
                    layout,
                    &window.scroll,
                    metrics,
                    window.interaction.pressed(),
                    // `focus_visible` gates the ring, not `focused`: a control
                    // reached by clicking is focused without being ringed.
                    window
                        .focus
                        .focus_visible()
                        .then(|| window.focus.focused())
                        .flatten(),
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
        // Every text capability this generation held, released. Keyed by
        // generation alone: `window_id` is presentation context for the crash
        // screen below, and has no authority over resource lifetime.
        self.text_resources.release_generation(generation);

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

    /// Chooses where scrollbars sit relative to the content they scroll.
    ///
    /// Host policy: one choice for the application, not a per-viewport or
    /// per-guest setting. [`ScrollbarStyle::Inset`] narrows every viewport's
    /// content rectangle, so this is a layout change and every window is
    /// recomputed — which also means it is correct to call after a tree
    /// exists, not only at startup.
    pub fn set_scrollbar_style(&mut self, scrollbars: ScrollbarStyle) {
        if self.scrollbars == scrollbars {
            return;
        }
        self.scrollbars = scrollbars;
        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for window_id in ids {
            if let Some(window) = self.windows.get_mut(&window_id) {
                window.recompute_layout(&mut self.text, scrollbars);
                window.clamp_scroll();
            }
            self.rebuild_scene(window_id);
        }
    }

    pub fn scrollbar_style(&self) -> ScrollbarStyle {
        self.scrollbars
    }

    pub fn interaction_stats(&self) -> InteractionStats {
        self.interaction_stats
    }

    pub fn reset_interaction_stats(&mut self) {
        self.interaction_stats = InteractionStats::default();
    }

    /// The one place an interaction happens.
    ///
    /// Every adapter funnels here, so the eligibility rules, the generational
    /// checks and the guest event are written once. An adapter that shortcut
    /// this would produce the same visible outcome and a second set of rules
    /// to keep in step — which is the failure this seam exists to make
    /// impossible rather than merely discouraged.
    pub fn dispatch(
        &mut self,
        window_id: WindowId,
        intent: InteractionIntent,
        source: InteractionSource,
    ) -> Vec<HostEffect> {
        match intent {
            InteractionIntent::Activate(key) => {
                self.interaction_stats.activate += 1;
                let Some(window) = self.windows.get(&window_id) else {
                    return Vec::new();
                };
                let Some(tree) = window.tree.as_ref() else {
                    return Vec::new();
                };
                // One eligibility predicate for all three adapters. A stale
                // generational key fails here whether it arrived from a queued
                // pointer event or from an assistive technology holding an old
                // NodeId.
                if !tree
                    .find(key)
                    .is_some_and(|node| node.kind.is_interactive())
                    || !instar_ui::focusable_order(tree).contains(&key)
                {
                    return Vec::new();
                }
                vec![HostEffect::SendToGuest(
                    UiAction::ButtonActivated(key).encode(),
                )]
            }
            InteractionIntent::Focus(key) => {
                self.interaction_stats.focus += 1;
                let Some(window) = self.windows.get_mut(&window_id) else {
                    return Vec::new();
                };
                let (Some(tree), Some(layout)) = (window.tree.as_ref(), window.layout.as_ref())
                else {
                    return Vec::new();
                };
                if !instar_ui::focusable_order(tree).contains(&key) {
                    return Vec::new();
                }
                let changed = match source {
                    // A click focuses without painting a keyboard ring.
                    InteractionSource::Pointer => window.focus.focus_by_pointer(Some(key)),
                    InteractionSource::Keyboard | InteractionSource::Accessibility => {
                        window.focus.focus_by_keyboard(Some(key))
                    }
                };
                let extents = scroll_extents(tree, layout);
                instar_ui::reveal(
                    tree,
                    layout,
                    &mut window.scroll,
                    &|k| extents.get(&k).copied(),
                    key,
                    instar_ui::RevealAlignment::Nearest,
                );
                if !changed {
                    return Vec::new();
                }
                self.rebuild_scene(window_id);
                vec![HostEffect::Render { window: window_id }]
            }
            InteractionIntent::Blur(key) => {
                self.interaction_stats.blur += 1;
                let Some(window) = self.windows.get_mut(&window_id) else {
                    return Vec::new();
                };
                // Conditional on purpose. An unconditional clear would let a
                // stale blur for one control take focus away from another.
                if window.focus.focused() != Some(key) {
                    return Vec::new();
                }
                window.focus.focus_by_keyboard(None);
                self.rebuild_scene(window_id);
                vec![HostEffect::Render { window: window_id }]
            }
            InteractionIntent::Reveal(key, alignment) => {
                self.interaction_stats.reveal += 1;
                let Some(window) = self.windows.get_mut(&window_id) else {
                    return Vec::new();
                };
                let (Some(tree), Some(layout)) = (window.tree.as_ref(), window.layout.as_ref())
                else {
                    return Vec::new();
                };
                let extents = scroll_extents(tree, layout);
                let moved = instar_ui::reveal(
                    tree,
                    layout,
                    &mut window.scroll,
                    &|k| extents.get(&k).copied(),
                    key,
                    alignment,
                );
                if !moved {
                    return Vec::new();
                }
                self.rebuild_scene(window_id);
                vec![HostEffect::Render { window: window_id }]
            }
        }
    }

    /// An action requested by assistive technology.
    ///
    /// Translates and hands over. Every supported action maps onto an intent
    /// the other adapters already use; unsupported ones do nothing, which is
    /// what AccessKit requires and is better than a placeholder that pretends.
    ///
    /// Runs on the main thread. AccessKit's own handler may be called on a
    /// platform-dependent thread, so F0 will proxy requests here rather than
    /// letting a callback touch host state.
    /// Forgets what the adapter was told, so the next update is the whole
    /// tree.
    ///
    /// The platform requires this on activation: an adapter that has just
    /// attached holds nothing for an incremental update to be relative to.
    pub fn reset_accessibility(&mut self, window_id: WindowId) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.a11y.reset();
        }
    }

    /// What the platform accessibility adapter has not yet been told.
    ///
    /// `None` means there is nothing to send -- the shell must then not call
    /// the adapter at all, which is what keeps a repaint from reaching an
    /// assistive technology.
    pub fn accessibility_update(&mut self, window_id: WindowId) -> Option<accesskit::TreeUpdate> {
        let window = self.windows.get_mut(&window_id)?;
        // The metrics barrier applies here too: a projection built from
        // geometry computed for a scale that has since changed would hand an
        // assistive technology rectangles that do not describe the window.
        window.metrics.usable()?;
        let tree = window.tree.as_ref()?;
        let layout = window.layout.as_ref()?;
        let scale = window.metrics.usable()?.scale_factor;
        window
            .a11y
            .update(tree, layout, &window.focus, &window.scroll, scale)
    }

    pub fn on_accessibility_action(
        &mut self,
        window_id: WindowId,
        action: accesskit::Action,
        target: accesskit::NodeId,
    ) -> Vec<HostEffect> {
        // The whole packed id, generation included. Reverse-mapping only the
        // numeric half would let an assistive technology holding a stale
        // NodeId reach the node that replaced it -- the ABA case the
        // generation exists to prevent, bypassed at an integration boundary.
        let key = NodeKey::from_accesskit_id(target.0);
        let intent = match action {
            accesskit::Action::Click => InteractionIntent::Activate(key),
            accesskit::Action::Focus => InteractionIntent::Focus(key),
            accesskit::Action::Blur => InteractionIntent::Blur(key),
            accesskit::Action::ScrollIntoView => {
                InteractionIntent::Reveal(key, instar_ui::RevealAlignment::Nearest)
            }
            _ => return Vec::new(),
        };
        self.dispatch(window_id, intent, InteractionSource::Accessibility)
    }

    /// Routes one window event, returning what should happen as a result.
    pub fn handle(&mut self, event: WindowOutput) -> Vec<HostEffect> {
        match event {
            WindowOutput::MetricsChanged(metrics) => self.on_metrics_changed(metrics),
            WindowOutput::MetricsInvalidated { window_id } => {
                self.on_metrics_invalidated(window_id)
            }
            WindowOutput::Pointer(event) => self.on_pointer(event),
            WindowOutput::PointerMoved(event) => {
                let (x, y) = event.logical_pos.round();
                self.on_pointer_moved(event.window_id, x, y)
            }
            WindowOutput::PointerLeft { window_id } => self.on_pointer_left(window_id),
            WindowOutput::Scroll(event) => self.on_scroll(event),
            WindowOutput::Key(event) => self.on_key(event),
            WindowOutput::WindowFocusChanged { window_id, focused } => {
                self.on_window_focus_changed(window_id, focused)
            }
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
        let snapshot = DecodedUiSnapshot::decode(batch)?;
        // This entry point has no side table, so a slot here names nothing.
        // Refusing is the point: silently keeping the node and dropping the
        // attachment would retain a text surface the guest believes is showing
        // a document. The bridge path is the one that carries capabilities.
        if let Some(attachment) = snapshot.text_attachments.first() {
            return Err(TreeError::AttachmentWithoutTable {
                key: attachment.node,
                slot: attachment.slot,
            });
        }
        self.apply_tree(window_id, snapshot.tree)
    }

    /// The fallible pre-promotion half: diff the new tree against the
    /// retained one and refuse kind changes.
    ///
    /// `attachment_refs` are carried, not resolved: resolution happens before
    /// this in the bridge's normative order, and `apply_tree` stages with an
    /// empty table and therefore an empty ref list.
    fn validate_ui_commit(
        &mut self,
        window_id: WindowId,
        tree: Tree,
        attachment_refs: Vec<TextAttachmentRef>,
    ) -> Result<ValidatedUiCommit, TreeError> {
        let window = self.windows.entry(window_id).or_default();
        let tree_changes = instar_ui::diff(window.tree.as_ref(), &tree)?;
        Ok(ValidatedUiCommit {
            tree,
            tree_changes,
            attachment_refs,
        })
    }

    /// The fallible staging half: accept the snapshot's id lifecycle and
    /// compute the attachment diff.
    ///
    /// This is the last place a refusal is possible. [`StagedUiCommit`] says
    /// so, and [`HostWindow::promote_ui_commit`] takes it without a `Result`.
    fn stage_ui_commit(
        &mut self,
        window_id: WindowId,
        validated: ValidatedUiCommit,
        attachments: BTreeMap<NodeKey, TextViewId>,
    ) -> Result<StagedUiCommit, TreeError> {
        let window = self.windows.entry(window_id).or_default();
        window.ledger.validate(&validated.tree)?;
        let attachment_changes =
            attachment::AttachmentChangeSet::diff(&window.text_attachments, &attachments);
        Ok(StagedUiCommit {
            tree: validated.tree,
            tree_changes: validated.tree_changes,
            attachments,
            attachment_changes,
        })
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
        // Stage with an empty attachment table: the bridge path stages with
        // resolved attachments, and this entry point keeps its existing
        // behaviour until 3c replaces it.
        let validated = self.validate_ui_commit(window_id, tree, Vec::new())?;
        let staged = self.stage_ui_commit(window_id, validated, BTreeMap::new())?;
        Ok(self.apply_staged_commit(window_id, staged))
    }

    /// The infallible half of UI admission: everything after the last
    /// refusal, in the order `apply_tree` always used.
    ///
    /// The staged commit cannot fail, so this returns effects rather than a
    /// `Result`. The no-op early return sits here, after staging, exactly as
    /// it always sat after the diff and ledger validation: a guest
    /// re-committing an identical interface is an ordinary shape, and it
    /// should cost the decode and nothing else.
    fn apply_staged_commit(
        &mut self,
        window_id: WindowId,
        staged: StagedUiCommit,
    ) -> Vec<HostEffect> {
        let window = self.windows.entry(window_id).or_default();

        // A guest re-committing an identical interface is an ordinary shape —
        // it is what an event the guest decided to ignore looks like from
        // here. It should cost the decode and nothing else: no layout, no
        // scene, no frame.
        // (An opening commit always reports its nodes as created, so this
        // cannot swallow a guest's first interface.)
        //
        // **Both** diffs, because a commit can be a no-op for the tree and not
        // for the attachments: the same `TextView` node showing a different
        // document sends byte-identical tree bytes with a different side
        // table. Gating on the tree diff alone would drop that change on the
        // floor while telling the guest the commit was accepted.
        if staged.tree_changes.is_empty() && staged.attachment_changes.is_empty() {
            return Vec::new();
        }

        // Validation ran above the early return, so a no-op commit cannot
        // dodge the lifecycle rules. `apply` sits after it: an identical
        // snapshot has identical live keys, so applying would only redo a
        // no-op, and the no-op commit keeps costing the decode and nothing
        // else.
        let needs_layout = staged.tree_changes.needs_layout();
        let needs_text_finalize = staged.tree_changes.needs_text_finalize();
        window.ledger.apply(&staged.tree);

        // Before the new snapshot becomes interactive: any transient state
        // referring to a node the guest removed is retired. A press that
        // outlived its node would otherwise be completable against whatever
        // reused its key. See `Interaction::retire`.
        window.interaction.retire(&staged.tree_changes.removed);
        self.text.retire(&staged.tree_changes.removed);
        // Deletion destroys a viewport's offset; hiding does not. This is the
        // deletion half, and it sits with the other retirements for the same
        // reason -- state that outlives the node it describes eventually
        // lands on something else.
        window.scroll.retire(&staged.tree_changes.removed);

        // A snapshot that survived the diff is a new version of the retained
        // tree. The promotion owns the two assignments that make the retained
        // UI state -- the tree and the attachment map -- and advances the
        // revision. The guest-visible commit sequence advances on every
        // accepted commit; the revision is the host's separate value for
        // whether the state actually changed, and the one caches key off.
        window.promote_ui_commit(staged);
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
            // And a Space held on a node that commit disabled, hid, removed,
            // or replaced at a new generation.
            let focused = window.focus.focused();
            window.interaction.retire_keyboard_press(tree, focused);
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
        if needs_layout {
            self.layout_passes += 1;
            window.recompute_layout(&mut self.text, self.scrollbars);
        } else if needs_text_finalize {
            // Alignment moved and geometry did not. Finalization normally
            // happens inside a layout pass because that is where the final
            // width is known -- but every width here is still correct, so
            // running Taffy would be work with a guaranteed-identical answer.
            // The alternative to this branch is not "a bit slower"; it is
            // never applying the alignment at all.
            window.refinalize_text(&mut self.text);
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
        self.layout_passes += 1;
        window.recompute_layout(&mut self.text, self.scrollbars);
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
        let activated = match event.state {
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
                None
            }
            PointerState::Released => window
                .interaction
                .on_release(tree, layout, scroll, x, y)
                .map(|UiAction::ButtonActivated(key)| key),
        };

        // Through the seam, like every other adapter. The pointer has no
        // private route to the guest.
        let mut effects = match activated {
            Some(key) => self.dispatch(
                event.window_id,
                InteractionIntent::Activate(key),
                InteractionSource::Pointer,
            ),
            None => Vec::new(),
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
        let window = self.windows.entry(window_id).or_default();

        // Recorded before the barrier is consulted, exactly as a button event
        // records it: knowing where the pointer is costs nothing and is worth
        // keeping across a monitor switch. Acting on it is the part that has
        // to wait.
        window.last_pointer = Some(LogicalPoint {
            x: f64::from(x),
            y: f64::from(y),
        });
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

    /// The pointer left the window.
    ///
    /// A lifecycle cancellation, not an event with a position: hover is gone,
    /// a scrollbar drag cannot continue, and a pointer press cannot complete
    /// against anything. What survives is semantic focus and a keyboard Space
    /// capture -- neither depends on the pointer being over the window.
    fn on_pointer_left(&mut self, window_id: WindowId) -> Vec<HostEffect> {
        let Some(window) = self.windows.get_mut(&window_id) else {
            return Vec::new();
        };
        let hover_changed = window.scroll.set_hovered(None);
        let had_drag = window.scroll.dragging().is_some();
        window.scroll.cancel_drag();
        // `Interaction::cancel` is the existing capture-clearing primitive;
        // the source check keeps it from touching a Space press the keyboard
        // owns, which a pointer leaving the window must not cancel.
        let had_pointer_press = window
            .interaction
            .press()
            .is_some_and(|press| press.source == instar_ui::PressSource::Pointer);
        if had_pointer_press {
            window.interaction.cancel();
        }
        if !hover_changed && !had_drag && !had_pointer_press {
            return Vec::new();
        }
        self.present_lifecycle_cancellation(window_id)
    }

    /// The window gained or lost keyboard focus.
    ///
    /// Loss cancels every transient input capture made against this surface
    /// -- hover, a pointer press, a thumb drag, and a Space held on the
    /// focused control -- because winit will not deliver the releases while
    /// the window is unfocused. The focused [`NodeKey`] is retained: it is
    /// semantic state, and the keyboard may come back. Gain emits the
    /// lifecycle event but restores nothing, because held input died with the
    /// loss and must not be resurrected.
    fn on_window_focus_changed(&mut self, window_id: WindowId, focused: bool) -> Vec<HostEffect> {
        if focused {
            return Vec::new();
        }
        let Some(window) = self.windows.get_mut(&window_id) else {
            return Vec::new();
        };
        let hover_changed = window.scroll.set_hovered(None);
        let had_drag = window.scroll.dragging().is_some();
        window.scroll.cancel_drag();
        let had_press = window.interaction.pressed().is_some();
        window.interaction.cancel();
        if !hover_changed && !had_drag && !had_press {
            return Vec::new();
        }
        self.present_lifecycle_cancellation(window_id)
    }

    /// Lower the scene for a lifecycle cancellation and ask for the frame
    /// that shows it, deferring the request while the metrics barrier is open.
    fn present_lifecycle_cancellation(&mut self, window_id: WindowId) -> Vec<HostEffect> {
        self.rebuild_scene(window_id);
        let window = self.windows.entry(window_id).or_default();
        if window.metrics.is_ready() {
            vec![HostEffect::Render { window: window_id }]
        } else {
            window.redraw_pending = true;
            Vec::new()
        }
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
            return self.on_key_release(event);
        }
        let Some(window) = self.windows.get_mut(&event.window_id) else {
            return Vec::new();
        };
        if window.metrics.usable().is_none() {
            return Vec::new();
        }
        let (Some(tree), Some(layout)) = (window.tree.as_ref(), window.layout.as_ref()) else {
            return Vec::new();
        };

        let focused = window.focus.focused();
        let mut action = None;
        let changed = match event.key {
            instar_window::Key::Tab => {
                // Autorepeat is *wanted* here: holding Tab walks the form.
                let direction = if event.shift {
                    FocusMove::Previous
                } else {
                    FocusMove::Next
                };
                let moved = window.focus.traverse(tree, direction);
                // Traversal brings the newly focused control into view. The
                // guest never has to pair a focus request with a scroll, which
                // is the whole point of navigation being semantic.
                if moved && let Some(key) = window.focus.focused() {
                    let extents = scroll_extents(tree, layout);
                    instar_ui::reveal(
                        tree,
                        layout,
                        &mut window.scroll,
                        &|k| extents.get(&k).copied(),
                        key,
                        instar_ui::RevealAlignment::Nearest,
                    );
                }
                // Moving focus abandons a Space the user is still holding on
                // the control they moved away from.
                let cancelled = window
                    .interaction
                    .retire_keyboard_press(tree, window.focus.focused());
                moved || cancelled
            }
            // Activates outright, with nothing held and therefore nothing to
            // show. Autorepeat is refused: holding Enter on a button is one
            // activation, not forty.
            instar_window::Key::Enter if !event.repeat => {
                action = window.interaction.on_enter(tree, focused);
                false
            }
            // Captures, so the release completes against *this* node rather
            // than whatever is focused by then. A repeat while already held
            // changes nothing.
            instar_window::Key::Space if !event.repeat => {
                window.interaction.on_keyboard_press(tree, focused)
            }
            _ => false,
        };

        let mut effects = match action {
            Some(UiAction::ButtonActivated(key)) => self.dispatch(
                event.window_id,
                InteractionIntent::Activate(key),
                InteractionSource::Keyboard,
            ),
            None => Vec::new(),
        };
        if !changed && effects.is_empty() {
            return Vec::new();
        }
        let Some(window) = self.windows.get_mut(&event.window_id) else {
            return effects;
        };
        let _ = &window;

        // Focus and pressed state are both drawn, so either moving is a visual
        // change -- and *only* a visual one. The scene is re-lowered; layout
        // and shaping are not touched, which is the structural invariant E1
        // asserts and E2 inherits.
        self.rebuild_scene(event.window_id);
        effects.push(HostEffect::Render {
            window: event.window_id,
        });
        effects
    }

    /// A key coming up. Only Space means anything so far.
    fn on_key_release(&mut self, event: instar_window::RawKeyEvent) -> Vec<HostEffect> {
        if event.key != instar_window::Key::Space {
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
        if window.interaction.press().is_none() {
            // A release with nothing held: a Space that began before this
            // window had focus, or after its capture was retired.
            return Vec::new();
        }

        let focused = window.focus.focused();
        let action = window.interaction.on_keyboard_release(tree, focused);

        // The chrome clears whether or not it activated, because the key is up
        // either way -- and it clears now, not when the guest gets round to
        // the activation.
        self.rebuild_scene(event.window_id);
        let mut effects = match action {
            Some(UiAction::ButtonActivated(key)) => self.dispatch(
                event.window_id,
                InteractionIntent::Activate(key),
                InteractionSource::Keyboard,
            ),
            None => Vec::new(),
        };
        effects.push(HostEffect::Render {
            window: event.window_id,
        });
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
    use instar_kernel::text_bridge::{OpaqueResourceKey, TextAnswer, TextOperation, text_request};
    use instar_text::TextViewId;
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
        let (request, _reply) = commit_request(bridge.generation(), batch, Vec::new());
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
            repeat: false,
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

    // --- E3: focus presentation and semantic reveal. ---

    /// A viewport 100 tall over 400 of content, with a button below the fold.
    fn offscreen_focus_fixture() -> Tree {
        use instar_ui::{Node, WireAlign, WireLayout, WireSize};
        Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    100,
                    Node::column(
                        101,
                        vec![
                            Node::text(102, "spacer").with_layout(WireLayout {
                                height: WireSize::Fixed(300),
                                ..WireLayout::default()
                            }),
                            Node::button(103, "below the fold").with_layout(WireLayout {
                                height: WireSize::Fixed(40),
                                ..WireLayout::default()
                            }),
                            Node::text(104, "tail").with_layout(WireLayout {
                                height: WireSize::Fixed(60),
                                ..WireLayout::default()
                            }),
                        ],
                    ),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(100),
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        ))
    }

    /// Tab onto something offscreen brings it into view, without the guest.
    #[test]
    fn traversal_reveals_an_offscreen_control() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, offscreen_focus_fixture())
            .expect("valid");
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(100))
                .y,
            0,
            "nothing is scrolled yet"
        );

        let effects = host.handle(key(instar_window::Key::Tab, false));
        assert_eq!(focused(&host), Some(NodeKey::first(103)));
        assert!(
            to_guest(&effects).is_empty(),
            "reveal is host-local: {effects:?}"
        );

        let window = host.window(WINDOW).unwrap();
        let offset = window.scroll.get(NodeKey::first(100)).y;
        assert!(offset > 0, "the viewport scrolled to expose it");

        // Nearest moves the minimum: the button's bottom edge reaches the
        // viewport's bottom edge and no further.
        assert_eq!(
            offset, 240,
            "content y 300..340 in a 100-tall viewport needs exactly 240, not \
             a gratuitous centring"
        );
        assert!(
            window.focus.focus_visible(),
            "and keyboard traversal shows the ring"
        );
    }

    #[test]
    fn revealing_something_already_visible_changes_nothing() {
        use instar_ui::Node;
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        let _ = Node::text(0, "");

        host.handle(key(instar_window::Key::Tab, false));
        let effects = host.handle(key(instar_window::Key::Tab, false));
        // Focus moved, so there is a frame -- but no viewport moved, because
        // nothing needed to.
        assert!(host.window(WINDOW).unwrap().scroll.is_empty());
        assert!(to_guest(&effects).is_empty());
    }

    /// The nested case, which is the only one where recomputing between steps
    /// matters. Computing both offsets from the original geometry leaves the
    /// outer viewport working from a position the inner one has already moved.
    #[test]
    fn nested_viewports_each_adjust_using_the_updated_position() {
        use instar_ui::{Node, WireAlign, WireLayout, WireSize};
        let stretch = |height: u16| WireLayout {
            height: WireSize::Fixed(height),
            align_self: Some(WireAlign::Stretch),
            ..WireLayout::default()
        };

        // Outer viewport 100 tall; inner sits 200 down inside it and is itself
        // 80 tall over 300 of content, with the target at the bottom.
        let tree = Tree::new(Node::root(
            0,
            vec![
                Node::scroll(
                    110,
                    Node::column(
                        111,
                        vec![
                            Node::text(112, "outer spacer").with_layout(stretch(200)),
                            Node::scroll(
                                113,
                                Node::column(
                                    114,
                                    vec![
                                        Node::text(115, "inner spacer").with_layout(stretch(220)),
                                        Node::button(116, "target").with_layout(stretch(40)),
                                    ],
                                ),
                            )
                            .with_layout(stretch(80)),
                            // A long tail, so the outer viewport has far more
                            // scrollable extent than it needs. Without it the
                            // outer offset clamps to its maximum whether the
                            // arithmetic was right or not, and the test cannot
                            // tell a recomputed answer from a stale one.
                            Node::text(117, "outer tail").with_layout(stretch(500)),
                        ],
                    ),
                )
                .with_layout(stretch(100)),
            ],
        ));

        let mut host = ready_host();
        host.apply_tree(WINDOW, tree).expect("valid");
        host.handle(key(instar_window::Key::Tab, false));

        let window = host.window(WINDOW).unwrap();
        let inner = window.scroll.get(NodeKey::first(113)).y;
        let outer = window.scroll.get(NodeKey::first(110)).y;
        assert!(inner > 0, "the inner viewport scrolled to its target");
        assert!(outer > 0, "and the outer one scrolled to expose the inner");

        // The proof: the target ends up inside both viewports at once.
        let layout = window.layout().unwrap();
        let target = layout.get(NodeKey::first(116)).unwrap();
        let inner_rect = layout.get(NodeKey::first(113)).unwrap();
        let outer_rect = layout.get(NodeKey::first(110)).unwrap();

        let presented_top = target.y - inner - outer;
        let inner_top = inner_rect.y - outer;
        assert!(
            presented_top >= inner_top && presented_top < inner_top + inner_rect.height,
            "the target sits within the inner viewport once both have moved: \
             target {presented_top}, inner {inner_top}..{}",
            inner_top + inner_rect.height
        );
        assert!(
            inner_top >= outer_rect.y && inner_top < outer_rect.y + outer_rect.height,
            "and the inner viewport sits within the outer one"
        );
    }

    #[test]
    fn a_hidden_target_is_not_revealable() {
        use instar_ui::{Node, WireAlign, WireLayout, WireSize};
        let mut host = ready_host();
        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(
                0,
                vec![
                    Node::scroll(
                        120,
                        Node::column(
                            121,
                            vec![
                                Node::text(122, "spacer").with_layout(WireLayout {
                                    height: WireSize::Fixed(300),
                                    ..WireLayout::default()
                                }),
                                Node::button(123, "hidden").hidden(),
                            ],
                        ),
                    )
                    .with_layout(WireLayout {
                        height: WireSize::Fixed(100),
                        align_self: Some(WireAlign::Stretch),
                        ..WireLayout::default()
                    }),
                ],
            )),
        )
        .expect("valid");

        host.handle(key(instar_window::Key::Tab, false));
        let window = host.window(WINDOW).unwrap();
        assert_eq!(
            window.focus.focused(),
            None,
            "a hidden control is not focusable in the first place"
        );
        assert_eq!(
            window.scroll.get(NodeKey::first(120)).y,
            0,
            "and nothing scrolled looking for it"
        );
    }

    #[test]
    fn the_focus_ring_is_drawn_only_when_focus_is_visible() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        let ring = host.theme().focus_ring;
        let has_ring =
            |host: &Host| {
                host.window(WINDOW)
                    .and_then(HostWindow::scene)
                    .is_some_and(|scene| {
                        scene.commands.iter().any(|command| matches!(
                        command,
                        instar_paint::PaintCommand::StrokeRect { color, .. } if *color == ring
                    ))
                    })
            };
        assert!(!has_ring(&host), "nothing focused yet");

        host.handle(key(instar_window::Key::Tab, false));
        assert!(has_ring(&host), "keyboard traversal draws it");

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
        assert!(
            !has_ring(&host),
            "a click focuses without painting a keyboard ring"
        );
    }

    /// The keyboard equivalent of D's drag proof, for navigation.
    #[test]
    fn tab_focus_and_reveal_complete_while_the_guest_is_blocked() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, offscreen_focus_fixture())
            .expect("valid");

        let stalled_until = Instant::now() + Duration::from_millis(100);
        let effects = host.handle(key(instar_window::Key::Tab, false));

        assert!(
            Instant::now() < stalled_until,
            "focus, reveal and ring presentation all completed inside the \
             stall window, so the guest could not have participated"
        );
        assert!(to_guest(&effects).is_empty());
        let window = host.window(WINDOW).unwrap();
        assert_eq!(window.focus.focused(), Some(NodeKey::first(103)));
        assert!(window.focus.focus_visible());
        assert!(window.scroll.get(NodeKey::first(100)).y > 0);
    }

    /// The Gallery's tree, in host terms, reduced to what F4 exercises.
    ///
    /// Kept in step with `guests/gallery` by hand. It is worth the
    /// duplication: F4 is a manual session against a real screen reader, and
    /// this is what makes a failure there point at the platform boundary
    /// rather than at the fixture.
    fn gallery_fixture() -> Tree {
        use instar_ui::{Node, WireAlign, WireLayout, WireSize};
        Tree::new(Node::root(
            0,
            vec![
                Node::text(1, "pointer 0"),
                Node::scroll(
                    10,
                    Node::column(
                        11,
                        vec![
                            Node::button(12, "Pointer target"),
                            Node::button(13, "Disabled control").disabled(),
                            Node::text(14, "outer overflow").with_layout(WireLayout {
                                height: WireSize::Fixed(600),
                                ..WireLayout::default()
                            }),
                            Node::button(15, "Offscreen target"),
                        ],
                    ),
                )
                .with_layout(WireLayout {
                    height: WireSize::Fixed(200),
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                }),
            ],
        ))
    }

    /// The F4 fixture is shaped the way the manual procedure assumes.
    ///
    /// Everything a screen reader will be asked to do is reachable here
    /// without one: the disabled button is present and marked, the offscreen
    /// button starts outside the viewport, focusing it reveals it, and
    /// activating it through the accessibility source produces an
    /// accessibility-observable change. If this test is green and the manual
    /// session still fails, the failure is at the native boundary.
    #[test]
    fn the_gallery_exercises_what_the_manual_accessibility_pass_will_ask_of_it() {
        // A bare host, not `ready_host`: the guest's first tree *is* this
        // fixture, and the node ids below are the guest's own.
        let mut host = Host::new();
        host.handle(WindowOutput::MetricsChanged(metrics(1.0)));
        host.apply_tree(WINDOW, gallery_fixture()).expect("valid");

        let update = host.accessibility_update(WINDOW).expect("a tree");
        let find = |key: NodeKey| {
            update
                .nodes
                .iter()
                .find(|(id, _)| *id == ak(key))
                .map(|(_, node)| node.clone())
                .unwrap_or_else(|| panic!("{key:?} is missing from the projection"))
        };

        assert!(
            find(NodeKey::first(13)).is_disabled(),
            "the disabled control must be announced as unavailable, not omitted"
        );
        assert!(!find(NodeKey::first(12)).is_disabled());
        assert!(
            !find(NodeKey::first(15)).is_disabled(),
            "the offscreen control is enabled -- it is merely out of view"
        );

        // Out of view to begin with, or the reveal step proves nothing.
        let offscreen = NodeKey::first(15);
        let scroll = NodeKey::first(10);
        assert_eq!(
            host.window(WINDOW).unwrap().scroll.get(scroll).y,
            0,
            "nothing is scrolled yet"
        );

        // Focus reveals it. This is the E3 path, reached from the keyboard
        // source; the manual pass reaches it from the accessibility source.
        host.dispatch(
            WINDOW,
            InteractionIntent::Focus(offscreen),
            InteractionSource::Accessibility,
        );
        assert_eq!(focused(&host), Some(offscreen));
        assert!(
            host.window(WINDOW).unwrap().scroll.get(scroll).y > 0,
            "focusing the offscreen button must scroll it into view -- if this \
             is zero the spacer is not pushing it out of the viewport, and \
             the manual pass would prove nothing"
        );

        // And activating it through the accessibility seam reaches the guest.
        host.reset_interaction_stats();
        let effects = host.on_accessibility_action(WINDOW, accesskit::Action::Click, ak(offscreen));
        assert_eq!(host.interaction_stats().activate, 1);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, HostEffect::SendToGuest(_))),
            "activation must reach the guest, or the readout never changes and \
             there is nothing for a screen reader to observe"
        );
    }

    // --- H3: basis is geometry, and must be treated as such. ---

    fn keypad_row(basis: instar_ui::WireBasis) -> Tree {
        use instar_ui::{Node, WireLayout, WireSize};
        let key = |id: u32, label: &str| {
            Node::button(id, label).with_layout(WireLayout {
                basis,
                grow: 1.0,
                min_width: Some(0),
                ..WireLayout::default()
            })
        };
        Tree::new(Node::root(
            0,
            vec![
                Node::row(80, vec![key(81, "0"), key(82, "000000000000")]).with_layout(
                    WireLayout {
                        width: WireSize::Fixed(120),
                        ..WireLayout::default()
                    },
                ),
            ],
        ))
    }

    /// The inverse of H2's control, and the reason it is worth having.
    ///
    /// Several packages have proved that particular changes *avoid* layout.
    /// This is the complement: a genuine geometry property must not end up
    /// categorized as paint-only, which would leave the new sizes computed and
    /// never applied.
    #[test]
    fn a_basis_change_enters_layout_and_moves_the_rectangles() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, keypad_row(instar_ui::WireBasis::Auto))
            .expect("valid");
        let width = |host: &Host, id: u32| {
            host.window(WINDOW)
                .and_then(HostWindow::layout)
                .and_then(|layout| layout.get(NodeKey::first(id)))
                .expect("laid out")
                .width
        };
        assert_ne!(
            width(&host, 81),
            width(&host, 82),
            "with Auto the labels decide, so the two differ to begin with"
        );

        host.reset_text_stats();
        host.apply_tree(WINDOW, keypad_row(instar_ui::WireBasis::Fixed(0)))
            .expect("valid");

        assert_eq!(
            host.layout_passes(),
            1,
            "basis is geometry: it has to enter Taffy, and a category that \
             skipped layout would compute nothing and change nothing"
        );
        assert_eq!(
            (width(&host, 81), width(&host, 82)),
            (60, 60),
            "and the rectangles actually move"
        );
    }

    // --- H2: alignment is positioning, not shaping. ---

    fn aligned_tree(align: instar_ui::WireTextAlign) -> Tree {
        use instar_ui::{Node, WireAlign, WireLayout, WireStyle, WireTextLayout};
        Tree::new(Node::root(
            0,
            vec![
                Node::text(90, "a readout")
                    .with_layout(WireLayout {
                        align_self: Some(WireAlign::Stretch),
                        ..WireLayout::default()
                    })
                    .with_style(WireStyle {
                        text_layout: WireTextLayout { align },
                        ..WireStyle::default()
                    }),
            ],
        ))
    }

    /// The acceptance criterion for the whole category:
    ///
    /// > An alignment-only update must reach pixels without entering either
    /// > Taffy or shaping.
    ///
    /// `reused == 0` is in here for the reason C5 taught: without it, the test
    /// passes if the change accidentally enters a broader finalization path
    /// and merely gets a cheap cache hit on the way through.
    #[test]
    fn an_alignment_only_change_realigns_and_nothing_else() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, aligned_tree(instar_ui::WireTextAlign::Start))
            .expect("valid");
        let before = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|layout| layout.get(NodeKey::first(90)))
            .expect("the readout is laid out");

        host.reset_text_stats();
        host.apply_tree(WINDOW, aligned_tree(instar_ui::WireTextAlign::End))
            .expect("valid");
        let stats = host.text_stats();

        assert_eq!(
            (
                stats.rebuilt,
                stats.relinebroken,
                stats.realigned,
                stats.reused
            ),
            (0, 0, 1, 0),
            "alignment realigns and extracts; it must not reshape, re-break, \
             or slip through a path that merely hits the cache"
        );
        assert_eq!(stats.extracted, 1, "the artifact has to be produced again");
        assert_eq!(
            host.layout_passes(),
            0,
            "and Taffy was never entered. The text counters cannot say this on \
             their own -- folding alignment into `text_style_changed` runs a \
             full layout pass and still reports the same four numbers, because \
             the shaping hash is unchanged and the width did not move. Entry \
             into the forbidden work has to be observed directly."
        );
        assert_eq!(
            host.window(WINDOW)
                .and_then(HostWindow::layout)
                .and_then(|layout| layout.get(NodeKey::first(90))),
            Some(before),
            "and no rectangle moved, which is why Taffy had nothing to do"
        );
    }

    /// Committing the same alignment again is not work.
    #[test]
    fn an_unchanged_alignment_does_nothing_at_all() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, aligned_tree(instar_ui::WireTextAlign::End))
            .expect("valid");

        host.reset_text_stats();
        host.apply_tree(WINDOW, aligned_tree(instar_ui::WireTextAlign::End))
            .expect("valid");
        let stats = host.text_stats();
        assert_eq!(
            (
                stats.rebuilt,
                stats.relinebroken,
                stats.realigned,
                stats.extracted
            ),
            (0, 0, 0, 0),
            "an identical commit is a no-op, alignment included"
        );
    }

    /// A width change re-breaks *and* re-aligns.
    ///
    /// The bug this exists for is subtle: `End` looks right when first
    /// applied, and then stays positioned against the old line width after a
    /// resize. A counter that only tracked "the alignment property changed"
    /// would report zero here and describe the wire rather than the machine.
    #[test]
    fn a_width_change_reapplies_alignment_to_the_new_lines() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, aligned_tree(instar_ui::WireTextAlign::End))
            .expect("valid");

        host.reset_text_stats();
        host.handle(WindowOutput::MetricsChanged(WindowMetricsChanged {
            window_id: WINDOW,
            logical_size: LogicalSize {
                width: 260.0,
                height: 300.0,
            },
            physical_size: PhysicalSize {
                width: 260,
                height: 300,
            },
            scale_factor: 1.0,
        }));

        let stats = host.text_stats();
        assert_eq!(stats.rebuilt, 0, "a resize does not reshape");
        assert_eq!(stats.relinebroken, 1, "it re-breaks");
        assert_eq!(
            stats.realigned, 1,
            "and must re-apply alignment to the lines it just made, or End \
             stays positioned against the width that is gone"
        );
    }

    // --- F0: the transport seam, minus the platform adapter. ---

    fn host_for_actions() -> Host {
        let mut host = ready_host();
        host.apply_tree(WINDOW, offscreen_focus_fixture())
            .expect("valid");
        host
    }

    /// Nothing is lost or altered carrying an action across the boundary.
    ///
    /// F3 proved the semantics; this proves the transport. Every action the
    /// shell forwards enters the intent it should and no other, and the two
    /// kinds of request that must produce nothing produce nothing.
    #[test]
    fn every_forwarded_action_reaches_its_intent_and_no_other() {
        let target = ak(NodeKey::first(103));

        for (action, want) in [
            (accesskit::Action::Click, (1, 0, 0, 0)),
            (accesskit::Action::Focus, (0, 1, 0, 0)),
            (accesskit::Action::ScrollIntoView, (0, 0, 0, 1)),
        ] {
            let mut host = host_for_actions();
            host.reset_interaction_stats();
            host.on_accessibility_action(WINDOW, action, target);
            let s = host.interaction_stats();
            assert_eq!(
                (s.activate, s.focus, s.blur, s.reveal),
                want,
                "{action:?} entered the wrong intent"
            );
        }

        // Blur needs focus on the target first, or it is correctly a no-op --
        // so the conditionality is part of what transport must not disturb.
        let mut host = host_for_actions();
        host.dispatch(
            WINDOW,
            InteractionIntent::Focus(NodeKey::first(103)),
            InteractionSource::Accessibility,
        );
        host.reset_interaction_stats();
        host.on_accessibility_action(WINDOW, accesskit::Action::Blur, target);
        assert_eq!(host.interaction_stats().blur, 1);
        assert_eq!(focused(&host), None, "blur on the focused node clears it");

        // An action Instar does not implement dies at the boundary rather
        // than falling through to whatever the match arm below it happens
        // to be.
        let mut host = host_for_actions();
        host.reset_interaction_stats();
        host.on_accessibility_action(WINDOW, accesskit::Action::SetValue, target);
        assert_eq!(
            host.interaction_stats(),
            InteractionStats::default(),
            "an unsupported action is ignored, not half-handled"
        );

        // A stale id reaches the seam -- eligibility is decided there, not in
        // transport -- and is then refused, which is the ABA case the
        // generation exists to prevent.
        let mut host = host_for_actions();
        host.on_accessibility_action(
            WINDOW,
            accesskit::Action::Focus,
            accesskit::NodeId(NodeKey::new(103, 99).to_accesskit_id()),
        );
        assert_eq!(
            focused(&host),
            None,
            "a superseded generation must not reach the node that replaced it"
        );
    }

    /// The reverse direction: the host offers an update exactly when there is
    /// one, so the shell knows when *not* to call the adapter.
    #[test]
    fn the_host_offers_an_accessibility_update_only_when_something_changed() {
        use instar_ui::{Node, WireColor};
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");

        assert!(
            host.accessibility_update(WINDOW).is_some(),
            "the first call is the whole tree"
        );
        assert!(
            host.accessibility_update(WINDOW).is_none(),
            "and calling again with nothing changed offers nothing"
        );

        // Paint-only, carried all the way out to the boundary.
        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(
                0,
                vec![
                    Node::text(90, "label"),
                    Node::button(91, "first").with_foreground(WireColor::opaque(255, 0, 0)),
                    Node::button(92, "second"),
                ],
            )),
        )
        .expect("valid");
        assert!(
            host.accessibility_update(WINDOW).is_none(),
            "a recolour must not reach the platform adapter at all"
        );

        // Focus is accessibility-observable, so it must.
        host.handle(key(instar_window::Key::Tab, false));
        let update = host
            .accessibility_update(WINDOW)
            .expect("focus moved, so the adapter must hear about it");
        assert_eq!(update.focus, ak(NodeKey::first(91)));
        assert!(
            host.accessibility_update(WINDOW).is_none(),
            "and exactly once -- the seam is not called twice for one change"
        );
    }

    /// While the metrics barrier is up there is no coherent geometry, so
    /// there is nothing honest to tell an assistive technology.
    ///
    /// The pending change is banked *before* the barrier goes up. Invalidate
    /// first and the input that would have changed anything is refused
    /// upstream, and the test passes whether or not the barrier is checked
    /// here at all.
    #[test]
    fn no_accessibility_update_is_offered_while_geometry_is_invalid() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        host.accessibility_update(WINDOW).expect("the initial tree");

        host.handle(key(instar_window::Key::Tab, false));
        host.handle(WindowOutput::MetricsInvalidated { window_id: WINDOW });

        assert!(
            host.accessibility_update(WINDOW).is_none(),
            "rectangles computed for a window that has since changed size or \
             scale are worse than none"
        );
    }

    /// An assistive technology that attaches after the tree has been sitting
    /// there must be given the whole thing, not a diff against nothing.
    #[test]
    fn activation_resends_the_whole_tree_even_though_nothing_changed() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        let first = host.accessibility_update(WINDOW).expect("the initial tree");
        assert!(!first.nodes.is_empty());
        assert!(host.accessibility_update(WINDOW).is_none(), "drained");

        // Nothing about the interface has changed -- only who is listening.
        host.reset_accessibility(WINDOW);
        let again = host
            .accessibility_update(WINDOW)
            .expect("a newly attached adapter holds nothing to diff against");
        assert_eq!(
            again.nodes.len(),
            first.nodes.len(),
            "the second listener is told exactly as much as the first"
        );
        assert!(
            again.tree.is_some(),
            "including the tree declaration itself"
        );
    }

    // --- F3: three adapters, one interaction system. ---

    fn ak(key: NodeKey) -> accesskit::NodeId {
        accesskit::NodeId(key.to_accesskit_id())
    }

    /// All three adapters reach the same seam, and are counted doing it.
    ///
    /// Convergence cannot be inferred from the guest receiving the right
    /// event: an accessibility handler that built `ButtonActivated` itself
    /// would produce an identical guest-visible result. Only counting entries
    /// into `dispatch` distinguishes one interaction system from three.
    #[test]
    fn pointer_keyboard_and_accessibility_all_enter_the_same_seam() {
        let expected = vec![UiAction::ButtonActivated(NodeKey::first(91)).encode()];

        // Pointer.
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        let rect = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey::first(91)))
            .unwrap();
        let (x, y) = (f64::from(rect.x + 1), f64::from(rect.y + 1));
        host.reset_interaction_stats();
        host.handle(pointer(PointerState::Pressed, x, y));
        let clicked = host.handle(pointer(PointerState::Released, x, y));
        assert_eq!(activations(&clicked), expected);
        assert_eq!(
            host.interaction_stats().activate,
            1,
            "the pointer entered the seam exactly once"
        );

        // Keyboard.
        let mut host = keyboard_host();
        host.reset_interaction_stats();
        let pressed = host.handle(key_event(instar_window::Key::Enter, true, false));
        assert_eq!(activations(&pressed), expected);
        assert_eq!(host.interaction_stats().activate, 1);

        // Accessibility.
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        host.reset_interaction_stats();
        let invoked =
            host.on_accessibility_action(WINDOW, accesskit::Action::Click, ak(NodeKey::first(91)));
        assert_eq!(
            activations(&invoked),
            expected,
            "assistive technology produces the same event as a click"
        );
        assert_eq!(
            host.interaction_stats().activate,
            1,
            "and reached it by the same route"
        );
    }

    #[test]
    fn an_accessibility_focus_uses_the_same_focus_machinery() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, offscreen_focus_fixture())
            .expect("valid");
        host.reset_interaction_stats();

        let effects =
            host.on_accessibility_action(WINDOW, accesskit::Action::Focus, ak(NodeKey::first(103)));

        assert_eq!(host.interaction_stats().focus, 1, "through the seam");
        assert!(to_guest(&effects).is_empty(), "focus is not a guest event");

        let window = host.window(WINDOW).unwrap();
        assert_eq!(window.focus.focused(), Some(NodeKey::first(103)));
        assert!(
            window.focus.focus_visible(),
            "accessibility focus is deliberate, so it draws"
        );
        assert!(
            window.scroll.get(NodeKey::first(100)).y > 0,
            "and it reveals, using the same nested walk Tab does"
        );
    }

    #[test]
    fn an_accessibility_blur_only_clears_its_own_target() {
        let mut host = keyboard_host();
        assert_eq!(focused(&host), Some(NodeKey::first(91)));

        host.on_accessibility_action(WINDOW, accesskit::Action::Blur, ak(NodeKey::first(92)));
        assert_eq!(
            focused(&host),
            Some(NodeKey::first(91)),
            "a blur naming a different control must not take focus from this one"
        );

        host.on_accessibility_action(WINDOW, accesskit::Action::Blur, ak(NodeKey::first(91)));
        assert_eq!(focused(&host), None, "its own target does clear");
    }

    #[test]
    fn scroll_into_view_routes_into_the_reveal_primitive() {
        let mut host = ready_host();
        host.apply_tree(WINDOW, offscreen_focus_fixture())
            .expect("valid");
        host.reset_interaction_stats();

        host.on_accessibility_action(
            WINDOW,
            accesskit::Action::ScrollIntoView,
            ak(NodeKey::first(103)),
        );

        assert_eq!(host.interaction_stats().reveal, 1);
        assert_eq!(
            host.window(WINDOW)
                .unwrap()
                .scroll
                .get(NodeKey::first(100))
                .y,
            240,
            "the same minimum movement Tab produces -- not a second \
             nested-scroll implementation living in the accessibility layer"
        );
    }

    /// A stale NodeId from assistive technology must not reach the node that
    /// replaced it. The generation is in the packed id, and the reverse
    /// mapping must not discard it.
    #[test]
    fn a_stale_accessibility_node_id_activates_nothing() {
        use instar_ui::{Node, NodeKind, WireLayout};
        let with_generation = |generation: u32| {
            Tree::new(Node::root(
                0,
                vec![Node {
                    key: NodeKey::new(95, generation),
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
        // Gone, then back at a new generation.
        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(0, vec![Node::text(96, "gap")])),
        )
        .expect("valid");
        host.apply_tree(WINDOW, with_generation(1)).expect("valid");

        let stale =
            host.on_accessibility_action(WINDOW, accesskit::Action::Click, ak(NodeKey::new(95, 0)));
        assert!(
            activations(&stale).is_empty(),
            "an assistive technology holding the old NodeId must not reach \
             the button that replaced it"
        );

        let current =
            host.on_accessibility_action(WINDOW, accesskit::Action::Click, ak(NodeKey::new(95, 1)));
        assert_eq!(
            activations(&current).len(),
            1,
            "the current generation still works"
        );
    }

    #[test]
    fn an_unsupported_accessibility_action_does_nothing() {
        let mut host = keyboard_host();
        host.reset_interaction_stats();
        let effects = host.on_accessibility_action(
            WINDOW,
            accesskit::Action::SetValue,
            ak(NodeKey::first(91)),
        );
        assert!(effects.is_empty());
        assert_eq!(
            host.interaction_stats(),
            InteractionStats::default(),
            "an action Instar cannot honour is ignored rather than \
             half-implemented"
        );
    }

    #[test]
    fn a_disabled_button_refuses_every_adapter_equally() {
        use instar_ui::Node;
        let mut host = ready_host();
        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(0, vec![Node::button(97, "off").disabled()])),
        )
        .expect("valid");

        let clicked =
            host.on_accessibility_action(WINDOW, accesskit::Action::Click, ak(NodeKey::first(97)));
        assert!(
            activations(&clicked).is_empty(),
            "the seam's eligibility check is the same one the pointer meets"
        );
    }

    // --- E2: keyboard activation. ---

    fn key_event(k: instar_window::Key, pressed: bool, repeat: bool) -> WindowOutput {
        WindowOutput::Key(instar_window::RawKeyEvent {
            window_id: WINDOW,
            key: k,
            pressed,
            shift: false,
            repeat,
        })
    }

    fn activations(effects: &[HostEffect]) -> Vec<Vec<u8>> {
        to_guest(effects).into_iter().cloned().collect()
    }

    /// Focus already on button 91.
    fn keyboard_host() -> Host {
        let mut host = ready_host();
        host.apply_tree(WINDOW, focus_fixture()).expect("valid");
        host.handle(key(instar_window::Key::Tab, false));
        host
    }

    #[test]
    fn enter_activates_the_focused_button_exactly_once() {
        let mut host = keyboard_host();
        let effects = host.handle(key_event(instar_window::Key::Enter, true, false));
        assert_eq!(
            activations(&effects),
            vec![UiAction::ButtonActivated(NodeKey::first(91)).encode()],
            "Enter produces the same semantic event a click does"
        );
    }

    #[test]
    fn enter_autorepeat_does_not_multiply_activation() {
        let mut host = keyboard_host();
        host.handle(key_event(instar_window::Key::Enter, true, false));
        let repeated = host.handle(key_event(instar_window::Key::Enter, true, true));
        assert!(
            activations(&repeated).is_empty(),
            "holding Enter on a button is one activation, not forty"
        );
    }

    #[test]
    fn space_presses_on_the_way_down_and_activates_on_the_way_up() {
        let mut host = keyboard_host();

        let down = host.handle(key_event(instar_window::Key::Space, true, false));
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::first(91)),
            "the button is held"
        );
        assert!(
            activations(&down).is_empty(),
            "and the guest hears nothing yet"
        );
        assert!(
            down.iter()
                .any(|effect| matches!(effect, HostEffect::Render { .. })),
            "pressed chrome is drawn immediately"
        );

        let up = host.handle(key_event(instar_window::Key::Space, false, false));
        assert_eq!(
            activations(&up),
            vec![UiAction::ButtonActivated(NodeKey::first(91)).encode()]
        );
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            None,
            "and the hold is released"
        );
    }

    #[test]
    fn space_repeat_while_held_changes_nothing() {
        let mut host = keyboard_host();
        host.handle(key_event(instar_window::Key::Space, true, false));
        let repeated = host.handle(key_event(instar_window::Key::Space, true, true));
        assert!(
            repeated.is_empty(),
            "a repeat of an already-held Space is not a new press and not a \
             frame: {repeated:?}"
        );
    }

    #[test]
    fn space_up_without_space_down_does_nothing() {
        let mut host = keyboard_host();
        let up = host.handle(key_event(instar_window::Key::Space, false, false));
        assert!(up.is_empty(), "nothing was held, so nothing is released");
    }

    /// Space captures, exactly as a pointer press does. Release must not
    /// activate whatever happens to be focused by then.
    #[test]
    fn moving_focus_between_space_down_and_up_activates_nothing() {
        let mut host = keyboard_host();
        host.handle(key_event(instar_window::Key::Space, true, false));
        host.handle(key(instar_window::Key::Tab, false));
        assert_eq!(focused(&host), Some(NodeKey::first(92)));
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            None,
            "moving away abandons the hold"
        );

        let up = host.handle(key_event(instar_window::Key::Space, false, false));
        assert!(
            activations(&up).is_empty(),
            "neither the button that was held nor the one now focused activates"
        );
    }

    #[test]
    fn disabling_the_held_button_before_release_activates_nothing() {
        use instar_ui::Node;
        let mut host = keyboard_host();
        host.handle(key_event(instar_window::Key::Space, true, false));

        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(
                0,
                vec![
                    Node::text(90, "label"),
                    Node::button(91, "first").disabled(),
                    Node::button(92, "second"),
                ],
            )),
        )
        .expect("valid");

        let up = host.handle(key_event(instar_window::Key::Space, false, false));
        assert!(activations(&up).is_empty());
    }

    /// The generational case, for the keyboard capture rather than for focus.
    #[test]
    fn a_reused_id_cannot_be_activated_by_a_space_held_on_its_predecessor() {
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
        host.handle(key_event(instar_window::Key::Space, true, false));
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::new(93, 0))
        );

        host.apply_tree(
            WINDOW,
            Tree::new(Node::root(0, vec![Node::text(94, "gap")])),
        )
        .expect("valid");
        host.apply_tree(WINDOW, with_generation(1)).expect("valid");

        let up = host.handle(key_event(instar_window::Key::Space, false, false));
        assert!(
            activations(&up).is_empty(),
            "generation 1 never activates from a Space held on generation 0"
        );
    }

    /// One capture slot shared by two input paths would let each complete the
    /// other's press.
    #[test]
    fn pointer_and_keyboard_presses_cannot_complete_each_other() {
        let mut host = keyboard_host();
        let rect = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|l| l.get(NodeKey::first(91)))
            .unwrap();
        let (x, y) = (f64::from(rect.x + 1), f64::from(rect.y + 1));

        // Space down, then a pointer release over the same button.
        host.handle(key_event(instar_window::Key::Space, true, false));
        let released = host.handle(pointer(PointerState::Released, x, y));
        assert!(
            activations(&released).is_empty(),
            "a pointer release must not complete a press the keyboard started"
        );
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::first(91)),
            "and must not steal the capture either"
        );

        // The other direction.
        let mut host = keyboard_host();
        host.handle(pointer(PointerState::Pressed, x, y));
        let up = host.handle(key_event(instar_window::Key::Space, false, false));
        assert!(
            activations(&up).is_empty(),
            "a Space release must not complete a press the mouse started"
        );
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::first(91)),
            "the pointer still holds it"
        );
    }

    /// The keyboard counterpart of D's drag test, splitting the two properties
    /// that have different answers.
    #[test]
    fn a_held_space_paints_immediately_while_the_guest_is_blocked() {
        let mut host = keyboard_host();
        let stalled_until = Instant::now() + Duration::from_millis(100);

        let down = host.handle(key_event(instar_window::Key::Space, true, false));
        assert!(
            Instant::now() < stalled_until,
            "the press was handled inside the stall window, so the guest \
             could not have participated"
        );
        assert!(
            activations(&down).is_empty(),
            "interaction feedback does not consult the guest"
        );

        // Property one: presentation changed, now.
        let scene = host.window(WINDOW).and_then(HostWindow::scene).unwrap();
        let pressed_face = host.theme().pressed_face;
        assert!(
            scene.commands.iter().any(|command| matches!(
                command,
                instar_paint::PaintCommand::FillRect { color, .. } if *color == pressed_face
            )),
            "the button is drawn pressed before the guest could run"
        );

        // Property two: the consequence is queued, and is allowed to wait.
        let up = host.handle(key_event(instar_window::Key::Space, false, false));
        assert!(Instant::now() < stalled_until, "and so was the release");
        assert_eq!(
            activations(&up),
            vec![UiAction::ButtonActivated(NodeKey::first(91)).encode()],
            "exactly one activation, carrying the captured generational key"
        );
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            None,
            "and the chrome cleared without waiting for the guest to act on it"
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

    fn moved(x: f64, y: f64) -> WindowOutput {
        WindowOutput::PointerMoved(instar_window::RawPointerMoved {
            window_id: WINDOW,
            logical_pos: LogicalPoint::new(x, y),
        })
    }

    /// The whole drag, driven only through `WindowOutput`.
    ///
    /// Every other drag test calls `on_pointer_moved` directly, which is how
    /// the arithmetic was proved correct while the thumb still could not be
    /// dragged in the running application: nothing translated a cursor move
    /// into that call. This one goes through `handle`, so it fails if the
    /// vocabulary loses the term again.
    #[test]
    fn a_thumb_drag_works_through_the_event_vocabulary_alone() {
        let mut host = scrolled_host();
        let bar = bar_of(&host);
        let viewport = NodeKey::first(50);
        let start_y = f64::from(bar.thumb.y + 2);

        assert_eq!(host.window(WINDOW).unwrap().scroll.get(viewport).y, 0);

        host.handle(pointer(
            PointerState::Pressed,
            f64::from(bar.thumb.x + 2),
            start_y,
        ));
        assert!(
            host.window(WINDOW).unwrap().scroll.dragging().is_some(),
            "the press must start a drag, or there is nothing to continue"
        );

        let effects = host.handle(moved(f64::from(bar.thumb.x + 2), start_y + 30.0));
        let offset = host.window(WINDOW).unwrap().scroll.get(viewport).y;
        assert!(
            offset > 0,
            "dragging the thumb down must scroll the content: offset is still \
             {offset}"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, HostEffect::Render { .. })),
            "and ask for the frame that shows it"
        );
        assert!(
            to_guest(&effects).is_empty(),
            "while telling the guest nothing -- a drag is presentation"
        );

        // And it keeps tracking, rather than moving once and sticking.
        host.handle(moved(f64::from(bar.thumb.x + 2), start_y + 60.0));
        assert!(
            host.window(WINDOW).unwrap().scroll.get(viewport).y > offset,
            "a continued drag keeps scrolling"
        );

        host.handle(pointer(
            PointerState::Released,
            f64::from(bar.thumb.x + 2),
            start_y + 60.0,
        ));
        assert!(
            host.window(WINDOW).unwrap().scroll.dragging().is_none(),
            "and the release ends it"
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

    // --- HARDEN-1: window lifecycle cancellation. ---

    fn focus_lost() -> WindowOutput {
        WindowOutput::WindowFocusChanged {
            window_id: WINDOW,
            focused: false,
        }
    }

    fn focus_regained() -> WindowOutput {
        WindowOutput::WindowFocusChanged {
            window_id: WINDOW,
            focused: true,
        }
    }

    #[test]
    fn pointer_left_cancels_a_pointer_press_but_keeps_focus() {
        let mut host = keyboard_host();
        let rect = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|layout| layout.get(NodeKey::first(91)))
            .unwrap();
        let (x, y) = (f64::from(rect.x + 1), f64::from(rect.y + 1));
        host.handle(pointer(PointerState::Pressed, x, y));
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::first(91))
        );

        let effects = host.handle(WindowOutput::PointerLeft { window_id: WINDOW });
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            None,
            "a press the pointer started cannot outlive the pointer"
        );
        assert_eq!(
            focused(&host),
            Some(NodeKey::first(91)),
            "semantic focus is not pointer capture and survives a CursorLeft"
        );
        assert!(
            effects.contains(&HostEffect::Render { window: WINDOW }),
            "clearing the pressed look is a visible change and asks for a frame"
        );

        assert!(
            activations(&host.handle(pointer(PointerState::Released, x, y))).is_empty(),
            "the later release cannot complete a press the pointer no longer owns"
        );
    }

    #[test]
    fn pointer_left_keeps_a_keyboard_space_capture_and_requests_no_frame() {
        let mut host = keyboard_host();
        host.handle(key_event(instar_window::Key::Space, true, false));
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::first(91))
        );

        let effects = host.handle(WindowOutput::PointerLeft { window_id: WINDOW });
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::first(91)),
            "a Space capture belongs to focus, not to the pointer"
        );
        assert_eq!(focused(&host), Some(NodeKey::first(91)));
        assert!(
            effects.is_empty(),
            "nothing visible changed, so a CursorLeft must not ask for a \
             frame: {effects:?}"
        );
    }

    #[test]
    fn pointer_left_cancels_a_scrollbar_drag() {
        let mut host = scrolled_host();
        let bar = bar_of(&host);
        let viewport = NodeKey::first(50);
        let x = f64::from(bar.thumb.x + 2);
        let start_y = f64::from(bar.thumb.y + 2);

        host.handle(pointer(PointerState::Pressed, x, start_y));
        host.handle(moved(x, start_y + 30.0));
        let offset = host.window(WINDOW).unwrap().scroll.get(viewport).y;
        assert!(offset > 0, "the drag is live before the pointer leaves");

        let effects = host.handle(WindowOutput::PointerLeft { window_id: WINDOW });
        assert!(
            host.window(WINDOW).unwrap().scroll.dragging().is_none(),
            "a drag whose pointer left the window cannot continue"
        );
        assert!(
            effects.contains(&HostEffect::Render { window: WINDOW }),
            "releasing the thumb's held look is a visible change"
        );

        host.handle(moved(-500.0, -500.0));
        assert_eq!(
            host.window(WINDOW).unwrap().scroll.get(viewport).y,
            offset,
            "a later move must not resume the cancelled drag"
        );
    }

    #[test]
    fn pointer_left_clears_scrollbar_hover_and_a_noop_requests_no_frame() {
        let mut host = scrolled_host();
        let bar = bar_of(&host);
        host.on_pointer_moved(WINDOW, bar.thumb.x + 2, bar.thumb.y + 2);
        assert!(
            host.window(WINDOW).unwrap().scroll.hovered().is_some(),
            "hover is present before the pointer leaves"
        );

        let effects = host.handle(WindowOutput::PointerLeft { window_id: WINDOW });
        assert_eq!(
            host.window(WINDOW).unwrap().scroll.hovered(),
            None,
            "hover presentation cannot survive the pointer leaving"
        );
        assert!(
            effects.contains(&HostEffect::Render { window: WINDOW }),
            "removing hover is a visible change and asks for a frame"
        );

        assert!(
            host.handle(WindowOutput::PointerLeft { window_id: WINDOW })
                .is_empty(),
            "a second PointerLeft changes nothing visible, so no frame"
        );
    }

    #[test]
    fn focus_loss_cancels_a_pointer_press_and_the_release_cannot_activate() {
        let mut host = keyboard_host();
        let rect = host
            .window(WINDOW)
            .and_then(HostWindow::layout)
            .and_then(|layout| layout.get(NodeKey::first(91)))
            .unwrap();
        let (x, y) = (f64::from(rect.x + 1), f64::from(rect.y + 1));
        host.handle(pointer(PointerState::Pressed, x, y));

        let effects = host.handle(focus_lost());
        assert_eq!(host.window(WINDOW).unwrap().interaction.pressed(), None);
        assert_eq!(
            focused(&host),
            Some(NodeKey::first(91)),
            "focus loss retains the focused NodeKey"
        );
        assert!(
            effects.contains(&HostEffect::Render { window: WINDOW }),
            "clearing the pressed look is a visible change"
        );

        assert!(
            activations(&host.handle(pointer(PointerState::Released, x, y))).is_empty(),
            "the release that follows focus loss cannot activate anything"
        );
    }

    #[test]
    fn focus_loss_cancels_a_space_capture_and_the_release_cannot_activate() {
        let mut host = keyboard_host();
        host.handle(key_event(instar_window::Key::Space, true, false));
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            Some(NodeKey::first(91))
        );

        let effects = host.handle(focus_lost());
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            None,
            "a Space held on the focused control dies with focus"
        );
        assert_eq!(focused(&host), Some(NodeKey::first(91)));
        assert!(
            effects.contains(&HostEffect::Render { window: WINDOW }),
            "the held button's pressed look cleared, so a frame is due"
        );

        assert!(
            activations(&host.handle(key_event(instar_window::Key::Space, false, false)))
                .is_empty(),
            "the Space up after focus loss cannot activate anything"
        );

        assert!(
            host.handle(focus_regained()).is_empty(),
            "regaining focus restores nothing and asks for nothing"
        );
        assert_eq!(
            host.window(WINDOW).unwrap().interaction.pressed(),
            None,
            "the held input is not resurrected by a regain"
        );
        assert!(
            activations(&host.handle(key_event(instar_window::Key::Space, false, false)))
                .is_empty(),
            "and a later Space up still cannot activate the dead capture"
        );
    }

    #[test]
    fn focus_loss_clears_hover_and_drag_while_retaining_focus() {
        let mut host = scrolled_host();
        host.handle(key(instar_window::Key::Tab, false));
        assert_eq!(focused(&host), Some(NodeKey::first(53)));

        let bar = bar_of(&host);
        let x = f64::from(bar.thumb.x + 2);
        let start_y = f64::from(bar.thumb.y + 2);
        host.on_pointer_moved(WINDOW, bar.thumb.x + 2, bar.thumb.y + 2);
        host.handle(pointer(PointerState::Pressed, x, start_y));
        assert!(
            host.window(WINDOW).unwrap().scroll.dragging().is_some(),
            "the drag is live before focus is lost"
        );

        let effects = host.handle(focus_lost());
        let window = host.window(WINDOW).unwrap();
        assert_eq!(window.scroll.hovered(), None);
        assert_eq!(window.scroll.dragging(), None);
        assert_eq!(
            focused(&host),
            Some(NodeKey::first(53)),
            "the focused NodeKey survives focus loss"
        );
        assert!(
            effects.contains(&HostEffect::Render { window: WINDOW }),
            "hover and the held thumb both cleared, so a frame is due"
        );
    }

    #[test]
    fn focus_loss_with_nothing_captured_requests_no_frame() {
        let mut host = keyboard_host();
        let effects = host.handle(focus_lost());
        assert!(
            effects.is_empty(),
            "a loss that changes no hover, press or drag is a no-op: \
             {effects:?}"
        );
        assert_eq!(focused(&host), Some(NodeKey::first(91)));
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

    // --- Text-view attachments (B2e-3b) ---

    /// Opens one buffer and one view for `GEN1`, registering both leases.
    fn create_view(host: &mut Host) -> OpaqueResourceKey {
        let mut serve = |operation: TextOperation| {
            let (request, wait) = text_request(GEN1, operation);
            let screened = request.screen(GEN1).expect("current generation");
            host.text_resources_mut().serve(screened);
            wait.blocking_recv().expect("answered")
        };

        let buffer = match serve(TextOperation::CreateBuffer) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a buffer, got {other:?}"),
        };
        match serve(TextOperation::CreateView { buffer }) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a view, got {other:?}"),
        }
    }

    /// A root whose text-view children name the given slots.
    fn attachment_batch(nodes: &[(u32, u16)]) -> Vec<u8> {
        let mut encoder = BatchEncoder::new();
        encoder.node(
            opcode::NODE_ROOT,
            NodeKey::first(0),
            0,
            None,
            WireLayout::default(),
            nodes.len() as u16,
        );
        for (id, slot) in nodes {
            encoder.text_view(
                NodeKey::first(*id),
                flags::ENABLED,
                *slot,
                WireLayout::default(),
            );
        }
        encoder.finish()
    }

    /// Stages a commit exactly as the bridge's normative order would, with
    /// every key assumed to resolve, and returns the staged commit plus the
    /// resolved map.
    fn stage_with_attachments(
        host: &mut Host,
        batch: &[u8],
        keys: &[OpaqueResourceKey],
    ) -> (StagedUiCommit, BTreeMap<NodeKey, TextViewId>) {
        let snapshot = DecodedUiSnapshot::decode(batch).expect("batch decodes");
        let resolved = host
            .resolve_attachment_table(GEN1, keys)
            .expect("every key is leased to the generation");
        // Production's own slot resolution, not a reimplementation of it.
        // Rebuilding the map here would leave the permutation regression below
        // proving something about this helper rather than about the host.
        let attachments = Host::resolve_attachments(&snapshot.text_attachments, &resolved)
            .expect("slots are in range and no view is claimed twice");
        let validated = host
            .validate_ui_commit(WINDOW, snapshot.tree, snapshot.text_attachments)
            .expect("the tree diff accepts");
        let staged = host
            .stage_ui_commit(WINDOW, validated, attachments.clone())
            .expect("the ledger accepts");
        (staged, attachments)
    }

    /// The tableless entry point refuses a text view rather than stripping it.
    ///
    /// `on_guest_commit` carries no capabilities, so a slot names nothing.
    /// Admitting the tree with the attachment quietly dropped would retain a
    /// text surface the guest believes is showing a document, and leave no
    /// record that it ever named one.
    #[test]
    fn a_text_view_without_a_side_table_is_refused_not_stripped() {
        let mut host = Host::new();
        let batch = attachment_batch(&[(10, 0)]);

        assert!(matches!(
            host.on_guest_commit(WINDOW, &batch),
            Err(TreeError::AttachmentWithoutTable { slot: 0, .. })
        ));
        assert!(
            host.window(WINDOW).is_none_or(|w| w.tree().is_none()),
            "and the refusal left no tree behind"
        );
    }

    /// A: slot 0 -> V7, B: slot 9 -> V7. The attachment diff is EMPTY.
    ///
    /// The slot is commit-local scratch: what survived into the retained map
    /// is `node10 -> V7` in both commits, so a diff that looked at slots
    /// would report a change that never happened.
    #[test]
    fn a_moving_slot_that_resolves_to_the_same_view_diffs_to_nothing() {
        let mut host = Host::new();
        let v7 = create_view(&mut host);
        let a = attachment_batch(&[(10, 0)]);
        let b = attachment_batch(&[(10, 9)]);

        let (staged_a, _) = stage_with_attachments(&mut host, &a, &[v7]);
        host.apply_staged_commit(WINDOW, staged_a);

        // B's table is ten entries long so slot 9 can name V7; the other nine
        // entries are unreferenced scratch, which is legal.
        let (staged_b, map_b) = stage_with_attachments(&mut host, &b, &[v7; 10]);
        assert!(
            staged_b.tree_changes.is_empty(),
            "the tree is identical; only the slot moved"
        );
        assert_eq!(
            staged_b.attachment_changes,
            attachment::AttachmentChangeSet::default(),
            "slot 0 and slot 9 both resolve to V7, so nothing changed in the \
             only representation admission retains"
        );
        assert_eq!(
            map_b.get(&NodeKey::first(10)),
            Some(&TextViewId {
                id: 0,
                generation: 0
            }),
            "and the retained map really does still name V7"
        );
    }

    /// A: slot 0 -> V7, B: slot 0 -> V12. The attachment diff CHANGED.
    #[test]
    fn the_same_node_now_naming_a_different_view_is_replaced() {
        let mut host = Host::new();
        let v7 = create_view(&mut host);
        let v12 = create_view(&mut host);
        let a = attachment_batch(&[(10, 0)]);
        let b = attachment_batch(&[(10, 0)]);

        let (staged_a, _) = stage_with_attachments(&mut host, &a, &[v7]);
        host.apply_staged_commit(WINDOW, staged_a);

        let (staged_b, _) = stage_with_attachments(&mut host, &b, &[v12]);
        assert_eq!(
            staged_b.attachment_changes.replaced,
            vec![NodeKey::first(10)],
            "node10 now names V12 instead of V7"
        );
        assert!(staged_b.attachment_changes.attached.is_empty());
        assert!(staged_b.attachment_changes.detached.is_empty());
    }

    /// The permutation: the same two nodes keep the same two views while both
    /// slots move. Tree diff EMPTY, attachment diff EMPTY.
    ///
    /// This is the case that makes the table's positional scratch semantics
    /// unavoidable: a single moving slot only proves one slot may move, while
    /// swapping both proves the whole table is positional scratch space. Any
    /// code comparing slot numbers or side-table positions must fail it.
    #[test]
    fn swapping_every_slot_still_diff_to_nothing() {
        let mut host = Host::new();
        let v7 = create_view(&mut host);
        let v8 = create_view(&mut host);
        let v7_id = host.resolve_attachment_table(GEN1, &[v7]).unwrap()[0];
        let v8_id = host.resolve_attachment_table(GEN1, &[v8]).unwrap()[0];
        let a = attachment_batch(&[(10, 0), (20, 1)]);
        let b = attachment_batch(&[(10, 1), (20, 0)]);

        let (staged_a, _) = stage_with_attachments(&mut host, &a, &[v7, v8]);
        host.apply_staged_commit(WINDOW, staged_a);

        // B swaps the table order too: slot 1 now names V7 and slot 0 names
        // V8, so both nodes keep the same view while every position moved.
        let (staged_b, map_b) = stage_with_attachments(&mut host, &b, &[v8, v7]);
        assert!(
            staged_b.tree_changes.is_empty(),
            "the tree is identical; only the slot assignment moved"
        );
        assert_eq!(
            staged_b.attachment_changes,
            attachment::AttachmentChangeSet::default(),
            "node10 still names V7 and node20 still names V8, so no NodeKey \
             was attached, detached, or replaced"
        );
        assert_eq!(map_b.get(&NodeKey::first(10)), Some(&v7_id));
        assert_eq!(map_b.get(&NodeKey::first(20)), Some(&v8_id));
    }
}
