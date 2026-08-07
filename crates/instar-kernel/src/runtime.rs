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
use std::sync::{Arc, Mutex};

use wasmtime::Store;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit",
    world: "kernel",
    imports: { default: trappable },
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

    pub fn live_operations(&self) -> usize {
        self.operations.lock().expect("registry poisoned").len()
    }
}

/// Per-generation store data.
struct GenerationState {
    ctx: WasiCtx,
    table: ResourceTable,
    kernel: Arc<SharedKernel>,
    generation: GenerationId,
    events: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Event>>>,
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

impl instar::kernel::kernel_ui::Host for GenerationState {
    fn commit(&mut self, batch: Vec<u8>) -> wasmtime::Result<Result<CommitResult, CommitError>> {
        // Step 2 of teardown, enforced at the point of effect rather than
        // trusted: a superseded generation's commits are refused, not applied.
        if !self.kernel.is_current(self.generation) {
            self.kernel
                .stale_commits_rejected
                .fetch_add(1, Ordering::SeqCst);
            return Ok(Err(CommitError::StaleGeneration));
        }

        let revision = self.kernel.revision.fetch_add(1, Ordering::SeqCst) + 1;
        self.kernel
            .commits
            .lock()
            .expect("commit log poisoned")
            .push((self.generation, batch));
        Ok(Ok(CommitResult { revision }))
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
    events: tokio::sync::mpsc::UnboundedSender<Event>,
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
    events: tokio::sync::mpsc::UnboundedSender<Event>,
}

impl GenerationHandle {
    pub fn id(&self) -> GenerationId {
        self.id
    }

    /// Queues an event for this generation's guest.
    pub fn send(&self, payload: impl Into<Vec<u8>>) -> Result<(), &'static str> {
        self.events
            .send(Event::Payload(payload.into()))
            .map_err(|_| "generation is no longer receiving events")
    }

    /// Asks the guest to leave its event loop and return from `run`.
    pub fn shutdown(&self) -> Result<(), &'static str> {
        self.events
            .send(Event::Shutdown)
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
}

/// Owns the engine, the shared host state, and the generation sequence.
pub struct Runtime {
    engine: wasmtime::Engine,
    component: Component,
    linker: Linker<GenerationState>,
    kernel: Arc<SharedKernel>,
}

impl Runtime {
    pub fn new(component_bytes: &[u8]) -> wasmtime::Result<Self> {
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
        })
    }

    pub fn kernel(&self) -> Arc<SharedKernel> {
        Arc::clone(&self.kernel)
    }

    /// Creates the next generation: a fresh `Store`, a fresh instance, and a
    /// bumped generation id.
    ///
    /// This is steps 5 and 6 of the teardown sequence, and also how the first
    /// generation is born. Note it takes no state from any previous
    /// generation — that is the whole point.
    pub async fn new_generation(&mut self) -> wasmtime::Result<RuntimeGeneration> {
        let id = GenerationId(self.kernel.current.fetch_add(1, Ordering::SeqCst) + 1);

        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = GenerationState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            kernel: Arc::clone(&self.kernel),
            generation: id,
            events: Arc::new(tokio::sync::Mutex::new(events_rx)),
        };

        let mut store = Store::new(&self.engine, state);
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
