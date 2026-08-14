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
use crate::text_bridge::{GuestTextBuffer, GuestTextView, MAX_TEXT_ATTACHMENTS, OpaqueResourceKey};

wasmtime::component::bindgen!({
    path: "wit",
    world: "kernel",
    // Only the text capability is async. Its WIT functions are synchronous --
    // the guest blocks inside the call -- while the Rust host method awaits the
    // thread that owns the text subsystem. Making the *default* async would
    // change every other kernel import for no reason.
    imports: {
        default: trappable,
        "instar:text/text": async | trappable,
    },
    with: {
        // Each guest handle is a lease -- two `u32`s naming a host resource --
        // and never the resource itself. See `text_bridge`.
        "instar:text/text.text-buffer": crate::text_bridge::GuestTextBuffer,
        "instar:text/text.text-view": crate::text_bridge::GuestTextView,
    },
});

use instar::kernel::kernel_types::{
    AttachmentError, CommitError, CommitResult, OpError, RuntimeError,
};

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

/// Proof that a commit has passed the two cheap gates.
///
/// Owning this value encodes that the stale and overlap checks have already
/// passed: the generation was current when it was minted and this generation's
/// commit slot was acquired. `commit_batch` therefore needs no gate of its
/// own, and a later edit cannot silently move expensive work (decoding,
/// attachment extraction) ahead of the gate by writing a fresh
/// `commit_batch` call that skips `begin_commit`.
///
/// The type exists for that ordering, not for taste: the semaphore permit is
/// just RAII on its own, and the generation is rechecked separately on the
/// main thread anyway. What the wrapper adds is that the two checks become a
/// single unit a caller either has or does not have.
#[derive(Debug)]
pub struct CommitPermit {
    /// Held for its `Drop`, never read.
    _permit: tokio::sync::OwnedSemaphorePermit,
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
    /// Where text-resource requests go.
    ///
    /// Unlike `commit_sink`, absence has **no** fallback. A headless commit log
    /// is a meaningful thing to read back; a kernel-invented text system is
    /// not, and pretending to create a buffer nobody owns would hand a guest a
    /// capability onto nothing.
    text_sink: OnceLock<Arc<dyn crate::text_bridge::TextSink>>,
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
            text_sink: OnceLock::new(),
        }
    }
}

impl SharedKernel {
    pub fn current_generation(&self) -> GenerationId {
        GenerationId(self.current.load(Ordering::SeqCst))
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

    /// Installs the owner of the text subsystem. Once, before any generation
    /// runs, exactly as [`Self::install_commit_sink`].
    pub fn install_text_sink(
        &self,
        sink: Arc<dyn crate::text_bridge::TextSink>,
    ) -> Result<(), &'static str> {
        self.text_sink
            .set(sink)
            .map_err(|_| "a text sink is already installed")
    }

    pub fn has_text_sink(&self) -> bool {
        self.text_sink.get().is_some()
    }

    /// Marshals one text operation to whoever owns the text subsystem.
    ///
    /// Returns a refusal rather than parking on every failure path: no sink, a
    /// sink that would not take the request, and a reply channel torn down
    /// mid-flight all wake the guest with something it can act on.
    pub async fn submit_text(
        &self,
        generation: GenerationId,
        operation: crate::text_bridge::TextOperation,
    ) -> Result<crate::text_bridge::TextAnswer, crate::text_bridge::TextRefusal> {
        use crate::text_bridge::{TextRefusal, text_request};

        let Some(sink) = self.text_sink.get() else {
            return Err(TextRefusal::HostUnavailable);
        };

        let (request, reply) = text_request(generation, operation);
        if let Err(returned) = sink.submit(request) {
            // Nobody took ownership of answering it, so answering it is this
            // path's job. Dropping it would also answer, via the reply guard,
            // but saying so explicitly is what makes that not an accident.
            returned.refuse(TextRefusal::HostUnavailable);
            return Err(TextRefusal::HostUnavailable);
        }

        match reply.await {
            Ok(verdict) => verdict,
            Err(_) => Err(TextRefusal::HostUnavailable),
        }
    }

    pub fn has_commit_sink(&self) -> bool {
        self.commit_sink.get().is_some()
    }

    /// Runs the two cheap gates every commit must pass before any work is
    /// done on its behalf.
    ///
    /// Order is fixed and load-bearing: a superseded generation is refused
    /// before it can consume its successor's commit slot, and an overlapping
    /// commit is refused before its batch, its attachment count, or any of
    /// its handles can make the host spend work. The returned [`CommitPermit`]
    /// is what makes a later edit unable to run the expensive parts ahead of
    /// these gates.
    fn begin_commit(
        &self,
        generation: GenerationId,
        commit_slot: &Arc<tokio::sync::Semaphore>,
    ) -> Result<CommitPermit, CommitError> {
        if !self.is_current(generation) {
            self.stale_commits_rejected.fetch_add(1, Ordering::SeqCst);
            return Err(CommitError::StaleGeneration);
        }

        // Single-flight gate. The permit is RAII: it is released when the
        // commit future completes, is rejected, is cancelled, or is dropped --
        // so a later sequential commit can always proceed after the first
        // resolves.
        let permit = match Arc::clone(commit_slot).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.commit_in_progress_rejections
                    .fetch_add(1, Ordering::SeqCst);
                return Err(CommitError::CommitInProgress);
            }
        };
        Ok(CommitPermit { _permit: permit })
    }

    /// Answers one guest `commit` call.
    ///
    /// The generation check happens twice on purpose. `begin_commit` already
    /// passed it, so the batch could reach this thread at all; it is
    /// rechecked here because by then the answer may have changed — a
    /// generation can be torn down while its commit is in flight, and
    /// `docs/PHASE-1.md` makes the main thread's arrival check the *first*
    /// thing it does. The permit argument is what keeps this method unable to
    /// run the expensive parts before either gate.
    async fn commit_batch(
        &self,
        _permit: CommitPermit,
        generation: GenerationId,
        text_views: Vec<OpaqueResourceKey>,
        batch: Vec<u8>,
    ) -> Result<CommitResult, CommitError> {
        if !self.is_current(generation) {
            self.stale_commits_rejected.fetch_add(1, Ordering::SeqCst);
            return Err(CommitError::StaleGeneration);
        }

        let Some(sink) = self.commit_sink.get() else {
            // No owner installed: keep the batch here so a headless caller can
            // read back what the guest said. The attachment keys are ignored
            // because a headless commit log has no TextHost to resolve them
            // against — the side table is meaningful only to whoever owns both
            // the tree and the text subsystem, and this path has neither. Note
            // this log is *not* written on the sink path — a host that owns
            // the tree does not need the kernel hoarding every batch it was
            // ever handed.
            let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
            self.commits
                .lock()
                .expect("commit log poisoned")
                .push((generation, batch));
            return Ok(CommitResult { revision });
        };

        let (request, reply) = crate::bridge::commit_request(generation, batch, text_views);
        if sink.submit(request).is_err() {
            // The request came back, so nobody took ownership of answering it.
            // Dropping it here is the answer.
            return Err(CommitError::HostUnavailable);
        }

        match reply.await {
            Ok(Ok(revision)) => Ok(CommitResult { revision }),
            Ok(Err(CommitRejection::Invalid(reason))) => Err(CommitError::InvalidBatch(reason)),
            Ok(Err(CommitRejection::Attachment(refusal))) => {
                Err(CommitError::InvalidAttachment(refusal.into()))
            }
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

impl instar::text::text_types::Host for GenerationState {}

impl From<crate::text_bridge::TextRefusal> for instar::text::text_types::TextError {
    fn from(refusal: crate::text_bridge::TextRefusal) -> Self {
        use crate::text_bridge::TextRefusal;
        match refusal {
            TextRefusal::TooManyBuffers(limit) => Self::TooManyBuffers(limit),
            TextRefusal::TooManyViews(limit) => Self::TooManyViews(limit),
            TextRefusal::NoSuchResource => Self::NoSuchResource,
            // A guest whose generation died while a request was in flight is
            // told the host could not serve it, not that its handle was bad.
            TextRefusal::StaleGeneration | TextRefusal::HostUnavailable => Self::Unavailable,
        }
    }
}

impl From<crate::text_bridge::AttachmentRefusal> for AttachmentError {
    fn from(refusal: crate::text_bridge::AttachmentRefusal) -> Self {
        use crate::text_bridge::AttachmentRefusal;
        match refusal {
            AttachmentRefusal::TooManyAttachments => Self::TooManyAttachments,
            AttachmentRefusal::UnavailableTextView => Self::UnavailableTextView,
            AttachmentRefusal::AttachmentOutOfRange => Self::AttachmentOutOfRange,
            AttachmentRefusal::TextViewAlreadyAttached => Self::TextViewAlreadyAttached,
        }
    }
}

/// The guest's text capabilities.
///
/// Every method here does the same three things: copy the tiny opaque lease
/// out of the resource table, release every Store-backed borrow, and only then
/// await the thread that owns the text subsystem. Nothing derived from the
/// table survives the await — which is the invariant, stated in terms of what
/// can actually go wrong rather than as "no Store access".
impl instar::text::text::Host for GenerationState {
    async fn create_empty_buffer(
        &mut self,
    ) -> wasmtime::Result<Result<Resource<GuestTextBuffer>, instar::text::text_types::TextError>>
    {
        use crate::text_bridge::{TextAnswer, TextOperation};

        let answer = self
            .kernel
            .submit_text(self.generation, TextOperation::CreateBuffer)
            .await;
        let key = match answer {
            Ok(TextAnswer::Created(key)) => key,
            Ok(TextAnswer::Released) => {
                // The sink answered a creation with a release. That is not a
                // refusal a guest can act on; it is the host contradicting
                // itself, which is what traps are for.
                return Err(wasmtime::Error::msg(
                    "text sink answered create-empty-buffer with a release",
                ));
            }
            Err(refusal) => return Ok(Err(refusal.into())),
        };

        // The host resource exists now. If the table will not take a handle to
        // it, the failure that was meant to refuse a resource must not be the
        // one that leaks it.
        match self.table.push(GuestTextBuffer { key }) {
            Ok(handle) => Ok(Ok(handle)),
            Err(error) => {
                let _ = self
                    .kernel
                    .submit_text(self.generation, TextOperation::ReleaseBuffer { key })
                    .await;
                Err(error.into())
            }
        }
    }

    async fn create_view(
        &mut self,
        buffer: Resource<GuestTextBuffer>,
    ) -> wasmtime::Result<Result<Resource<GuestTextView>, instar::text::text_types::TextError>>
    {
        use crate::text_bridge::{TextAnswer, TextOperation};

        // Copied, then the borrow ends. Nothing table-derived crosses the
        // await below.
        let buffer_key = self.table.get(&buffer)?.key;

        let answer = self
            .kernel
            .submit_text(
                self.generation,
                TextOperation::CreateView { buffer: buffer_key },
            )
            .await;
        let key = match answer {
            Ok(TextAnswer::Created(key)) => key,
            Ok(TextAnswer::Released) => {
                return Err(wasmtime::Error::msg(
                    "text sink answered create-view with a release",
                ));
            }
            Err(refusal) => return Ok(Err(refusal.into())),
        };

        match self.table.push(GuestTextView { key }) {
            Ok(handle) => Ok(Ok(handle)),
            Err(error) => {
                let _ = self
                    .kernel
                    .submit_text(self.generation, TextOperation::ReleaseView { key })
                    .await;
                Err(error.into())
            }
        }
    }
}

/// The one text function that suspends on an external event (C2b).
///
/// Separate from [`instar::text::text::Host`] above, and not by choice:
/// declaring `next-edit` an `async func` in the WIT moves it to
/// `HostWithStore` and hands it an `Accessor`, while `create-empty-buffer` and
/// `create-view` stay on `Host` with `&mut self`. That split is the toolchain
/// reporting the distinction the interface intends — bounded calls in one
/// trait, the suspending one in the other — and it is why the `bindgen!`
/// default was **not** broadened to make this compile. Doing that would have
/// migrated every existing text import into a different trait and ABI shape to
/// serve one function.
///
/// `Accessor` only permits Store access inside a synchronous `with` closure,
/// so nothing Store-derived can survive an await. That is the same constraint
/// B2e-3a already works under, and it is exactly compatible with resolving the
/// borrowed handle into a stable `TextBufferId` before sleeping.
impl instar::text::text::HostWithStore<GenerationState>
    for wasmtime::component::HasSelf<GenerationState>
{
    fn next_edit(
        _accessor: &wasmtime::component::Accessor<GenerationState, Self>,
        _buffer: Resource<GuestTextBuffer>,
    ) -> impl std::future::Future<
        Output = wasmtime::Result<
            Result<instar::text::text_types::EditNotification, instar::text::text_types::TextError>,
        >,
    > + Send {
        // C2b-0 is a toolchain proof and nothing else. Delivery -- resolving
        // the borrow, installing the waiter, suspending, and draining -- is
        // C2b-1. Until then the capability genuinely is not available, and
        // saying so is better than a stub that pretends to have answered.
        async move { Ok(Err(instar::text::text_types::TextError::Unavailable)) }
    }
}

/// Explicit guest drops.
///
/// The table entry goes first, so the lease is extracted and every borrow is
/// released before the main thread is asked to let the resource go. Note what
/// is *not* here: any cross-thread work in a Rust `Drop` impl. A destructor
/// doing hidden thread-affine work is the lifecycle coupling this project has
/// spent two phases removing, and it would not run on the path that matters
/// anyway — a trapped generation destroys its Store without the guest dropping
/// anything.
impl instar::text::text::HostTextBuffer for GenerationState {
    async fn drop(&mut self, handle: Resource<GuestTextBuffer>) -> wasmtime::Result<()> {
        use crate::text_bridge::TextOperation;
        let lease = self.table.delete(handle)?;
        let _ = self
            .kernel
            .submit_text(
                self.generation,
                TextOperation::ReleaseBuffer { key: lease.key },
            )
            .await;
        Ok(())
    }
}

impl instar::text::text::HostTextView for GenerationState {
    async fn drop(&mut self, handle: Resource<GuestTextView>) -> wasmtime::Result<()> {
        use crate::text_bridge::TextOperation;
        let lease = self.table.delete(handle)?;
        let _ = self
            .kernel
            .submit_text(
                self.generation,
                TextOperation::ReleaseView { key: lease.key },
            )
            .await;
        Ok(())
    }
}

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
    /// Committing suspends the guest (WP7B1), and now carries a side table of
    /// borrowed text-view handles alongside the batch.
    ///
    /// Ordering is the point (B2e-3a). The generation preflight and the
    /// single-flight gate run first, before anything derived from the table.
    /// Then the attachment-count bound is checked, and only then are the
    /// handles turned into opaque keys. Every table-derived borrow dies inside
    /// the second `accessor.with`; nothing but tiny keys crosses the await,
    /// and a borrowed handle is never `table.delete`d.
    fn commit(
        accessor: &wasmtime::component::Accessor<GenerationState, Self>,
        batch: Vec<u8>,
        text_views: Vec<Resource<GuestTextView>>,
    ) -> impl std::future::Future<Output = wasmtime::Result<Result<CommitResult, CommitError>>> + Send
    {
        Box::pin(commit_impl(accessor, batch, text_views))
    }
}

/// The body of `commit`, as a plain async function so the early refusals can
/// `return` without juggling boxed futures.
async fn commit_impl(
    accessor: &wasmtime::component::Accessor<
        GenerationState,
        wasmtime::component::HasSelf<GenerationState>,
    >,
    batch: Vec<u8>,
    text_views: Vec<Resource<GuestTextView>>,
) -> wasmtime::Result<Result<CommitResult, CommitError>> {
    // O(1). No per-handle work: just the kernel, the generation, and the
    // commit slot that gates it.
    let (kernel, generation, commit_slot) = accessor.with(|mut access| {
        let state = access.get();
        (
            Arc::clone(&state.kernel),
            state.generation,
            Arc::clone(&state.commit_slot),
        )
    });

    let permit = match kernel.begin_commit(generation, &commit_slot) {
        Ok(permit) => permit,
        Err(error) => return Ok(Err(error)),
    };

    // Count before iterating, and before the second accessor entry. The
    // bound is what makes the extraction below bounded work rather than
    // "how large an argument a guest managed to lift".
    if text_views.len() > MAX_TEXT_ATTACHMENTS {
        return Ok(Err(CommitError::InvalidAttachment(
            AttachmentError::TooManyAttachments,
        )));
    }

    // Second entry, now bounded. Every table-derived borrow dies with this
    // closure: copy the tiny opaque key out, and only the keys survive.
    let keys = match accessor.with(|mut access| {
        let table = &access.get().table;
        text_views
            .iter()
            .map(|handle| table.get(handle).map(|lease| lease.key).map_err(Into::into))
            .collect::<wasmtime::Result<Vec<_>>>()
    }) {
        Ok(keys) => keys,
        Err(error) => return Err(error),
    };

    Ok(kernel.commit_batch(permit, generation, keys, batch).await)
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

    /// A generation state with no guest behind it: no component, no event
    /// loop, just the host side of a `commit` call.
    fn inert_state(
        kernel: Arc<SharedKernel>,
        generation: GenerationId,
        commit_slot: Arc<tokio::sync::Semaphore>,
    ) -> GenerationState {
        let (_, events_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE_CAPACITY);
        GenerationState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            kernel,
            generation,
            commit_slot,
            events: Arc::new(tokio::sync::Mutex::new(events_rx)),
            limits: MeasuredLimiter::new(&ResourcePolicy::instar_default(), Arc::default()),
        }
    }

    /// Handles that name nothing: every index is far outside an empty table,
    /// so any code that touches the table on the way to a refusal would trap
    /// instead of answering with the bound verdict.
    fn unresolvable_view_handles(count: usize) -> Vec<Resource<GuestTextView>> {
        (0..count)
            .map(|i| Resource::new_borrow(i as u32 + 10_000))
            .collect()
    }

    /// Drives the real `HostWithStore::commit` implementation, the same path
    /// a guest's call enters, from an inert store.
    async fn call_host_commit(
        state: GenerationState,
        text_views: Vec<Resource<GuestTextView>>,
    ) -> wasmtime::Result<Result<CommitResult, CommitError>> {
        let engine = crate::engine::configured_engine().expect("engine");
        let mut store = Store::new(&engine, state);
        let outcome = store
            .run_concurrent(async move |accessor| {
                <wasmtime::component::HasSelf<GenerationState>
                        as instar::kernel::kernel_ui::HostWithStore<GenerationState>>::commit(
                        accessor,
                        b"batch".to_vec(),
                        text_views,
                    )
                    .await
            })
            .await?;
        outcome
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
                let permit = kernel
                    .begin_commit(GenerationId(1), &slot)
                    .expect("the first commit passes the gates");
                kernel
                    .commit_batch(permit, GenerationId(1), Vec::new(), b"first".to_vec())
                    .await
            }
        });
        let first_request = sink.wait_for_one().await;
        assert_eq!(first_request.generation(), GenerationId(1));

        // The second attempt must fail immediately, before any request is
        // created or enqueued.
        let second = kernel.begin_commit(GenerationId(1), &slot);
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
                let permit = kernel
                    .begin_commit(GenerationId(1), &slot)
                    .expect("the slot is free after the first commit resolves");
                kernel
                    .commit_batch(permit, GenerationId(1), Vec::new(), b"third".to_vec())
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
                let permit = kernel
                    .begin_commit(GenerationId(1), &slot)
                    .expect("the first commit passes the gates");
                kernel
                    .commit_batch(permit, GenerationId(1), Vec::new(), b"first".to_vec())
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
                let permit = kernel
                    .begin_commit(GenerationId(1), &slot)
                    .expect("the slot is released when the first commit is dropped");
                kernel
                    .commit_batch(permit, GenerationId(1), Vec::new(), b"second".to_vec())
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
                let permit = kernel
                    .begin_commit(GenerationId(1), &slot)
                    .expect("gen1 passes the gates");
                kernel
                    .commit_batch(permit, GenerationId(1), Vec::new(), b"first".to_vec())
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
                let permit = kernel
                    .begin_commit(GenerationId(2), &slot)
                    .expect("gen2 is current and has its own free slot");
                kernel
                    .commit_batch(permit, GenerationId(2), Vec::new(), b"second".to_vec())
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
                let permit = kernel
                    .begin_commit(GenerationId(2), &slot)
                    .expect("gen2 is current");
                kernel
                    .commit_batch(permit, GenerationId(2), Vec::new(), b"current".to_vec())
                    .await
            }
        });
        let held = sink.wait_for_one().await;

        let stale = kernel.begin_commit(GenerationId(1), &Arc::new(tokio::sync::Semaphore::new(1)));
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

    /// Ordering, held the way B2e-3a says it must be: a stale generation is
    /// refused before the attachment-count gate can even be reached.
    ///
    /// The commit carries [`MAX_TEXT_ATTACHMENTS`] + 1 handles that are not
    /// in the resource table. If the host counted first, or extracted first,
    /// the answer would be `TooManyAttachments` or a table trap; the
    /// `StaleGeneration` verdict is only possible when the generation
    /// preflight genuinely runs first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_generation_precedes_the_attachment_bound() {
        let kernel = Arc::new(SharedKernel::default());
        kernel.current.store(2, Ordering::SeqCst);

        let result = call_host_commit(
            inert_state(
                Arc::clone(&kernel),
                GenerationId(1),
                Arc::new(tokio::sync::Semaphore::new(1)),
            ),
            unresolvable_view_handles(MAX_TEXT_ATTACHMENTS + 1),
        )
        .await
        .expect("the host method answers with a verdict, not a trap");

        assert!(
            matches!(result, Err(CommitError::StaleGeneration)),
            "the stale generation must win over the oversized side table, got {result:?}"
        );
        assert_eq!(kernel.stale_commits_rejected(), 1);
        assert_eq!(
            kernel.commit_single_flight_rejections(),
            0,
            "a dead generation must not consume a live generation's slot"
        );
    }

    /// Ordering, second gate: an outstanding commit is refused before the
    /// attachment-count gate is reached.
    ///
    /// The slot is held the way an in-flight commit would hold it. The side
    /// table again carries more than [`MAX_TEXT_ATTACHMENTS`] unresolvable
    /// handles; `CommitInProgress` is only possible when the single-flight
    /// gate runs before counting or extracting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_outstanding_commit_precedes_the_attachment_bound() {
        let kernel = Arc::new(SharedKernel::default());
        kernel.current.store(1, Ordering::SeqCst);
        let slot = Arc::new(tokio::sync::Semaphore::new(1));
        let _held = Arc::clone(&slot)
            .try_acquire_owned()
            .expect("the slot is free before the test holds it");

        let result = call_host_commit(
            inert_state(Arc::clone(&kernel), GenerationId(1), slot),
            unresolvable_view_handles(MAX_TEXT_ATTACHMENTS + 1),
        )
        .await
        .expect("the host method answers with a verdict, not a trap");

        assert!(
            matches!(result, Err(CommitError::CommitInProgress)),
            "the outstanding commit must win over the oversized side table, got {result:?}"
        );
        assert_eq!(kernel.commit_single_flight_rejections(), 1);
        assert_eq!(
            kernel.stale_commits_rejected(),
            0,
            "this is a live generation, not a stale one"
        );
    }

    /// Containment: an oversized side table is refused with NO resource-table
    /// access at all.
    ///
    /// Every handle in the table is deliberately unresolvable. A host that
    /// touched the table before the bound check would trap or report a
    /// different refusal; only a count check that runs first can answer
    /// `TooManyAttachments` while every handle still points nowhere.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_oversized_side_table_is_refused_without_table_access() {
        let kernel = Arc::new(SharedKernel::default());
        kernel.current.store(1, Ordering::SeqCst);

        let result = call_host_commit(
            inert_state(
                Arc::clone(&kernel),
                GenerationId(1),
                Arc::new(tokio::sync::Semaphore::new(1)),
            ),
            unresolvable_view_handles(MAX_TEXT_ATTACHMENTS + 1),
        )
        .await
        .expect("the host method answers with a verdict, not a trap");

        assert!(
            matches!(
                result,
                Err(CommitError::InvalidAttachment(
                    AttachmentError::TooManyAttachments
                ))
            ),
            "the count check must refuse before any handle is resolved, got {result:?}"
        );
    }

    /// The bound is exactly that: 4096 entries pass it and reach the commit
    /// path. The handles here are real table entries so the extraction after
    /// the guard can succeed, and the headless log receives the batch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_boundary_count_passes_the_guard() {
        let kernel = Arc::new(SharedKernel::default());
        kernel.current.store(1, Ordering::SeqCst);
        let mut state = inert_state(
            Arc::clone(&kernel),
            GenerationId(1),
            Arc::new(tokio::sync::Semaphore::new(1)),
        );
        let handles = (0..MAX_TEXT_ATTACHMENTS)
            .map(|i| {
                state
                    .table
                    .push(GuestTextView {
                        key: OpaqueResourceKey {
                            slot: i as u32,
                            incarnation: 0,
                        },
                    })
                    .expect("the empty table accepts the test's own leases")
            })
            .collect::<Vec<_>>();

        let result = call_host_commit(state, handles)
            .await
            .expect("the host method answers with a verdict, not a trap");

        assert!(
            matches!(result, Ok(CommitResult { revision: 1 })),
            "4096 entries must pass the guard and reach the headless commit log, got {result:?}"
        );
        assert_eq!(
            kernel.commits().len(),
            1,
            "the headless path records exactly the one commit"
        );
    }
}
