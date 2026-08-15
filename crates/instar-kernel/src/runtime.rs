//! Guest lifecycle: generations, operations, teardown (WP4).
//!
//! This module encodes the rule Gate 0 forced (see `docs/GATE-0.md` Finding 5
//! and `docs/PHASE-1.md`):
//!
//! > **A guest's lifetime boundary is its `Store` plus component instance.**
//! > Never drop a suspended guest future and then reuse that `Store` as if
//! > nothing happened.
//!
//! The consequence is that there are two unrelated things both called
//! "cancellation", and keeping them apart is most of what this module is for:
//!
//! | | Per-operation | Whole-generation |
//! |---|---|---|
//! | Requested by | the guest, via `ops.cancel` | the host, never the guest |
//! | Mechanism | abort the host task backing that operation | drop the instance and `Store` |
//! | Guest task | stays alive and keeps running | ceases to exist |
//! | Frequency | ordinary control flow | restart, trap recovery |
//!
//! Dropping the future that drives a guest is **only** ever the second one. It
//! is not a per-operation mechanism, because an abandoned suspended task
//! retains runtime bookkeeping for the life of its `Store` — which is exactly
//! why the `Store` does not outlive the guest.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use wasmtime::Store;
use wasmtime::component::{Component, Linker, Resource, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bridge::{CommitRejection, CommitSink};
use crate::resource::{EPOCH_DEADLINE_TICKS, MeasuredLimiter, ResourceMetrics, ResourcePolicy};

wasmtime::component::bindgen!({
    path: "wit",
    world: "kernel",
    imports: {
        default: trappable,
        "instar:kernel/text-layouts": async | trappable,
        "instar:kernel/surfaces": async | trappable,
    },
    with: {
        "instar:kernel/text-layouts.text-layout": crate::presentation::GuestTextLayout,
    },
});

use instar::kernel::kernel_types::{CommitError, CommitResult, OpError, RuntimeError};

/// Identifies one guest generation: one `Store`, one component instance, one
/// guest task. Monotonic and never reused, so a stale message can always be
/// recognised by comparing against the current generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationId(pub u64);

impl std::fmt::Display for GenerationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "gen{}", self.0)
    }
}

/// Why an operation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpOutcome {
    Completed(Vec<u8>),
    Failed(String),
    Cancelled,
}

/// A host-owned operation belonging to exactly one generation.
struct Operation {
    generation: GenerationId,
    /// Aborting this is how per-operation cancellation actually happens. Note
    /// what it is *not*: it never touches the guest task, which stays alive
    /// and simply observes its `await-op` resolve as cancelled.
    abort: tokio::task::AbortHandle,
    /// Taken by the first `await-op`; a second await on the same id sees
    /// `unknown`, which is the honest answer since the result is gone.
    join: Option<tokio::task::JoinHandle<Result<Vec<u8>, String>>>,
    cancelled: bool,
}

/// Tracks in-flight host operations across generations.
///
/// Keyed globally rather than per-generation so that an id minted by a dead
/// generation is *recognised and rejected* rather than colliding with a live
/// one. Ids are never reused.
#[derive(Default)]
pub struct OperationRegistry {
    next_id: u64,
    ops: HashMap<u64, Operation>,
}

impl OperationRegistry {
    fn insert(
        &mut self,
        generation: GenerationId,
        join: tokio::task::JoinHandle<Result<Vec<u8>, String>>,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.ops.insert(
            id,
            Operation {
                generation,
                abort: join.abort_handle(),
                join: Some(join),
                cancelled: false,
            },
        );
        id
    }

    /// Number of operations still tracked. The soak test asserts this returns
    /// to its baseline, which is what "teardown does not leak" means here.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Cancels one operation on the host's initiative.
    ///
    /// The guest-facing path is `ops.cancel`; this is the same mechanism
    /// reached from the other side, so that a host which knows a piece of work
    /// is no longer wanted (its window closed, its result superseded) can stop
    /// it without waiting to be asked. It is still *per-operation*: the guest
    /// task keeps running and simply sees its `await-op` resolve as cancelled.
    fn cancel(&mut self, generation: GenerationId, id: u64) -> bool {
        match self.ops.get_mut(&id) {
            Some(op) if op.generation == generation && !op.cancelled => {
                op.abort.abort();
                op.cancelled = true;
                true
            }
            _ => false,
        }
    }

    /// Cancels every operation belonging to `generation`. Step 3 of the
    /// teardown sequence: host-owned children die with their parent, rather
    /// than outliving it and completing into a generation that no longer
    /// exists.
    pub fn cancel_generation(&mut self, generation: GenerationId) -> usize {
        let ids: Vec<u64> = self
            .ops
            .iter()
            .filter(|(_, op)| op.generation == generation)
            .map(|(id, _)| *id)
            .collect();

        for id in &ids {
            if let Some(op) = self.ops.remove(id) {
                op.abort.abort();
            }
        }
        ids.len()
    }
}

/// Host state shared across generations.
///
/// Everything a *successor* generation must be protected from lives here: the
/// commit log, the operation registry, and the current generation id that both
/// are checked against.
pub struct SharedKernel {
    current: AtomicU64,
    operations: Mutex<OperationRegistry>,
    commits: Mutex<Vec<(GenerationId, Vec<u8>)>>,
    revision: AtomicU64,
    stale_commits_rejected: AtomicU64,
    /// Commits refused because their generation already had one outstanding.
    ///
    /// Kept separate from stale rejections because the two are different
    /// failures: a stale generation is dead, while an in-progress rejection
    /// is a live generation being asked to overlap itself.
    commit_in_progress_rejections: AtomicU64,
    /// Where commits go when an embedder owns the retained tree (WP7B1).
    ///
    /// Absent by default, and the absence is a supported mode rather than an
    /// unconfigured one: a headless test that only wants to see what a guest
    /// committed has no presentation thread to marshal onto, and gets the
    /// in-memory log below instead.
    ///
    /// `OnceLock` because a sink may be installed exactly once, before any
    /// generation runs. Swapping the owner of the retained tree underneath a
    /// live guest is not a thing this design has an answer for, so it is not
    /// expressible.
    commit_sink: OnceLock<Arc<dyn CommitSink>>,
    presentation_sink: OnceLock<crate::presentation::SharedPresentationSink>,
}

impl Default for SharedKernel {
    fn default() -> Self {
        Self {
            // Generation 0 is never a live generation; the first
            // `new_generation` call produces gen1. This makes "no generation
            // is current" representable without an Option.
            current: AtomicU64::new(0),
            operations: Mutex::default(),
            commits: Mutex::default(),
            revision: AtomicU64::new(0),
            stale_commits_rejected: AtomicU64::new(0),
            commit_in_progress_rejections: AtomicU64::new(0),
            commit_sink: OnceLock::new(),
            presentation_sink: OnceLock::new(),
        }
    }
}

impl SharedKernel {
    pub fn current_generation(&self) -> GenerationId {
        GenerationId(self.current.load(Ordering::SeqCst))
    }

    pub fn install_presentation_sink(
        &self,
        sink: crate::presentation::SharedPresentationSink,
    ) -> Result<(), &'static str> {
        self.presentation_sink
            .set(sink)
            .map_err(|_| "a presentation sink is already installed")
    }

    pub async fn submit_presentation(
        &self,
        generation: GenerationId,
        operation: crate::presentation::PresentationOperation,
    ) -> Result<crate::presentation::PresentationAnswer, crate::presentation::PresentationRefusal>
    {
        use crate::presentation::PresentationRefusal;
        if !self.is_current(generation) {
            return Err(PresentationRefusal::StaleGeneration);
        }
        let Some(sink) = self.presentation_sink.get() else {
            return Err(PresentationRefusal::HostUnavailable);
        };
        let (request, answer) = crate::presentation::request(generation, operation);
        sink.submit(request)
            .map_err(|_| PresentationRefusal::HostUnavailable)?;
        answer
            .await
            .unwrap_or(Err(PresentationRefusal::HostUnavailable))
    }

    fn is_current(&self, generation: GenerationId) -> bool {
        generation == self.current_generation()
    }

    /// Commits accepted so far, oldest first, tagged with their generation.
    pub fn commits(&self) -> Vec<(GenerationId, Vec<u8>)> {
        self.commits.lock().expect("commit log poisoned").clone()
    }

    pub fn commits_utf8(&self) -> Vec<String> {
        self.commits()
            .into_iter()
            .map(|(g, c)| format!("{g}:{}", String::from_utf8_lossy(&c)))
            .collect()
    }

    /// How many commits were rejected for arriving from a superseded
    /// generation. Should be zero in normal operation — a non-zero value means
    /// a generation outlived its teardown, which is the bug this whole module
    /// exists to prevent.
    pub fn stale_commits_rejected(&self) -> u64 {
        self.stale_commits_rejected.load(Ordering::SeqCst)
    }

    /// How many commits were refused because another commit from the same
    /// generation was already outstanding.
    pub fn commit_single_flight_rejections(&self) -> u64 {
        self.commit_in_progress_rejections.load(Ordering::SeqCst)
    }

    pub fn live_operations(&self) -> usize {
        self.operations.lock().expect("registry poisoned").len()
    }

    /// Cancels one in-flight operation on the host's initiative.
    ///
    /// Per-operation, never whole-generation: the guest task stays alive. See
    /// this module's opening table for why the distinction is load-bearing.
    pub fn cancel_operation(&self, generation: GenerationId, id: u64) -> bool {
        self.operations
            .lock()
            .expect("registry poisoned")
            .cancel(generation, id)
    }

    /// Installs the side that owns the retained tree. Once only; a second
    /// attempt is refused rather than silently ignored.
    pub fn install_commit_sink(&self, sink: Arc<dyn CommitSink>) -> Result<(), &'static str> {
        self.commit_sink
            .set(sink)
            .map_err(|_| "a commit sink is already installed")
    }

    pub fn has_commit_sink(&self) -> bool {
        self.commit_sink.get().is_some()
    }

    /// Answers one guest `commit` call.
    ///
    /// The generation check happens twice on purpose. Here it is the cheap
    /// local one, so a superseded guest's batch never crosses a thread at all.
    /// The main thread checks again on arrival, because by then the answer may
    /// have changed — a generation can be torn down while its commit is in
    /// flight, and `docs/PHASE-1.md` makes that second check the *first* thing
    /// the main thread does.
    async fn commit_batch(
        &self,
        generation: GenerationId,
        commit_slot: Arc<tokio::sync::Semaphore>,
        batch: Vec<u8>,
    ) -> Result<CommitResult, CommitError> {
        if !self.is_current(generation) {
            self.stale_commits_rejected.fetch_add(1, Ordering::SeqCst);
            return Err(CommitError::StaleGeneration);
        }

        // Single-flight gate. The permit is RAII: it is released when this
        // future completes, is rejected, is cancelled, or is dropped -- so a
        // later sequential commit can always proceed after the first resolves.
        let _commit_permit = match commit_slot.try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.commit_in_progress_rejections
                    .fetch_add(1, Ordering::SeqCst);
                return Err(CommitError::CommitInProgress);
            }
        };

        let Some(sink) = self.commit_sink.get() else {
            // No owner installed: keep the batch here so a headless caller can
            // read back what the guest said. Note this log is *not* written on
            // the sink path — a host that owns the tree does not need the
            // kernel hoarding every batch it was ever handed.
            let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
            self.commits
                .lock()
                .expect("commit log poisoned")
                .push((generation, batch));
            return Ok(CommitResult { revision });
        };

        let (request, reply) = crate::bridge::commit_request(generation, batch);
        if sink.submit(request).is_err() {
            // The request came back, so nobody took ownership of answering it.
            // Dropping it here is the answer.
            return Err(CommitError::HostUnavailable);
        }

        match reply.await {
            Ok(Ok(revision)) => Ok(CommitResult { revision }),
            Ok(Err(CommitRejection::Invalid(reason))) => Err(CommitError::InvalidBatch(reason)),
            Ok(Err(CommitRejection::StaleGeneration)) => {
                self.stale_commits_rejected.fetch_add(1, Ordering::SeqCst);
                Err(CommitError::StaleGeneration)
            }
            Ok(Err(CommitRejection::HostUnavailable)) => Err(CommitError::HostUnavailable),
            // The reply channel was dropped without an answer, which is what
            // tearing the presentation side down mid-commit looks like from
            // here. Waking the guest with a verdict it can act on beats
            // leaving it parked on a reply that is never coming.
            Err(_) => Err(CommitError::HostUnavailable),
        }
    }
}

/// Per-generation store data.
struct GenerationState {
    ctx: WasiCtx,
    table: ResourceTable,
    kernel: Arc<SharedKernel>,
    generation: GenerationId,
    commit_slot: Arc<tokio::sync::Semaphore>,
    events: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Event>>>,
    limits: MeasuredLimiter,
}

impl WasiView for GenerationState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// What the host hands the guest when it asks for an event.
pub enum Event {
    Payload(Vec<u8>),
    Shutdown,
}

impl instar::kernel::kernel_types::Host for GenerationState {}
impl instar::kernel::text_layout_types::Host for GenerationState {}
impl instar::kernel::surface_types::Host for GenerationState {}

fn layout_error(
    refusal: crate::presentation::PresentationRefusal,
) -> instar::kernel::text_layout_types::LayoutError {
    use crate::presentation::PresentationRefusal as R;
    use instar::kernel::text_layout_types::LayoutError as E;
    match refusal {
        R::TextTooLarge(value) => E::TextTooLarge(value),
        R::InvalidStyle => E::InvalidStyle,
        R::TooManyLines(value) => E::TooManyLines(value),
        R::TooManyClusters(value) => E::TooManyClusters(value),
        R::InvalidCursor(value) => E::InvalidCursor(value),
        R::TooManySelectionRects(value) => E::TooManySelectionRects(value),
        R::TooManyLiveLayouts(value) => E::TooManyLiveLayouts(value),
        R::NoSuchLayout => E::NoSuchLayout,
        R::StaleGeneration => E::StaleGeneration,
        _ => E::HostUnavailable,
    }
}

fn surface_error(
    refusal: crate::presentation::PresentationRefusal,
) -> instar::kernel::surface_types::SurfaceError {
    use crate::presentation::PresentationRefusal as R;
    use instar::kernel::surface_types::SurfaceError as E;
    match refusal {
        R::StaleGeneration => E::StaleGeneration,
        R::NoSuchSurface => E::NoSuchSurface,
        R::UpdateInProgress => E::UpdateInProgress,
        R::SceneTooLarge(value) => E::SceneTooLarge(value),
        R::TooManyLayouts(value) => E::TooManyLayouts(value),
        R::NoSuchLayout => E::NoSuchLayout,
        R::InvalidScene(reason) => E::InvalidScene(reason),
        R::NotFocusable => E::NotFocusable,
        R::NotInterested => E::NotInterested,
        _ => E::HostUnavailable,
    }
}

fn bridge_cursor(
    cursor: instar::kernel::text_layout_types::Cursor,
) -> crate::presentation::BridgeCursor {
    crate::presentation::BridgeCursor {
        index: cursor.byte_index,
        affinity: match cursor.affinity {
            instar::kernel::text_layout_types::Affinity::Downstream => {
                crate::presentation::BridgeAffinity::Downstream
            }
            instar::kernel::text_layout_types::Affinity::Upstream => {
                crate::presentation::BridgeAffinity::Upstream
            }
        },
    }
}

fn wit_cursor(
    cursor: crate::presentation::BridgeCursor,
) -> instar::kernel::text_layout_types::Cursor {
    instar::kernel::text_layout_types::Cursor {
        byte_index: cursor.index,
        affinity: match cursor.affinity {
            crate::presentation::BridgeAffinity::Downstream => {
                instar::kernel::text_layout_types::Affinity::Downstream
            }
            crate::presentation::BridgeAffinity::Upstream => {
                instar::kernel::text_layout_types::Affinity::Upstream
            }
        },
    }
}

impl instar::kernel::text_layouts::Host for GenerationState {
    async fn create_layout(
        &mut self,
        text: String,
        style: instar::kernel::text_layout_types::LayoutStyle,
    ) -> wasmtime::Result<
        Result<
            Resource<crate::presentation::GuestTextLayout>,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        use crate::presentation::*;
        let style = BridgeLayoutStyle {
            role: match style.role {
                instar::kernel::text_layout_types::FontRole::SystemUi => BridgeFontRole::SystemUi,
                instar::kernel::text_layout_types::FontRole::Monospace => BridgeFontRole::Monospace,
            },
            size: style.size,
            weight: style.weight,
            wrap: style.wrap,
            line_height: match style.line_height {
                instar::kernel::text_layout_types::LineHeight::MetricsRelative(value) => {
                    BridgeLineHeight::MetricsRelative(value)
                }
                instar::kernel::text_layout_types::LineHeight::FontSizeRelative(value) => {
                    BridgeLineHeight::FontSizeRelative(value)
                }
                instar::kernel::text_layout_types::LineHeight::Absolute(value) => {
                    BridgeLineHeight::Absolute(value)
                }
            },
            width: style.width,
            alignment: match style.alignment {
                instar::kernel::text_layout_types::Alignment::Start => BridgeAlignment::Start,
                instar::kernel::text_layout_types::Alignment::Center => BridgeAlignment::Center,
                instar::kernel::text_layout_types::Alignment::End => BridgeAlignment::End,
            },
        };
        match self
            .kernel
            .submit_presentation(
                self.generation,
                PresentationOperation::CreateLayout { text, style },
            )
            .await
        {
            Ok(PresentationAnswer::Layout(key)) => {
                Ok(Ok(self.table.push(GuestTextLayout { key })?))
            }
            Ok(other) => Err(wasmtime::Error::msg(format!(
                "presentation sink returned {other:?} to create-layout"
            ))),
            Err(error) => Ok(Err(layout_error(error))),
        }
    }
}

impl GenerationState {
    async fn query_layout(
        &mut self,
        handle: Resource<crate::presentation::GuestTextLayout>,
        query: crate::presentation::LayoutQuery,
    ) -> Result<crate::presentation::PresentationAnswer, crate::presentation::PresentationRefusal>
    {
        let key = match self.table.get(&handle) {
            Ok(layout) => layout.key,
            Err(_) => return Err(crate::presentation::PresentationRefusal::NoSuchLayout),
        };
        self.kernel
            .submit_presentation(
                self.generation,
                crate::presentation::PresentationOperation::QueryLayout { key, query },
            )
            .await
    }
}

impl instar::kernel::text_layouts::HostTextLayout for GenerationState {
    async fn drop(
        &mut self,
        handle: Resource<crate::presentation::GuestTextLayout>,
    ) -> wasmtime::Result<()> {
        let Ok(layout) = self.table.delete(handle) else {
            return Ok(());
        };
        let _ = self
            .kernel
            .submit_presentation(
                self.generation,
                crate::presentation::PresentationOperation::ReleaseLayout { key: layout.key },
            )
            .await;
        Ok(())
    }

    async fn metrics(
        &mut self,
        self_: Resource<crate::presentation::GuestTextLayout>,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Metrics,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        match self
            .query_layout(self_, crate::presentation::LayoutQuery::Metrics)
            .await
        {
            Ok(crate::presentation::PresentationAnswer::Metrics(m)) => {
                Ok(Ok(instar::kernel::text_layout_types::Metrics {
                    width: m.width,
                    height: m.height,
                    lines: m.lines,
                    clusters: m.clusters,
                }))
            }
            Ok(other) => Err(wasmtime::Error::msg(format!(
                "presentation sink returned {other:?} to metrics"
            ))),
            Err(error) => Ok(Err(layout_error(error))),
        }
    }

    async fn cursor_from_point(
        &mut self,
        self_: Resource<crate::presentation::GuestTextLayout>,
        x: f32,
        y: f32,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        match self
            .query_layout(
                self_,
                crate::presentation::LayoutQuery::CursorFromPoint {
                    x_bits: x.to_bits(),
                    y_bits: y.to_bits(),
                },
            )
            .await
        {
            Ok(crate::presentation::PresentationAnswer::Cursor(c)) => Ok(Ok(wit_cursor(c))),
            Ok(other) => Err(wasmtime::Error::msg(format!("bad cursor answer {other:?}"))),
            Err(error) => Ok(Err(layout_error(error))),
        }
    }

    async fn caret_rect(
        &mut self,
        self_: Resource<crate::presentation::GuestTextLayout>,
        cursor: instar::kernel::text_layout_types::Cursor,
        width: f32,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Rect,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        let query = crate::presentation::LayoutQuery::CaretRect {
            cursor: bridge_cursor(cursor),
            width_bits: width.to_bits(),
        };
        match self.query_layout(self_, query).await {
            Ok(crate::presentation::PresentationAnswer::Rect(r)) => {
                Ok(Ok(instar::kernel::text_layout_types::Rect {
                    x: r.x,
                    y: r.y,
                    width: r.width,
                    height: r.height,
                }))
            }
            Ok(other) => Err(wasmtime::Error::msg(format!("bad rect answer {other:?}"))),
            Err(error) => Ok(Err(layout_error(error))),
        }
    }

    async fn selection_rects(
        &mut self,
        self_: Resource<crate::presentation::GuestTextLayout>,
        anchor: instar::kernel::text_layout_types::Cursor,
        focus: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            Vec<instar::kernel::text_layout_types::Rect>,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        let query = crate::presentation::LayoutQuery::SelectionRects {
            anchor: bridge_cursor(anchor),
            focus: bridge_cursor(focus),
        };
        match self.query_layout(self_, query).await {
            Ok(crate::presentation::PresentationAnswer::Rects(rects)) => Ok(Ok(rects
                .into_iter()
                .map(|r| instar::kernel::text_layout_types::Rect {
                    x: r.x,
                    y: r.y,
                    width: r.width,
                    height: r.height,
                })
                .collect())),
            Ok(other) => Err(wasmtime::Error::msg(format!("bad rects answer {other:?}"))),
            Err(error) => Ok(Err(layout_error(error))),
        }
    }

    async fn previous_visual(
        &mut self,
        h: Resource<crate::presentation::GuestTextLayout>,
        c: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        self.cursor_query(
            h,
            crate::presentation::LayoutQuery::PreviousVisual(bridge_cursor(c)),
        )
        .await
    }
    async fn next_visual(
        &mut self,
        h: Resource<crate::presentation::GuestTextLayout>,
        c: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        self.cursor_query(
            h,
            crate::presentation::LayoutQuery::NextVisual(bridge_cursor(c)),
        )
        .await
    }
    async fn visual_line_start(
        &mut self,
        h: Resource<crate::presentation::GuestTextLayout>,
        c: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        self.cursor_query(
            h,
            crate::presentation::LayoutQuery::VisualLineStart(bridge_cursor(c)),
        )
        .await
    }
    async fn visual_line_end(
        &mut self,
        h: Resource<crate::presentation::GuestTextLayout>,
        c: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        self.cursor_query(
            h,
            crate::presentation::LayoutQuery::VisualLineEnd(bridge_cursor(c)),
        )
        .await
    }
    async fn hard_line_start(
        &mut self,
        h: Resource<crate::presentation::GuestTextLayout>,
        c: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        self.cursor_query(
            h,
            crate::presentation::LayoutQuery::HardLineStart(bridge_cursor(c)),
        )
        .await
    }
    async fn hard_line_end(
        &mut self,
        h: Resource<crate::presentation::GuestTextLayout>,
        c: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        self.cursor_query(
            h,
            crate::presentation::LayoutQuery::HardLineEnd(bridge_cursor(c)),
        )
        .await
    }
    async fn previous_standard_word_boundary(
        &mut self,
        h: Resource<crate::presentation::GuestTextLayout>,
        c: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        self.cursor_query(
            h,
            crate::presentation::LayoutQuery::PreviousWord(bridge_cursor(c)),
        )
        .await
    }
    async fn next_standard_word_boundary(
        &mut self,
        h: Resource<crate::presentation::GuestTextLayout>,
        c: instar::kernel::text_layout_types::Cursor,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        self.cursor_query(
            h,
            crate::presentation::LayoutQuery::NextWord(bridge_cursor(c)),
        )
        .await
    }
}

impl GenerationState {
    async fn cursor_query(
        &mut self,
        handle: Resource<crate::presentation::GuestTextLayout>,
        query: crate::presentation::LayoutQuery,
    ) -> wasmtime::Result<
        Result<
            instar::kernel::text_layout_types::Cursor,
            instar::kernel::text_layout_types::LayoutError,
        >,
    > {
        match self.query_layout(handle, query).await {
            Ok(crate::presentation::PresentationAnswer::Cursor(c)) => Ok(Ok(wit_cursor(c))),
            Ok(other) => Err(wasmtime::Error::msg(format!("bad cursor answer {other:?}"))),
            Err(error) => Ok(Err(layout_error(error))),
        }
    }
}

impl GenerationState {
    async fn surface_control(
        &mut self,
        operation: crate::presentation::PresentationOperation,
    ) -> wasmtime::Result<Result<(), instar::kernel::surface_types::SurfaceError>> {
        match self
            .kernel
            .submit_presentation(self.generation, operation)
            .await
        {
            Ok(crate::presentation::PresentationAnswer::Unit) => Ok(Ok(())),
            Ok(other) => Err(wasmtime::Error::msg(format!(
                "bad surface-control answer {other:?}"
            ))),
            Err(error) => Ok(Err(surface_error(error))),
        }
    }
}

impl instar::kernel::surfaces::Host for GenerationState {
    async fn capture_pointer(
        &mut self,
        target: instar::kernel::surface_types::NodeKey,
    ) -> wasmtime::Result<Result<(), instar::kernel::surface_types::SurfaceError>> {
        self.surface_control(crate::presentation::PresentationOperation::CapturePointer {
            target: (target.id, target.generation),
        })
        .await
    }
    async fn release_pointer(
        &mut self,
        target: instar::kernel::surface_types::NodeKey,
    ) -> wasmtime::Result<Result<(), instar::kernel::surface_types::SurfaceError>> {
        self.surface_control(crate::presentation::PresentationOperation::ReleasePointer {
            target: (target.id, target.generation),
        })
        .await
    }
    async fn request_focus(
        &mut self,
        target: instar::kernel::surface_types::NodeKey,
    ) -> wasmtime::Result<Result<(), instar::kernel::surface_types::SurfaceError>> {
        self.surface_control(crate::presentation::PresentationOperation::RequestFocus {
            target: (target.id, target.generation),
        })
        .await
    }
    async fn configure_text_input(
        &mut self,
        target: instar::kernel::surface_types::NodeKey,
        enabled: bool,
        local_candidate_rect: instar::kernel::surface_types::LocalRect,
    ) -> wasmtime::Result<Result<(), instar::kernel::surface_types::SurfaceError>> {
        let rect = crate::presentation::BridgeRect {
            x: local_candidate_rect.x,
            y: local_candidate_rect.y,
            width: local_candidate_rect.width,
            height: local_candidate_rect.height,
        };
        self.surface_control(
            crate::presentation::PresentationOperation::ConfigureTextInput {
                target: (target.id, target.generation),
                enabled,
                rect,
            },
        )
        .await
    }
}

impl instar::kernel::surfaces::HostWithStore<GenerationState>
    for wasmtime::component::HasSelf<GenerationState>
{
    fn update_surface(
        accessor: &wasmtime::component::Accessor<GenerationState, Self>,
        target: instar::kernel::surface_types::NodeKey,
        scene: Vec<u8>,
        layouts: Vec<Resource<crate::presentation::GuestTextLayout>>,
    ) -> impl std::future::Future<
        Output = wasmtime::Result<Result<u64, instar::kernel::surface_types::SurfaceError>>,
    > + Send {
        let extracted = accessor.with(|mut access| {
            let state = access.get();
            let keys = layouts
                .into_iter()
                .map(|handle| state.table.get(&handle).map(|layout| layout.key))
                .collect::<Result<Vec<_>, _>>();
            (Arc::clone(&state.kernel), state.generation, keys)
        });
        async move {
            let (kernel, generation, keys) = extracted;
            let keys = match keys {
                Ok(keys) => keys,
                Err(_) => {
                    return Ok(Err(
                        instar::kernel::surface_types::SurfaceError::NoSuchLayout,
                    ));
                }
            };
            let operation = crate::presentation::PresentationOperation::UpdateSurface {
                target: (target.id, target.generation),
                scene,
                layouts: keys,
            };
            match kernel.submit_presentation(generation, operation).await {
                Ok(crate::presentation::PresentationAnswer::Revision(revision)) => Ok(Ok(revision)),
                Ok(other) => Err(wasmtime::Error::msg(format!(
                    "bad update-surface answer {other:?}"
                ))),
                Err(error) => Ok(Err(surface_error(error))),
            }
        }
    }
}
impl instar::kernel::kernel_runtime::Host for GenerationState {}
impl instar::kernel::ops::Host for GenerationState {
    fn start(&mut self, kind: String, payload: Vec<u8>) -> wasmtime::Result<u64> {
        Ok(self.start_operation(kind, payload))
    }

    /// Per-operation cancellation. Note what this does *not* touch: the guest
    /// task. It aborts the host task backing one operation and nothing else,
    /// which is precisely the distinction docs/PHASE-1.md draws between this
    /// and destroying a generation.
    fn cancel(&mut self, id: u64) -> wasmtime::Result<bool> {
        let mut registry = self.kernel.operations.lock().expect("registry poisoned");
        match registry.ops.get_mut(&id) {
            // Only the owning generation may cancel an operation; an id from a
            // dead generation is not this guest's to act on.
            Some(op) if op.generation == self.generation && !op.cancelled => {
                op.abort.abort();
                op.cancelled = true;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

impl instar::kernel::kernel_ui::Host for GenerationState {}

/// Committing suspends the guest (WP7B1).
///
/// Step 2 of teardown is enforced inside `commit_batch` at the point of effect
/// rather than trusted: a superseded generation's commits are refused, not
/// applied. What is new here is *where* the applying happens — off this
/// thread, on whichever thread owns the retained tree — which is why this is
/// an async import now and not a plain method.
impl instar::kernel::kernel_ui::HostWithStore<GenerationState>
    for wasmtime::component::HasSelf<GenerationState>
{
    fn commit(
        accessor: &wasmtime::component::Accessor<GenerationState, Self>,
        batch: Vec<u8>,
    ) -> impl std::future::Future<Output = wasmtime::Result<Result<CommitResult, CommitError>>> + Send
    {
        let (kernel, generation, commit_slot) = accessor.with(|mut access| {
            let state = access.get();
            (
                Arc::clone(&state.kernel),
                state.generation,
                Arc::clone(&state.commit_slot),
            )
        });

        async move { Ok(kernel.commit_batch(generation, commit_slot, batch).await) }
    }
}

impl instar::kernel::kernel_runtime::HostWithStore<GenerationState>
    for wasmtime::component::HasSelf<GenerationState>
{
    fn next_event(
        accessor: &wasmtime::component::Accessor<GenerationState, Self>,
    ) -> impl std::future::Future<Output = wasmtime::Result<Result<Vec<u8>, RuntimeError>>> + Send
    {
        let events = accessor.with(|mut access| Arc::clone(&access.get().events));

        async move {
            let mut events = events.lock().await;
            match events.recv().await {
                Some(Event::Payload(payload)) => Ok(Ok(payload)),
                Some(Event::Shutdown) | None => Ok(Err(RuntimeError::Shutdown)),
            }
        }
    }
}

impl instar::kernel::ops::HostWithStore<GenerationState>
    for wasmtime::component::HasSelf<GenerationState>
{
    fn await_op(
        accessor: &wasmtime::component::Accessor<GenerationState, Self>,
        id: u64,
    ) -> impl std::future::Future<Output = wasmtime::Result<Result<Vec<u8>, OpError>>> + Send {
        let (kernel, generation) = accessor.with(|mut access| {
            let state = access.get();
            (Arc::clone(&state.kernel), state.generation)
        });

        async move {
            // Take the join handle out under the lock, await it outside:
            // holding a std Mutex across an await would be a deadlock waiting
            // to happen, and the guest may be parked here for a long time.
            let join = {
                let mut registry = kernel.operations.lock().expect("registry poisoned");
                match registry.ops.get_mut(&id) {
                    // An id from another generation is not this guest's to
                    // await, even though the registry knows about it.
                    Some(op) if op.generation != generation => None,
                    Some(op) => op.join.take(),
                    None => None,
                }
            };

            let Some(join) = join else {
                return Ok(Err(OpError::Unknown));
            };

            let outcome = match join.await {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(message)) => Err(OpError::Failed(message)),
                // An aborted task is a cancelled operation -- either the guest
                // asked, or the generation is being torn down under it.
                Err(join_error) if join_error.is_cancelled() => Err(OpError::Cancelled),
                Err(join_error) => {
                    Err(OpError::Failed(format!("operation panicked: {join_error}")))
                }
            };

            // The operation is finished either way; stop tracking it so the
            // registry reflects only live work.
            kernel
                .operations
                .lock()
                .expect("registry poisoned")
                .ops
                .remove(&id);

            Ok(outcome)
        }
    }
}

impl GenerationState {
    fn start_operation(&mut self, kind: String, payload: Vec<u8>) -> u64 {
        let join = tokio::spawn(async move {
            match kind.as_str() {
                // Synthetic, for tests: sleep for `payload` parsed as millis.
                "delay" => {
                    let millis: u64 = String::from_utf8_lossy(&payload)
                        .parse()
                        .map_err(|e| format!("bad delay payload: {e}"))?;
                    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
                    Ok(format!("delayed:{millis}").into_bytes())
                }
                "echo" => Ok(payload),
                "fail" => Err(String::from_utf8_lossy(&payload).into_owned()),
                other => Err(format!("unknown operation kind: {other}")),
            }
        });

        self.kernel
            .operations
            .lock()
            .expect("registry poisoned")
            .insert(self.generation, join)
    }
}

/// One guest generation: exactly one `Store`, one instance, one guest task.
///
/// Deliberately not `Clone` and deliberately owns its `Store`: the type system
/// should make "reuse the Store across generations" hard to express by
/// accident.
pub struct RuntimeGeneration {
    id: GenerationId,
    store: Store<GenerationState>,
    bindings: Kernel,
    events: tokio::sync::mpsc::Sender<Event>,
    metrics: Arc<ResourceMetrics>,
}

/// How many events may be queued for a guest that has not yet asked for them.
///
/// Bounded, and it matters that it is: a guest suspended inside an unanswered
/// `commit` stops draining this queue entirely, and an unbounded inbox would
/// turn "the guest is behind" into "the host grows without limit". The bound
/// is also what makes a bounded queue *above* this one mean anything — a
/// backlog that simply moves from one unbounded place to another has not been
/// bounded, only relocated.
pub const EVENT_QUEUE_CAPACITY: usize = 256;

/// Reserved room in a guest's inbox.
///
/// Taking one of these before dequeuing work is how a caller applies
/// back-pressure to *itself* rather than to the guest: acquiring the permit is
/// the thing that waits, and it can be raced against the guest's own progress
/// so that nothing is blocked while it waits.
pub struct EventPermit<'a>(tokio::sync::mpsc::Permit<'a, Event>);

impl EventPermit<'_> {
    pub fn send(self, payload: impl Into<Vec<u8>>) {
        self.0.send(Event::Payload(payload.into()));
    }

    pub fn shutdown(self) {
        self.0.send(Event::Shutdown);
    }
}

/// Sends events to a generation's guest.
///
/// Split from [`RuntimeGeneration`] because running the guest borrows the
/// generation mutably for as long as the guest lives -- which is the entire
/// time you would want to send it anything. Taking a handle first is the
/// supported pattern:
///
/// ```ignore
/// let handle = generation.handle();
/// let mut run = std::pin::pin!(generation.run());
/// handle.send("...")?;
/// ```
#[derive(Clone)]
pub struct GenerationHandle {
    id: GenerationId,
    events: tokio::sync::mpsc::Sender<Event>,
}

impl GenerationHandle {
    pub fn id(&self) -> GenerationId {
        self.id
    }

    /// Queues an event for this generation's guest, without waiting.
    ///
    /// Fails rather than blocks when the guest is [`EVENT_QUEUE_CAPACITY`]
    /// events behind. A caller that can afford to wait — and can keep the
    /// guest running while it does — should [`GenerationHandle::reserve`]
    /// instead.
    pub fn send(&self, payload: impl Into<Vec<u8>>) -> Result<(), &'static str> {
        self.events
            .try_send(Event::Payload(payload.into()))
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    "the guest's event queue is full"
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    "generation is no longer receiving events"
                }
            })
    }

    /// Asks the guest to leave its event loop and return from `run`.
    pub fn shutdown(&self) -> Result<(), &'static str> {
        self.events
            .try_send(Event::Shutdown)
            .map_err(|_| "generation is no longer receiving events")
    }

    /// Waits for room in the guest's inbox and holds it.
    ///
    /// Await this *before* taking work off whatever queue feeds it: then a
    /// guest that has fallen behind stops that upstream queue draining, which
    /// lets the bound at the top of the chain do its job instead of the
    /// backlog quietly relocating one layer down.
    pub async fn reserve(&self) -> Result<EventPermit<'_>, &'static str> {
        self.events
            .reserve()
            .await
            .map(EventPermit)
            .map_err(|_| "generation is no longer receiving events")
    }
}

impl RuntimeGeneration {
    pub fn id(&self) -> GenerationId {
        self.id
    }

    /// A sender for this generation's events. Take one before calling
    /// [`RuntimeGeneration::run`].
    pub fn handle(&self) -> GenerationHandle {
        GenerationHandle {
            id: self.id,
            events: self.events.clone(),
        }
    }

    /// Runs this generation's guest to completion.
    pub async fn run(&mut self) -> wasmtime::Result<Result<(), String>> {
        let bindings = &self.bindings;
        self.store
            .run_concurrent(async move |accessor| bindings.call_run(accessor).await)
            .await?
    }

    /// Wasmtime's view of this store's concurrent bookkeeping, for leak
    /// assertions.
    pub fn concurrent_state_table_size(&mut self) -> usize {
        self.store.concurrent_state_table_size()
    }

    /// Resource evidence collected from this generation's Store.
    pub fn metrics(&self) -> Arc<ResourceMetrics> {
        Arc::clone(&self.metrics)
    }
}

/// Owns the engine, the shared host state, and the generation sequence.
pub struct Runtime {
    engine: wasmtime::Engine,
    component: Component,
    linker: Linker<GenerationState>,
    kernel: Arc<SharedKernel>,
    policy: ResourcePolicy,
}

impl Runtime {
    pub fn new(component_bytes: &[u8]) -> wasmtime::Result<Self> {
        Self::new_with_policy(component_bytes, ResourcePolicy::instar_default())
    }

    /// Builds a runtime whose generations run under `policy`.
    ///
    /// Production uses [`Runtime::new`], which applies Instar's one policy.
    /// The parameterised constructor exists so the measurement gate can probe
    /// a component's core-instance demand and so the resource tests can prove
    /// containment with a deliberately small ceiling.
    pub fn new_with_policy(
        component_bytes: &[u8],
        policy: ResourcePolicy,
    ) -> wasmtime::Result<Self> {
        let engine = crate::engine::configured_engine()?;
        let component = Component::from_binary(&engine, component_bytes)?;

        let mut linker: Linker<GenerationState> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        Kernel::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |s| s)?;
        #[cfg(feature = "bench-probe")]
        crate::bench_probe::add_to_linker(&mut linker)?;

        Ok(Self {
            engine,
            component,
            linker,
            kernel: Arc::default(),
            policy,
        })
    }

    pub fn kernel(&self) -> Arc<SharedKernel> {
        Arc::clone(&self.kernel)
    }

    /// The engine this runtime instantiates its generations on.
    ///
    /// Handed to the runtime thread so an out-of-band shutdown can increment
    /// the epoch and interrupt non-yielding Wasm.
    pub fn engine(&self) -> wasmtime::Engine {
        self.engine.clone()
    }

    pub fn policy(&self) -> ResourcePolicy {
        self.policy
    }

    /// Creates the next generation: a fresh `Store`, a fresh instance, and a
    /// bumped generation id.
    ///
    /// This is steps 5 and 6 of the teardown sequence, and also how the first
    /// generation is born. Note it takes no state from any previous
    /// generation — that is the whole point.
    pub async fn new_generation(&mut self) -> wasmtime::Result<RuntimeGeneration> {
        let id = GenerationId(self.kernel.current.fetch_add(1, Ordering::SeqCst) + 1);

        let (events_tx, events_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE_CAPACITY);
        let metrics = Arc::new(ResourceMetrics::for_component(&self.component));
        let state = GenerationState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            kernel: Arc::clone(&self.kernel),
            generation: id,
            commit_slot: Arc::new(tokio::sync::Semaphore::new(1)),
            events: Arc::new(tokio::sync::Mutex::new(events_rx)),
            limits: MeasuredLimiter::new(&self.policy, Arc::clone(&metrics)),
        };

        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        // The shutdown path's single epoch increment is the deadline. See
        // `resource::EPOCH_DEADLINE_TICKS`.
        store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        let instance = self
            .linker
            .instantiate_async(&mut store, &self.component)
            .await?;
        let bindings = Kernel::new(&mut store, &instance)?;

        Ok(RuntimeGeneration {
            id,
            store,
            bindings,
            events: events_tx,
            metrics,
        })
    }

    /// Destroys a generation, in the order `docs/PHASE-1.md` specifies.
    ///
    /// Steps 1 and 2 (mark dead, stop accepting commits) are implicit and
    /// permanent: `generation` is already not current the moment a successor
    /// is created, and `commit` checks that on every call. This function
    /// performs steps 3 and 4 — cancel host-owned children, then drop the
    /// whole instance and `Store`.
    ///
    /// Taking `generation` **by value** is the enforcement mechanism: the
    /// caller cannot keep using a generation it has torn down.
    pub fn destroy_generation(&mut self, generation: RuntimeGeneration) -> usize {
        let id = generation.id;

        // Step 3: host-owned children die with the parent. Do this *before*
        // dropping the store so nothing is left running that could complete
        // into a generation that no longer exists.
        let cancelled = self
            .kernel
            .operations
            .lock()
            .expect("registry poisoned")
            .cancel_generation(id);

        // Step 4: drop the instance and Store together. This is the only
        // supported way to reclaim a suspended guest task's runtime state.
        drop(generation);

        cancelled
    }
}

/// The guest fixture component for the kernel world, built by `build.rs`.
pub fn guest_component_bytes() -> std::io::Result<Vec<u8>> {
    std::fs::read(env!("KERNEL_GUEST_WASM"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{CommitRequest, CommitSink};
    use std::sync::Mutex;
    use std::time::Duration;

    /// A sink that accepts requests and lets the test answer them by hand.
    #[derive(Default)]
    struct HeldSink {
        held: Mutex<Vec<CommitRequest>>,
    }

    impl CommitSink for HeldSink {
        fn submit(&self, request: CommitRequest) -> Result<(), CommitRequest> {
            self.held.lock().expect("held sink poisoned").push(request);
            Ok(())
        }
    }

    impl HeldSink {
        async fn wait_for_one(&self) -> CommitRequest {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    {
                        let mut held = self.held.lock().expect("held sink poisoned");
                        if !held.is_empty() {
                            return held.remove(0);
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the sink never received the commit")
        }
    }

    fn kernel_with_sink() -> (Arc<SharedKernel>, Arc<HeldSink>) {
        let kernel = Arc::new(SharedKernel::default());
        let sink = Arc::new(HeldSink::default());
        kernel
            .install_commit_sink(sink.clone())
            .expect("installs once");
        (kernel, sink)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_commits_are_single_flight_and_the_slot_releases() {
        let (kernel, sink) = kernel_with_sink();
        let slot = Arc::new(tokio::sync::Semaphore::new(1));
        kernel.current.store(1, Ordering::SeqCst);

        let first = tokio::spawn({
            let kernel = Arc::clone(&kernel);
            let slot = Arc::clone(&slot);
            async move {
                kernel
                    .commit_batch(GenerationId(1), slot, b"first".to_vec())
                    .await
            }
        });
        let first_request = sink.wait_for_one().await;
        assert_eq!(first_request.generation(), GenerationId(1));

        // The second attempt must fail immediately, before any request is
        // created or enqueued.
        let second = kernel
            .commit_batch(GenerationId(1), Arc::clone(&slot), b"second".to_vec())
            .await;
        assert!(
            matches!(second, Err(CommitError::CommitInProgress)),
            "the concurrent attempt must fail as commit-in-progress, got {second:?}"
        );
        assert_eq!(kernel.commit_single_flight_rejections(), 1);

        let screened = first_request
            .screen(kernel.current_generation())
            .expect("gen1 is current");
        screened.accept(7);
        assert_eq!(
            first
                .await
                .expect("task ran")
                .expect("commit resolves")
                .revision,
            7
        );

        // Once the first resolves, a later sequential commit works again.
        let third = tokio::spawn({
            let kernel = Arc::clone(&kernel);
            let slot = Arc::clone(&slot);
            async move {
                kernel
                    .commit_batch(GenerationId(1), slot, b"third".to_vec())
                    .await
            }
        });
        let third_request = sink.wait_for_one().await;
        let screened = third_request
            .screen(kernel.current_generation())
            .expect("gen1 is current");
        screened.accept(8);
        assert_eq!(
            third
                .await
                .expect("task ran")
                .expect("commit resolves")
                .revision,
            8
        );
        assert_eq!(kernel.commit_single_flight_rejections(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_an_in_flight_commit_releases_the_slot() {
        let (kernel, sink) = kernel_with_sink();
        let slot = Arc::new(tokio::sync::Semaphore::new(1));
        kernel.current.store(1, Ordering::SeqCst);

        let first = tokio::spawn({
            let kernel = Arc::clone(&kernel);
            let slot = Arc::clone(&slot);
            async move {
                kernel
                    .commit_batch(GenerationId(1), slot, b"first".to_vec())
                    .await
            }
        });
        let held = sink.wait_for_one().await;

        // Cancel the guest side of the commit. The permit must be released by
        // the future's drop even though the host never answered.
        first.abort();
        assert!(first.await.is_err(), "the in-flight commit was dropped");
        drop(held);

        let second = tokio::spawn({
            let kernel = Arc::clone(&kernel);
            let slot = Arc::clone(&slot);
            async move {
                kernel
                    .commit_batch(GenerationId(1), slot, b"second".to_vec())
                    .await
            }
        });
        let second_request = sink.wait_for_one().await;
        let screened = second_request
            .screen(kernel.current_generation())
            .expect("gen1 is current");
        screened.accept(9);
        assert_eq!(
            second
                .await
                .expect("task ran")
                .expect("commit resolves")
                .revision,
            9
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_generation_does_not_share_the_previous_slot() {
        let (kernel, sink) = kernel_with_sink();
        let first_slot = Arc::new(tokio::sync::Semaphore::new(1));
        let second_slot = Arc::new(tokio::sync::Semaphore::new(1));
        kernel.current.store(1, Ordering::SeqCst);

        let _first = tokio::spawn({
            let kernel = Arc::clone(&kernel);
            let slot = Arc::clone(&first_slot);
            async move {
                kernel
                    .commit_batch(GenerationId(1), slot, b"first".to_vec())
                    .await
            }
        });
        let _first_request = sink.wait_for_one().await;

        // Gen1 is still outstanding, but gen2 has its own slot: it must not be
        // refused as "commit in progress" by gen1's commit.
        kernel.current.store(2, Ordering::SeqCst);
        let second = tokio::spawn({
            let kernel = Arc::clone(&kernel);
            let slot = Arc::clone(&second_slot);
            async move {
                kernel
                    .commit_batch(GenerationId(2), slot, b"second".to_vec())
                    .await
            }
        });
        let second_request = sink.wait_for_one().await;
        assert_eq!(second_request.generation(), GenerationId(2));

        let screened = second_request
            .screen(kernel.current_generation())
            .expect("gen2 is current");
        screened.accept(11);
        assert_eq!(
            second
                .await
                .expect("task ran")
                .expect("commit resolves")
                .revision,
            11
        );
        assert_eq!(kernel.commit_single_flight_rejections(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_commits_fail_before_the_single_flight_slot() {
        let (kernel, sink) = kernel_with_sink();
        let current_slot = Arc::new(tokio::sync::Semaphore::new(1));
        kernel.current.store(2, Ordering::SeqCst);

        // Occupy gen2's slot so the ordering is observable: gen1's stale
        // commit must be rejected as stale, not as commit-in-progress.
        let occupied = tokio::spawn({
            let kernel = Arc::clone(&kernel);
            let slot = Arc::clone(&current_slot);
            async move {
                kernel
                    .commit_batch(GenerationId(2), slot, b"current".to_vec())
                    .await
            }
        });
        let held = sink.wait_for_one().await;

        let stale = kernel
            .commit_batch(
                GenerationId(1),
                Arc::new(tokio::sync::Semaphore::new(1)),
                b"stale".to_vec(),
            )
            .await;
        assert!(
            matches!(stale, Err(CommitError::StaleGeneration)),
            "the stale commit must be rejected as stale, got {stale:?}"
        );
        assert_eq!(kernel.stale_commits_rejected(), 1);
        assert_eq!(
            kernel.commit_single_flight_rejections(),
            0,
            "a dead generation must not consume a live generation's slot"
        );

        drop(held);
        occupied.abort();
    }
}
