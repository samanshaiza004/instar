//! The runtime/main-thread bridge (WP7B1).
//!
//! ```text
//! MAIN THREAD                       RUNTIME THREAD
//! winit EventLoop                   instar-kernel
//! instar-window        bounded      RuntimeGeneration
//! instar-ui           messages      Wasmtime Store
//! layout/hit-test/render  <----->   guest run_concurrent task
//! ```
//!
//! # Why two threads and not one
//!
//! Winit requires its event loop on the main thread and its `EventLoop` is
//! deliberately neither `Send` nor `Sync`. Wasmtime ships no executor at all
//! and expects the embedder to own polling. Making those two cooperatively
//! share a thread means one of them driving the other's turn-taking, and both
//! are bad at being driven. `EventLoopProxy` is `Send + Sync` and exists
//! precisely so another thread can wake the loop, which is the arrangement
//! this module implements.
//!
//! # Why the guest's commit is async
//!
//! The authoritative retained tree belongs to the main/presentation side. The
//! tempting alternative — `Arc<Mutex<Tree>>`, mutated by the runtime thread —
//! is rejected: it can block the window thread behind a guest, and it leaves
//! nobody clearly owning the interface. Instead the guest's `commit(batch)`
//! suspends while the main thread applies the batch atomically and replies
//! over a one-shot. Suspending a concurrent guest call while the host does
//! work elsewhere is what the Component Model's async support is for, so this
//! is also a genuine proof that host services can marshal onto thread-affine
//! platform owners without blocking the Wasm task.
//!
//! # The ordering on the main thread is normative
//!
//! From `docs/PHASE-1.md`:
//!
//! ```text
//! receive UiCommit
//! -> check RuntimeGeneration      (before anything else)
//! -> only then decode bytes
//! -> validate semantics
//! -> apply atomically
//! -> layout
//! -> lower to PaintScene          (WP7B2)
//! -> request redraw
//! -> reply
//! ```
//!
//! Rejecting a stale generation *before decoding* means a dead guest cannot
//! make the host spend parser and allocation work on its behalf. The type
//! system carries that rule rather than a comment: `CommitRequest` has no
//! accessor for its bytes, and the only way to obtain them is to screen it
//! against the current generation first.
//!
//! The reply comes last, and after layout rather than merely after the tree is
//! swapped: a guest resuming from `commit().await` should mean "the host
//! accepted this as a usable presentation state". Rendering itself need not
//! have happened — only everything that could still have refused the batch.
//!
//! # Queues are bounded, and the winit thread never blocks on them
//!
//! 256 in each direction. Sends from the winit thread are `try_send`: a full
//! queue drops the event and increments a counter, because a window that
//! stops responding to the OS is worse than a click that does not land, and a
//! runtime thread 256 events behind is not going to be rescued by a 257th.
//!
//! The two directions use different channel types, which is not an oversight.
//! Main->runtime is a Tokio channel because the runtime thread `await`s on it
//! inside a `select!` against the guest. Runtime->main is a `std` sync channel
//! because the main thread has no async runtime and must not acquire one:
//! `recv_timeout` parks the thread properly, where a Tokio receiver would
//! leave it spinning.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use instar_kernel::bridge::{CommitRejection, CommitRequest, CommitSink, GuestEvent};
use instar_kernel::runtime::{GenerationId, Runtime};
use instar_kernel::text_bridge::{AttachmentRefusal, TextRequest, TextSink};
use instar_ui::DecodedUiSnapshot;
use instar_window::{WindowId, WindowOutput};
use tokio::sync::mpsc;

use crate::{Host, HostEffect};

/// How many messages may be in flight in either direction.
///
/// The main->runtime queue is the one that can genuinely fill: a user can
/// generate input faster than a busy guest drains it. Runtime->main is bounded
/// by the same number for symmetry, but its natural ceiling is far lower — a
/// guest task has at most one commit outstanding, because it is suspended on
/// the reply.
///
/// The guest's own inbox is bounded too, at
/// [`instar_kernel::runtime::EVENT_QUEUE_CAPACITY`]. Both bounds are needed:
/// this one stops the runtime thread falling behind the window, and that one
/// stops the guest falling behind the runtime thread. Bounding only the first
/// would relocate a backlog rather than refuse it.
pub const QUEUE_CAPACITY: usize = 256;

/// How long a shutdown waits for the guest to leave its event loop before the
/// generation is destroyed out from under it.
///
/// There is a case where waiting forever would hang: a guest suspended inside
/// `commit` whose reply never comes because the main thread is already gone.
/// Dropping the `Store` is the documented way to reclaim a suspended guest
/// task, and it is what this falls back to.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1_000);

/// Main thread -> runtime thread.
#[derive(Debug)]
pub enum RuntimeCommand {
    /// Wake the guest with an event — in practice an encoded [`instar_ui::UiAction`].
    DeliverEvent(GuestEvent),
    /// Cancel one in-flight host operation. Per-operation: the guest task
    /// stays alive, which is the distinction `docs/PHASE-1.md` draws between
    /// this and destroying a generation.
    CancelOperation(u64),
    /// Ask the guest to leave its event loop, then end the generation —
    /// *after* everything already queued ahead of it.
    ///
    /// [`RuntimeThread::shutdown`] does not use this. A shutdown that travels
    /// on a bounded queue can be dropped by the same back-pressure it is
    /// trying to escape, and a shutdown that can be dropped is not one; so the
    /// real path is an out-of-band signal that jumps the queue. This variant
    /// remains for the ordered case, where a caller genuinely wants the guest
    /// to see its pending events first.
    Shutdown,
}

/// Runtime thread -> main thread.
///
/// Named for winit's `EventLoop::with_user_event`, which is how these reach a
/// loop parked in `ControlFlow::Wait`.
#[derive(Debug)]
pub enum HostUserEvent {
    /// The guest committed an interface and is suspended awaiting the verdict.
    UiCommit {
        generation: GenerationId,
        request: CommitRequest,
    },
    /// The guest asked for a text resource and is suspended awaiting it.
    ///
    /// Ordinary work, deliberately on the same bounded queue as commits rather
    /// than a text-specific one: creating and dropping a resource is guest
    /// work like any other, and a second queue would mean a second answer to
    /// back-pressure and a second pump budget to keep in step. Terminal
    /// lifecycle stays on the out-of-band path, so `GuestGone` still outranks
    /// every queued text request.
    TextRequest {
        generation: GenerationId,
        request: TextRequest,
    },
}

/// How a guest generation ended.
///
/// This is terminal state, not ordinary work: it cannot compete with commits
/// for queue capacity, cannot be dropped by back-pressure, and is observed
/// exactly once. The runtime thread stores it in a dedicated single slot that
/// the main thread drains before touching any ordinary `UiCommit` work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutcome {
    /// The guest trapped, or returned an error from `run`.
    GuestTrapped {
        generation: GenerationId,
        error: String,
    },
    /// The guest's `run` returned cleanly.
    GuestExited { generation: GenerationId },
}

/// Wakes a main thread that is parked waiting for OS events.
///
/// In the windowed build this is `move || { let _ = proxy.send_event(()); }`.
/// It is a callback rather than a winit type because nothing in this crate
/// should need a display server to be tested, and because the queue above
/// carries the payload — the wake only has to make the loop look.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// The runtime thread's end of the runtime->main queue, and the kernel's
/// [`CommitSink`].
struct MainThreadSink {
    events: SyncSender<HostUserEvent>,
    wake: Wake,
    dropped: Arc<AtomicU64>,
}

/// The main-thread side of the bridge, as handed to the runtime thread.
struct MainThreadChannels {
    events: SyncSender<HostUserEvent>,
    terminal: Arc<Mutex<Option<TerminalOutcome>>>,
    dropped_commits: Arc<AtomicU64>,
    wake: Wake,
}

impl MainThreadSink {
    /// Non-blocking on purpose in both directions. A full runtime->main queue
    /// means the main thread has stopped draining, and in that state the guest
    /// is better told its commit will not be answered than parked on it.
    fn emit(&self, event: HostUserEvent) -> Result<(), HostUserEvent> {
        match self.events.try_send(event) {
            Ok(()) => {
                (self.wake)();
                Ok(())
            }
            Err(TrySendError::Full(event)) | Err(TrySendError::Disconnected(event)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Err(event)
            }
        }
    }
}

impl TextSink for MainThreadSink {
    /// Non-blocking, exactly as commits are. A full or disconnected queue
    /// means the main thread has stopped draining, and in that state a guest
    /// is better told its request will not be answered than parked on it.
    fn submit(&self, request: TextRequest) -> Result<(), TextRequest> {
        let generation = request.generation();
        match self.emit(HostUserEvent::TextRequest {
            generation,
            request,
        }) {
            Ok(()) => Ok(()),
            Err(HostUserEvent::TextRequest { request, .. }) => Err(request),
            Err(_) => unreachable!("emit returns the event it was given"),
        }
    }
}

impl CommitSink for MainThreadSink {
    fn submit(&self, request: CommitRequest) -> Result<(), CommitRequest> {
        let generation = request.generation();
        match self.emit(HostUserEvent::UiCommit {
            generation,
            request,
        }) {
            Ok(()) => Ok(()),
            // Hand the request back rather than dropping it here: the kernel
            // is the one that knows how to turn "nobody took this" into a
            // verdict the guest can act on.
            Err(HostUserEvent::UiCommit { request, .. }) => Err(request),
            Err(_) => unreachable!("emit returns the event it was given"),
        }
    }
}

/// How the guest generation ended.
#[derive(Debug)]
enum Ending {
    Exited,
    Trapped(String),
}

/// What the runtime thread hands back once its guest exists.
struct Started {
    generation: GenerationId,
    kernel: Arc<instar_kernel::runtime::SharedKernel>,
    /// The engine the guest runs on, so the main thread can increment its
    /// epoch as the out-of-band half of shutdown.
    engine: wasmtime::Engine,
}

/// Owns the guest: one thread, one Tokio runtime, one generation at a time.
///
/// Dropping this asks the guest to shut down and joins the thread, so a test
/// that forgets to call [`RuntimeThread::shutdown`] still cannot leave a
/// generation running.
pub struct RuntimeThread {
    commands: mpsc::Sender<RuntimeCommand>,
    /// The out-of-band stop signal. Deliberately not on the command queue: see
    /// [`RuntimeCommand::Shutdown`].
    stop: Arc<tokio::sync::Notify>,
    join: Option<std::thread::JoinHandle<()>>,
    generation: GenerationId,
    /// Shared with the runtime thread purely so the main thread can *observe*
    /// it — operation counts, stale-commit counts. Nothing here mutates guest
    /// state through it; that all goes over the command queue.
    kernel: Arc<instar_kernel::runtime::SharedKernel>,
    dropped_commands: u64,
    /// Commits the runtime thread could not queue because the runtime->main
    /// queue was full or the main thread was gone.
    dropped_commits: Arc<AtomicU64>,
    /// The engine hosting the guest, retained for shutdown's epoch increment.
    engine: wasmtime::Engine,
}

impl RuntimeThread {
    /// Starts a guest and blocks until its first generation exists.
    ///
    /// Blocking here is deliberate and happens once, before the window is
    /// interactive: the main thread needs a generation id before it can screen
    /// a single commit, and inventing one it has not confirmed would defeat
    /// the check.
    pub fn spawn(
        component: Vec<u8>,
        events: SyncSender<HostUserEvent>,
        terminal: Arc<Mutex<Option<TerminalOutcome>>>,
        dropped_commits: Arc<AtomicU64>,
        wake: Wake,
    ) -> Result<Self, String> {
        let (commands_tx, commands_rx) = mpsc::channel(QUEUE_CAPACITY);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(tokio::sync::Notify::new());
        let thread_stop = Arc::clone(&stop);
        let thread_dropped_commits = Arc::clone(&dropped_commits);
        let channels = MainThreadChannels {
            events,
            terminal,
            dropped_commits,
            wake,
        };

        let join = std::thread::Builder::new()
            .name("instar-runtime".to_string())
            .spawn(move || run_thread(component, commands_rx, thread_stop, channels, started_tx))
            .map_err(|error| format!("could not start the runtime thread: {error}"))?;

        match started_rx.recv() {
            Ok(Ok(Started {
                generation,
                kernel,
                engine,
            })) => Ok(Self {
                commands: commands_tx,
                stop,
                join: Some(join),
                generation,
                kernel,
                dropped_commands: 0,
                dropped_commits: thread_dropped_commits,
                engine,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("the runtime thread died before it started a guest".to_string()),
        }
    }

    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    /// Host operations still in flight for the guest.
    pub fn live_operations(&self) -> usize {
        self.kernel.live_operations()
    }

    /// Commits refused because another commit from the same generation was
    /// already outstanding.
    pub fn commit_single_flight_rejections(&self) -> u64 {
        self.kernel.commit_single_flight_rejections()
    }

    /// Commands queued for the runtime thread but not yet taken.
    pub fn queued_commands(&self) -> usize {
        self.commands
            .max_capacity()
            .saturating_sub(self.commands.capacity())
    }

    /// Queues a command, never blocking.
    ///
    /// Returns whether it was queued. A `false` here is the winit thread
    /// declining to wait for a wedged runtime, and is counted rather than
    /// escalated.
    pub fn send(&mut self, command: RuntimeCommand) -> bool {
        match self.commands.try_send(command) {
            Ok(()) => true,
            Err(_) => {
                self.dropped_commands += 1;
                false
            }
        }
    }

    /// Commands dropped because the queue was full or the thread was gone.
    pub fn dropped_commands(&self) -> u64 {
        self.dropped_commands
    }

    /// Commits dropped because the runtime->main queue was full or the main
    /// thread was gone.
    pub fn dropped_commits(&self) -> u64 {
        self.dropped_commits.load(Ordering::Relaxed)
    }

    /// Asks the guest to stop and waits for the thread.
    ///
    /// The cooperative signal jumps the command queue, because the state most
    /// in need of shutting down is exactly the state where that queue is full.
    /// The epoch increment is the second, independent half: it interrupts a
    /// guest executing non-yielding Wasm that will never look at the queue.
    /// Together they bound shutdown for executing Wasm (epoch trap), parked
    /// guests (cooperative shutdown), and guests suspended inside host
    /// operations (the runtime's own `SHUTDOWN_GRACE`).
    pub fn shutdown(&mut self) {
        self.stop.notify_one();
        self.engine.increment_epoch();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for RuntimeThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeThread")
            .field("generation", &self.generation)
            .field("dropped_commands", &self.dropped_commands)
            .field("dropped_commits", &self.dropped_commits())
            .finish_non_exhaustive()
    }
}

/// The runtime thread's whole life.
fn run_thread(
    component: Vec<u8>,
    mut commands: mpsc::Receiver<RuntimeCommand>,
    stop: Arc<tokio::sync::Notify>,
    channels: MainThreadChannels,
    started: std::sync::mpsc::Sender<Result<Started, String>>,
) {
    // Current-thread on purpose. This thread exists to drive one guest and the
    // host operations it starts; a work-stealing pool would add scheduling
    // hops to the very round-trip WP7B1 is measuring, for a workload with no
    // parallelism in it.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = started.send(Err(format!("could not build a Tokio runtime: {error}")));
            return;
        }
    };

    let sink = Arc::new(MainThreadSink {
        events: channels.events,
        wake: channels.wake,
        dropped: channels.dropped_commits,
    });

    runtime.block_on(async move {
        let mut runtime = match Runtime::new(&component) {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = started.send(Err(format!("component did not load: {error}")));
                return;
            }
        };

        let kernel = runtime.kernel();
        if let Err(error) = kernel.install_commit_sink(sink.clone()) {
            let _ = started.send(Err(error.to_string()));
            return;
        }
        // Before the component can run, so a guest never finds the capability
        // missing. Absence would be an immediate refusal rather than a hang,
        // but a guest that has to handle a service the host simply forgot to
        // install is a guest handling a bug.
        if let Err(error) = kernel.install_text_sink(sink.clone()) {
            let _ = started.send(Err(error.to_string()));
            return;
        }

        let mut generation = match runtime.new_generation().await {
            Ok(generation) => generation,
            Err(error) => {
                let _ = started.send(Err(format!("guest did not instantiate: {error}")));
                return;
            }
        };
        let id = generation.id();
        let handle = generation.handle();
        let started = started.send(Ok(Started {
            generation: id,
            kernel: Arc::clone(&kernel),
            engine: runtime.engine(),
        }));
        if started.is_err() {
            return;
        }

        let ending = {
            let run = std::pin::pin!(generation.run());
            drive(run, &mut commands, &stop, &handle, &kernel, id).await
        };

        // The run future is dropped by now, so the generation is ours again.
        // Destroying it cancels the guest's host-owned operations and drops
        // the Store, which is the only supported way to reclaim a suspended
        // guest task.
        runtime.destroy_generation(generation);

        // Terminal state lives outside the bounded queue on purpose. The main
        // thread observes it exactly once, before any queued commit, and it
        // cannot be dropped by a saturated work queue. The wake is what makes
        // a parked winit loop look.
        {
            let mut slot = channels.terminal.lock().expect("terminal slot poisoned");
            *slot = Some(match ending {
                Ending::Exited => TerminalOutcome::GuestExited { generation: id },
                Ending::Trapped(error) => TerminalOutcome::GuestTrapped {
                    generation: id,
                    error,
                },
            });
        }
        (sink.wake)();
    });
}

/// Runs the guest alongside the command queue until one of them ends it.
async fn drive<F>(
    mut run: std::pin::Pin<&mut F>,
    commands: &mut mpsc::Receiver<RuntimeCommand>,
    stop: &tokio::sync::Notify,
    handle: &instar_kernel::runtime::GenerationHandle,
    kernel: &Arc<instar_kernel::runtime::SharedKernel>,
    id: GenerationId,
) -> Ending
where
    F: std::future::Future<Output = wasmtime::Result<Result<(), String>>>,
{
    // An event taken off the command queue that the guest's inbox had no room
    // for. While one is held here nothing further is dequeued, which is what
    // makes the bound above actually bind: forwarding eagerly would drain the
    // command queue however far behind the guest was, and the backlog would
    // simply move one layer down, bounded in name only.
    let mut undelivered: Option<Vec<u8>> = None;

    loop {
        // `select!` keeps its other branches' futures alive across the arm
        // bodies, so nothing here may move `run`. Each turn reports what it
        // decided and the loop acts on it afterwards.
        let turn = if let Some(bytes) = undelivered.take() {
            // Raced against the guest, because the guest is the only thing
            // that can make room: parking on the permit without polling `run`
            // would deadlock the two against each other.
            tokio::select! {
                biased;
                result = run.as_mut() => Turn::Finished(ending_of(result)),
                () = stop.notified() => Turn::Stop,
                permit = handle.reserve() => match permit {
                    Ok(permit) => {
                        permit.send(bytes);
                        Turn::Continue
                    }
                    // The guest is no longer receiving; nothing left to drive.
                    Err(_) => Turn::Finished(Ending::Exited),
                },
            }
        } else {
            tokio::select! {
                // Biased so a guest that has already finished is noticed
                // before another command is pulled off the queue and
                // delivered into the void, and so a stop signal is never
                // starved by a busy queue.
                biased;
                result = run.as_mut() => Turn::Finished(ending_of(result)),
                () = stop.notified() => Turn::Stop,
                command = commands.recv() => match command {
                    Some(RuntimeCommand::DeliverEvent(event)) => {
                        Turn::Deliver(event.into_bytes())
                    }
                    // Deliberately not held behind the guest's inbox:
                    // cancelling work is most useful precisely when things are
                    // backed up.
                    Some(RuntimeCommand::CancelOperation(operation)) => {
                        kernel.cancel_operation(id, operation);
                        Turn::Continue
                    }
                    // A closed queue means the main thread is gone, which is
                    // the same instruction as an explicit shutdown.
                    Some(RuntimeCommand::Shutdown) | None => Turn::Stop,
                },
            }
        };

        match turn {
            Turn::Continue => {}
            Turn::Deliver(bytes) => undelivered = Some(bytes),
            Turn::Stop => return stop_guest(run, handle).await,
            Turn::Finished(ending) => return ending,
        }
    }
}

/// What one turn of the drive loop decided.
enum Turn {
    Continue,
    /// An event was taken off the command queue but has nowhere to go yet.
    Deliver(Vec<u8>),
    Stop,
    Finished(Ending),
}

/// Ends a generation: ask, wait a bounded while, then let the caller drop the
/// `Store`.
async fn stop_guest<F>(
    mut run: std::pin::Pin<&mut F>,
    handle: &instar_kernel::runtime::GenerationHandle,
) -> Ending
where
    F: std::future::Future<Output = wasmtime::Result<Result<(), String>>>,
{
    // Ask nicely, if there is room to ask. A guest so far behind that its
    // inbox is full will not hear this, and does not need to: the fallback
    // below ends it either way.
    let _ = handle.shutdown();

    match tokio::time::timeout(SHUTDOWN_GRACE, run.as_mut()).await {
        Ok(result) => ending_of(result),
        // The guest did not come back — most likely suspended inside a commit
        // nobody will answer. Returning here drops the run future, and the
        // caller then drops the `Store`, which is what actually ends it.
        Err(_) => Ending::Exited,
    }
}

fn ending_of(result: wasmtime::Result<Result<(), String>>) -> Ending {
    match result {
        Ok(Ok(())) => Ending::Exited,
        Ok(Err(message)) => Ending::Trapped(message),
        Err(error) => Ending::Trapped(format!("{error:#}")),
    }
}

/// Counters the acceptance gate asserts on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BridgeStats {
    /// Commits refused because their generation was no longer current. Refused
    /// *before* decoding — that is the point of counting them here.
    pub stale_commits: u64,
    /// Text requests refused because their generation was already gone.
    pub stale_text_requests: u64,
    /// Commits refused by the attachment gates: an unresolvable key, a slot
    /// outside the side table, or two live nodes naming one text view.
    ///
    /// Kept apart from [`BridgeStats::rejected_commits`], which counts the
    /// decode/validate/diff refusals, because the attachment gates have their
    /// own place in the normative order and their own counter keeps a
    /// regression in that order visible as a counter change.
    pub attachment_refusals: u64,
    /// Commits that decoded but did not validate, or did not decode.
    pub rejected_commits: u64,
    /// Commits applied.
    pub applied_commits: u64,
    /// Events the winit thread could not queue because the runtime was behind.
    pub dropped_commands: u64,
    /// Commits the runtime thread could not queue because the runtime->main
    /// queue was full or the main thread was gone. Terminal outcomes are not
    /// work and are never counted here.
    pub dropped_commits: u64,
}

/// The main thread's half of the bridge.
///
/// Wraps a [`Host`] and adds the two things the routing core deliberately does
/// not have: a generation to screen against, and somewhere to send guest
/// events. Everything here is synchronous, because the thread it runs on is
/// winit's and must stay free to answer the OS.
pub struct HostBridge {
    host: Host,
    /// The generation whose commits are currently acceptable.
    ///
    /// Set to `GenerationId(0)` once the guest is gone. Zero is never a live
    /// generation — the kernel's first is `gen1` — so "no guest" needs no
    /// extra flag and screens exactly like any other stale generation.
    generation: GenerationId,
    /// The window a guest commit describes. One guest, one window, in Phase 1.
    window: WindowId,
    runtime: RuntimeThread,
    events: std::sync::mpsc::Receiver<HostUserEvent>,
    /// The terminal slot shared with the runtime thread. A guest generation
    /// stores at most one outcome here, outside the bounded work queue.
    terminal: Arc<Mutex<Option<TerminalOutcome>>>,
    /// Wake callback retained so a bounded pump can arrange another pass when
    /// it stops with ordinary work still queued.
    wake: Wake,
    /// Accepted guest commits, in order.
    ///
    /// The counter the guest sees: `screened.accept(...)` is handed this value,
    /// so it advances on every accepted commit even when the snapshot was
    /// identical to the retained one. That monotonicity is the guest's sync
    /// contract, and it must not be tied to whether the host found the tree
    /// interesting.
    commit_sequence: u64,
    /// Host-to-guest interaction messages successfully queued by this bridge.
    /// This is diagnostic state for the internal real-runtime test harness;
    /// it is not part of the guest protocol.
    guest_message_count: u64,
    stats: BridgeStats,
}

impl HostBridge {
    /// Starts a guest and returns the main thread's side of the bridge.
    ///
    /// `wake` is called whenever something is queued for the main thread; pass
    /// a closure over an `EventLoopProxy` in a windowed build, and anything at
    /// all in a headless one, since [`HostBridge::wait`] blocks on the queue
    /// itself.
    pub fn spawn(component: Vec<u8>, window: WindowId, wake: Wake) -> Result<Self, String> {
        Self::start(Host::new(), component, window, wake)
    }

    /// As [`HostBridge::spawn`], with the shipped monospace face registered
    /// with Parley's font context before the guest can commit anything.
    pub fn spawn_with_monospace_face(
        component: Vec<u8>,
        window: WindowId,
        wake: Wake,
        face: Arc<[u8]>,
    ) -> Result<Self, String> {
        Self::start(Host::with_monospace_face(face), component, window, wake)
    }

    fn start(host: Host, component: Vec<u8>, window: WindowId, wake: Wake) -> Result<Self, String> {
        let (events_tx, events_rx) = std::sync::mpsc::sync_channel(QUEUE_CAPACITY);
        let terminal: Arc<Mutex<Option<TerminalOutcome>>> = Arc::default();
        let dropped_commits = Arc::new(AtomicU64::new(0));
        let bridge_wake = Arc::clone(&wake);
        let runtime = RuntimeThread::spawn(
            component,
            events_tx,
            Arc::clone(&terminal),
            dropped_commits,
            wake,
        )?;
        Ok(Self {
            host,
            generation: runtime.generation(),
            window,
            runtime,
            events: events_rx,
            terminal,
            wake: bridge_wake,
            commit_sequence: 0,
            guest_message_count: 0,
            stats: BridgeStats::default(),
        })
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    pub fn window(&self) -> WindowId {
        self.window
    }

    /// The generation whose commits are currently acceptable, or
    /// `GenerationId(0)` if the guest is gone.
    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    pub fn stats(&self) -> BridgeStats {
        BridgeStats {
            dropped_commands: self.runtime.dropped_commands(),
            dropped_commits: self.runtime.dropped_commits(),
            ..self.stats
        }
    }

    /// Commits refused by the kernel because another commit from the same
    /// generation was already outstanding.
    pub fn commit_single_flight_rejections(&self) -> u64 {
        self.runtime.commit_single_flight_rejections()
    }

    /// The sequence number of the last accepted guest commit.
    ///
    /// The guest sees this counter: every accepted commit advances it, no-op or
    /// not, so guest synchronization is unaffected by whether the host found
    /// the snapshot interesting. [`Self::tree_revision`] is the separate host
    /// value that layout, paint, and accessibility caches key off.
    pub fn commit_sequence(&self) -> u64 {
        self.commit_sequence
    }

    /// The number of host-to-guest interaction messages successfully queued.
    ///
    /// This deliberately counts after host policy and routing have run, so a
    /// disabled or otherwise rejected interaction does not look like a guest
    /// message merely because a platform event arrived.
    pub fn guest_message_count(&self) -> u64 {
        self.guest_message_count
    }

    /// The version of the retained UI state this bridge's window is showing.
    ///
    /// Forwards to the host's tree revision for this window. It advances only
    /// when the diff found something, so an identical re-commit does not claim
    /// a new tree exists. This is the value layout, paint, and accessibility
    /// caches key off; [`Self::commit_sequence`] is the guest-visible one.
    pub fn tree_revision(&self) -> u64 {
        self.host
            .window(self.window)
            .map(|window| window.tree_revision())
            .unwrap_or(0)
    }

    /// Host operations still in flight for the guest.
    pub fn live_operations(&self) -> usize {
        self.runtime.live_operations()
    }

    /// Commands queued for the runtime thread but not yet taken.
    pub fn queued_commands(&self) -> usize {
        self.runtime.queued_commands()
    }

    /// Routes one window event.
    ///
    /// [`HostEffect::SendToGuest`] is consumed here — it becomes a
    /// [`RuntimeCommand::DeliverEvent`] — so what comes back is only what the
    /// caller still has to do. The send is non-blocking: winit's thread waits
    /// for nothing.
    pub fn on_window_event(&mut self, event: WindowOutput) -> Vec<HostEffect> {
        let effects = self.host.handle(event);
        self.consume_guest_sends(effects)
    }

    /// Turns every [`HostEffect::SendToGuest`] into a queued runtime command,
    /// leaving only what the caller still has to do.
    fn consume_guest_sends(&mut self, effects: Vec<HostEffect>) -> Vec<HostEffect> {
        effects
            .into_iter()
            .filter(|effect| match effect {
                HostEffect::SendToGuest(bytes) => {
                    if self
                        .runtime
                        .send(RuntimeCommand::DeliverEvent(GuestEvent::new(bytes.clone())))
                    {
                        self.guest_message_count += 1;
                    }
                    false
                }
                _ => true,
            })
            .collect()
    }

    /// Chooses where scrollbars sit. See [`Host::set_scrollbar_style`].
    pub fn set_scrollbar_style(&mut self, scrollbars: instar_ui::ScrollbarStyle) {
        self.host.set_scrollbar_style(scrollbars);
    }

    /// Routes one accessibility action, on the main thread.
    ///
    /// Filters [`HostEffect::SendToGuest`] exactly as
    /// [`Self::on_window_event`] does -- an activation arriving from an
    /// assistive technology reaches the guest by the same route a click does,
    /// because by this point they are the same intent.
    pub fn on_accessibility_action(
        &mut self,
        action: accesskit::Action,
        target: accesskit::NodeId,
    ) -> Vec<HostEffect> {
        let window = self.window;
        let effects = self.host.on_accessibility_action(window, action, target);
        self.consume_guest_sends(effects)
    }

    /// What the platform accessibility adapter has not yet been told.
    ///
    /// Calling this *drains*: what it returns is not offered again. The caller
    /// must therefore only ask when something is listening, or the update is
    /// lost. See [`Self::full_accessibility_tree`] for the way back.
    pub fn accessibility_update(&mut self) -> Option<accesskit::TreeUpdate> {
        let window = self.window;
        self.host.accessibility_update(window)
    }

    /// The whole tree, as the platform requires on first activation.
    ///
    /// An adapter that has just attached knows nothing, so an incremental
    /// update would describe changes to a tree it does not have.
    pub fn full_accessibility_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        let window = self.window;
        self.host.reset_accessibility(window);
        self.host.accessibility_update(window)
    }

    /// Asks the runtime to cancel one in-flight operation.
    pub fn cancel_operation(&mut self, operation: u64) -> bool {
        self.runtime
            .send(RuntimeCommand::CancelOperation(operation))
    }

    /// How many ordinary messages one pump call may process before yielding.
    ///
    /// Together with [`PUMP_TIME_BUDGET`] this keeps a winit user-event turn
    /// bounded even when the guest has queued a large backlog. The item bound
    /// is deterministic; the time bound is a short ceiling for expensive items.
    pub const PUMP_ITEM_BUDGET: usize = 64;

    /// The elapsed-time budget for one pump call. Once it is spent, the pump
    /// arranges another wake and returns; a winit event-loop turn never
    /// becomes a hidden batch job.
    pub const PUMP_TIME_BUDGET: Duration = Duration::from_millis(1);

    /// Drains up to one pump budget of runtime thread work, without blocking.
    ///
    /// This is what a winit `user_event` handler calls: the proxy only says
    /// *that* something arrived, and the queue says what. Terminal state is
    /// observed before any ordinary work; if the budget stops the pass with
    /// ordinary work still queued, the retained wake arranges another pass.
    pub fn pump(&mut self) -> Vec<HostEffect> {
        self.pump_bounded(Self::PUMP_ITEM_BUDGET, Self::PUMP_TIME_BUDGET)
    }

    /// The budgeted pump, parameterised so deterministic tests can force the
    /// item or time bound without waiting on wall-clock luck.
    fn pump_bounded(&mut self, item_budget: usize, time_budget: Duration) -> Vec<HostEffect> {
        let mut effects = Vec::new();
        let deadline = Instant::now() + time_budget;
        let mut processed = 0usize;

        loop {
            // Terminal first, on every turn. A generation that has ended makes
            // every still-queued commit unacceptable, so the outcome must be
            // observed before another byte of ordinary work is touched.
            let outcome = self.terminal.lock().expect("terminal slot poisoned").take();
            if let Some(outcome) = outcome {
                effects.extend(self.on_terminal(outcome));
                // Retirement changes the generation to zero. Continue through
                // the ordinary budget so queued commits are screened stale,
                // without turning terminal handling into an unbounded drain.
                continue;
            }

            match self.events.try_recv() {
                Ok(event) => {
                    effects.extend(self.on_user_event(event));
                    processed += 1;
                    if processed >= item_budget || Instant::now() >= deadline {
                        // A continuation wake is harmless if the queue happens
                        // to be empty exactly here, and necessary whenever it
                        // is not: winit must not block waiting for a pass the
                        // main thread was asked to arrange itself.
                        (self.wake)();
                        return effects;
                    }
                }
                Err(_) => return effects,
            }
        }
    }

    /// Parks until the runtime thread queues something, or `timeout` passes,
    /// then drains.
    ///
    /// Models `ControlFlow::Wait` for headless drivers and tests, and parks the
    /// thread rather than spinning on it — the same property Instar's premise
    /// demands of an idle guest applies to an idle host. A windowed build never
    /// calls this: winit owns the parking there, and the proxy wake is what
    /// ends it.
    pub fn wait(&mut self, timeout: Duration) -> Vec<HostEffect> {
        if self
            .terminal
            .lock()
            .expect("terminal slot poisoned")
            .is_some()
        {
            return self.pump();
        }
        let first = match self.events.recv_timeout(timeout) {
            Ok(event) => event,
            // Nothing ordinary arrived. Terminal may have been stored while we
            // were parked, and pump observes it first.
            Err(_) => return self.pump(),
        };

        // The terminal slot is checked again after the receive: a terminal
        // outcome may have arrived before this queued commit was drained, and
        // in that case the commit must be refused, not applied.
        let mut effects = Vec::new();
        let outcome = self.terminal.lock().expect("terminal slot poisoned").take();
        if let Some(outcome) = outcome {
            effects.extend(self.on_terminal(outcome));
            effects.extend(self.on_user_event(first));
            effects.extend(self.pump());
            return effects;
        }

        effects.extend(self.on_user_event(first));
        effects.extend(self.pump());
        effects
    }

    /// Stores a terminal outcome as if the runtime thread had just reported
    /// it. The next pump observes it exactly once, before ordinary work.
    pub fn report_terminal(&mut self, outcome: TerminalOutcome) {
        *self.terminal.lock().expect("terminal slot poisoned") = Some(outcome);
    }

    /// Applies one message from the runtime thread, in the order
    /// `docs/PHASE-1.md` makes normative.
    ///
    /// Public because it is the whole substance of the main thread's
    /// obligations, and worth driving directly — with a request built by
    /// [`instar_kernel::bridge::commit_request`] — rather than only through a
    /// live guest that has to be coaxed into misbehaving.
    pub fn on_user_event(&mut self, event: HostUserEvent) -> Vec<HostEffect> {
        match event {
            HostUserEvent::UiCommit { request, .. } => self.on_ui_commit(request),
            HostUserEvent::TextRequest { request, .. } => self.on_text_request(request),
        }
    }

    /// Applies one terminal outcome exactly once.
    fn on_terminal(&mut self, outcome: TerminalOutcome) -> Vec<HostEffect> {
        match outcome {
            TerminalOutcome::GuestTrapped { generation, error } => {
                self.retire(generation);
                // Presentation first, so the `GuestGone` the caller receives is
                // already accompanied by the frame that shows it.
                let mut effects =
                    self.host
                        .on_guest_gone(self.window, generation, Some(error.clone()));
                effects.push(HostEffect::GuestGone {
                    generation,
                    error: Some(error),
                });
                effects
            }
            TerminalOutcome::GuestExited { generation } => {
                self.retire(generation);
                let mut effects = self.host.on_guest_gone(self.window, generation, None);
                effects.push(HostEffect::GuestGone {
                    generation,
                    error: None,
                });
                effects
            }
        }
    }

    /// The guest is gone. Nothing it committed may be applied from here on,
    /// including anything already sitting in the queue.
    fn retire(&mut self, generation: GenerationId) {
        if self.generation == generation {
            self.generation = GenerationId(0);
        }
    }

    /// Serves one text request, generation first.
    ///
    /// Same ordering rule as a commit and for the same reason: a superseded
    /// generation does not get to allocate host resources on its way out, and
    /// `TextRequest` has no way to show its operation until it has been
    /// screened. A stale request is answered by `screen` itself, so it can
    /// neither park the guest nor reach `TextHost`.
    ///
    /// Returns no effects: a resource appearing or going away changes nothing
    /// on screen until something attaches it, which is B2e-4.
    fn on_text_request(&mut self, request: TextRequest) -> Vec<HostEffect> {
        match request.screen(self.generation) {
            Ok(screened) => self.host.text_resources_mut().serve(screened),
            Err(_stale) => self.stats.stale_text_requests += 1,
        }
        Vec::new()
    }

    fn on_ui_commit(&mut self, request: CommitRequest) -> Vec<HostEffect> {
        // 1. Generation, before anything else. A superseded guest does not get
        //    to spend the host's parser and allocator on its behalf, so this
        //    happens before a single byte is looked at — and `CommitRequest`
        //    has no way to show those bytes until it has.
        let screened = match request.screen(self.generation) {
            Ok(screened) => screened,
            Err(_stale) => {
                self.stats.stale_commits += 1;
                return Vec::new();
            }
        };

        // 2. Resolve each attachment key before a single byte is decoded. A
        //    bad capability must never buy parser work, and keeping the keys
        //    in front of the batch gives each class of bad input exactly one
        //    refusal it can produce. The resolution is positional scratch:
        //    slot `i` names `resolved[i]`, and nothing here polices whether
        //    the table is a projection of the tree.
        let resolved = match self
            .host
            .resolve_attachment_table(screened.generation(), screened.text_view_keys())
        {
            Ok(resolved) => resolved,
            Err(_) => {
                self.stats.attachment_refusals += 1;
                screened.reject(CommitRejection::Attachment(
                    AttachmentRefusal::UnavailableTextView,
                ));
                return Vec::new();
            }
        };

        // 3. Decode and validate semantics. `DecodedUiSnapshot::decode` is
        //    both: the wire parser with its hard bounds, then the checks that
        //    reject a structurally decodable but meaningless tree — and it is
        //    the one parser, returning the attachment refs the bytes said.
        let snapshot = match DecodedUiSnapshot::decode(screened.batch()) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.stats.rejected_commits += 1;
                // Nothing was mutated, so the previous interface still stands.
                screened.reject(CommitRejection::Invalid(error.to_string()));
                return Vec::new();
            }
        };

        // 4. Slot resolution, then 5. uniqueness. The slot check happens on
        //    the decoded refs — the tree is what says a text view exists —
        //    but against the side table, which is where the slot indexes.
        let attachments = match Host::resolve_attachments(&snapshot.text_attachments, &resolved) {
            Ok(attachments) => attachments,
            Err(refusal) => {
                self.stats.attachment_refusals += 1;
                screened.reject(CommitRejection::Attachment(refusal));
                return Vec::new();
            }
        };

        // 6. Tree diff, 7. ledger.validate, 8. attachment diff. The diff can
        //    refuse — a key that named one kind of node and now names another
        //    — and so can the ledger; the attachment diff is the last thing
        //    computed and cannot. All three happen before a single byte of
        //    state is touched.
        let attachment_refs = snapshot.text_attachments;
        let validated =
            match self
                .host
                .validate_ui_commit(self.window, snapshot.tree, attachment_refs)
            {
                Ok(validated) => validated,
                Err(error) => {
                    self.stats.rejected_commits += 1;
                    screened.reject(CommitRejection::Invalid(error.to_string()));
                    return Vec::new();
                }
            };
        let staged = match self
            .host
            .stage_ui_commit(self.window, validated, attachments)
        {
            Ok(staged) => staged,
            Err(error) => {
                self.stats.rejected_commits += 1;
                screened.reject(CommitRejection::Invalid(error.to_string()));
                return Vec::new();
            }
        };

        // A StagedUiCommit exists, so nothing from here on can refuse: apply
        // atomically, lay out, lower, ask for a frame — the same infallible
        // tail `apply_tree` uses.
        let effects = self.host.apply_staged_commit(self.window, staged);

        // Every accepted commit is a new sequence number for the guest, even
        // when the snapshot was a no-op for the host; the tree revision is the
        // host's answer to "did anything actually change".
        self.commit_sequence += 1;
        self.stats.applied_commits += 1;

        // 9. Reply last. The guest resumes knowing the interface it described
        //    is the one the host is now showing.
        screened.accept(self.commit_sequence);
        effects
    }

    /// Stops the guest and joins its thread.
    pub fn shutdown(&mut self) {
        self.runtime.shutdown();
    }
}

impl std::fmt::Debug for HostBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostBridge")
            .field("generation", &self.generation)
            .field("window", &self.window)
            .field("commit_sequence", &self.commit_sequence)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostWindow;
    use instar_kernel::bridge::commit_request;
    use instar_kernel::runtime::SharedKernel;
    use instar_kernel::text_bridge::{OpaqueResourceKey, TextAnswer, TextOperation, text_request};
    use instar_text::TextViewId;
    use instar_ui::protocol::{BatchEncoder, WireAlign, WireLayout, flags, opcode};
    use instar_ui::{NodeKey, NodeKind};
    use instar_window::{LogicalSize, PhysicalSize, WindowMetricsChanged};

    const WINDOW: WindowId = WindowId::from_raw(1);
    const GEN: GenerationId = GenerationId(1);

    fn metrics() -> WindowMetricsChanged {
        WindowMetricsChanged {
            window_id: WINDOW,
            logical_size: LogicalSize {
                width: 400.0,
                height: 400.0,
            },
            physical_size: PhysicalSize {
                width: 400,
                height: 400,
            },
            scale_factor: 1.0,
        }
    }

    fn valid_batch(text: &str) -> Vec<u8> {
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
                flags::ENABLED,
                Some(text),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    fn label_text(bridge: &HostBridge) -> String {
        match bridge
            .host()
            .window(WINDOW)
            .and_then(HostWindow::tree)
            .and_then(|tree| tree.find(NodeKey::first(2)))
            .map(|node| &node.kind)
        {
            Some(NodeKind::Text { text }) => text.clone(),
            other => panic!("expected a text node, found {other:?}"),
        }
    }

    /// A bridge whose runtime thread is inert: the test feeds its queue
    /// directly through the returned sender. Real guest behaviour is covered
    /// by `crates/instar-host/tests/bridge.rs`; here the main-thread half is
    /// being driven deterministically.
    fn test_bridge() -> (
        HostBridge,
        std::sync::mpsc::SyncSender<HostUserEvent>,
        Arc<AtomicU64>,
    ) {
        let (events_tx, events_rx) = std::sync::mpsc::sync_channel(QUEUE_CAPACITY);
        let (commands_tx, _commands_rx) = mpsc::channel(1);
        let stop = Arc::new(tokio::sync::Notify::new());
        let wake_count = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&wake_count);
        let wake: Wake = Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        let bridge = HostBridge {
            host: Host::new(),
            generation: GEN,
            window: WINDOW,
            runtime: RuntimeThread {
                commands: commands_tx,
                stop,
                join: None,
                generation: GEN,
                kernel: Arc::new(SharedKernel::default()),
                dropped_commands: 0,
                dropped_commits: Arc::new(AtomicU64::new(0)),
                engine: instar_kernel::engine::configured_engine()
                    .expect("a dummy engine for the inert test bridge"),
            },
            events: events_rx,
            terminal: Arc::default(),
            wake,
            commit_sequence: 0,
            guest_message_count: 0,
            stats: BridgeStats::default(),
        };
        (bridge, events_tx, wake_count)
    }

    fn queued_commit(batch: Vec<u8>) -> HostUserEvent {
        HostUserEvent::UiCommit {
            generation: GEN,
            request: commit_request(GEN, batch, Vec::new()).0,
        }
    }

    /// Opens one buffer and one view for `GEN`, registering both leases.
    fn create_view(bridge: &mut HostBridge) -> OpaqueResourceKey {
        let mut serve = |operation: TextOperation| {
            let (request, wait) = text_request(GEN, operation);
            let screened = request.screen(GEN).expect("current generation");
            bridge.host.text_resources_mut().serve(screened);
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

    /// Delivers one commit through the main-thread half and returns the
    /// verdict the guest would have seen.
    fn deliver_commit(
        bridge: &mut HostBridge,
        batch: Vec<u8>,
        keys: Vec<OpaqueResourceKey>,
    ) -> Result<u64, CommitRejection> {
        let (request, wait) = commit_request(GEN, batch, keys);
        bridge.on_ui_commit(request);
        wait.blocking_recv().expect("commit answered")
    }

    #[test]
    fn pump_respects_its_item_budget_and_schedules_continuation() {
        let (mut bridge, events_tx, wake_count) = test_bridge();
        const TOTAL: usize = 128;
        for _ in 0..TOTAL {
            events_tx
                .try_send(queued_commit(b"undecodable".to_vec()))
                .expect("the queue accepts the test load");
        }

        let effects = bridge.pump_bounded(16, Duration::from_secs(60));
        assert!(effects.is_empty());
        assert_eq!(bridge.stats().rejected_commits, 16);
        assert!(
            wake_count.load(Ordering::Relaxed) > 0,
            "work remains, so the pump must arrange another wake"
        );

        bridge.pump_bounded(usize::MAX, Duration::from_secs(60));
        assert_eq!(bridge.stats().rejected_commits, TOTAL as u64);
    }

    #[test]
    fn pump_respects_its_time_budget() {
        let (mut bridge, events_tx, wake_count) = test_bridge();
        for _ in 0..4 {
            events_tx
                .try_send(queued_commit(b"undecodable".to_vec()))
                .expect("the queue accepts the test load");
        }

        // A zero time budget forces the time bound to fire after one item.
        bridge.pump_bounded(usize::MAX, Duration::ZERO);
        assert_eq!(
            bridge.stats().rejected_commits,
            1,
            "the elapsed-time budget must stop the pass"
        );
        assert!(wake_count.load(Ordering::Relaxed) > 0);

        bridge.pump_bounded(usize::MAX, Duration::from_secs(60));
        assert_eq!(bridge.stats().rejected_commits, 4);
    }

    #[test]
    fn bounded_pump_preserves_ordinary_ordering() {
        let (mut bridge, events_tx, _wake_count) = test_bridge();
        bridge.on_window_event(WindowOutput::MetricsChanged(metrics()));
        for text in ["first", "second", "third"] {
            events_tx
                .try_send(queued_commit(valid_batch(text)))
                .expect("the queue accepts the test load");
        }

        bridge.pump_bounded(2, Duration::from_secs(60));
        assert_eq!(bridge.commit_sequence(), 2);
        assert_eq!(
            label_text(&bridge),
            "second",
            "after two items the second-queued batch must be the one applied"
        );

        bridge.pump_bounded(usize::MAX, Duration::from_secs(60));
        assert_eq!(bridge.commit_sequence(), 3);
        assert_eq!(label_text(&bridge), "third");
    }

    #[test]
    fn terminal_is_observed_first_exactly_once_and_drops_no_work() {
        let (mut bridge, events_tx, wake_count) = test_bridge();

        // Saturate the bounded ordinary queue, then report terminal state. The
        // terminal outcome must still be observed, and must not be counted as
        // dropped work; every queued commit is refused instead of applied.
        for _ in 0..QUEUE_CAPACITY {
            events_tx
                .try_send(queued_commit(b"undecodable".to_vec()))
                .expect("the queue accepts exactly its capacity");
        }
        bridge.report_terminal(TerminalOutcome::GuestExited { generation: GEN });

        let effects = bridge.pump_bounded(64, Duration::from_secs(60));
        assert_eq!(
            effects,
            vec![HostEffect::GuestGone {
                generation: GEN,
                error: None,
            }],
            "the terminal outcome is observed before any queued commit"
        );
        assert_eq!(bridge.stats().stale_commits, 64);
        assert_eq!(bridge.stats().applied_commits, 0);
        assert_eq!(bridge.stats().dropped_commits, 0);
        assert_eq!(bridge.generation(), GenerationId(0));
        assert!(
            wake_count.load(Ordering::Relaxed) > 0,
            "the remaining stale work must schedule a continuation"
        );

        while bridge.stats().stale_commits < QUEUE_CAPACITY as u64 {
            assert!(
                bridge.pump_bounded(64, Duration::from_secs(60)).is_empty(),
                "terminal state must not be observed again"
            );
        }
        assert_eq!(bridge.stats().stale_commits, QUEUE_CAPACITY as u64);
        assert!(
            bridge.pump().is_empty(),
            "the terminal outcome is consumed exactly once"
        );
    }

    // --- Text-view attachments (B2e-3b) ---

    /// `[V7, V7]` in the side table is legal: a single text view may be
    /// attached in several places, and an entry nobody references is just an
    /// unreferenced scratch slot.
    #[test]
    fn duplicate_side_table_entries_are_accepted_when_one_node_names_the_view() {
        let (mut bridge, _tx, _wake) = test_bridge();
        let v7 = create_view(&mut bridge);
        let batch = attachment_batch(&[(10, 0)]);

        assert!(deliver_commit(&mut bridge, batch, vec![v7, v7]).is_ok());
        assert_eq!(bridge.stats().attachment_refusals, 0);
    }

    /// Unreferenced side-table entries are legal too: the table is a guest
    /// scratch list, not a projection of the tree.
    #[test]
    fn unreferenced_side_table_entries_are_accepted() {
        let (mut bridge, _tx, _wake) = test_bridge();
        let v7 = create_view(&mut bridge);
        let v8 = create_view(&mut bridge);
        let batch = attachment_batch(&[(10, 0)]);

        assert!(deliver_commit(&mut bridge, batch, vec![v7, v8]).is_ok());
        assert_eq!(
            bridge
                .host()
                .window(WINDOW)
                .unwrap()
                .text_attachments()
                .len(),
            1,
            "slot 1 names V8 but no node references it, so nothing is retained"
        );
    }

    /// The illegal state is two live NodeKeys reaching one TextViewId, not a
    /// duplicated table entry.
    #[test]
    fn two_live_nodes_naming_one_view_are_refused() {
        let (mut bridge, _tx, _wake) = test_bridge();
        let v7 = create_view(&mut bridge);
        let batch = attachment_batch(&[(10, 0), (20, 0)]);

        assert_eq!(
            deliver_commit(&mut bridge, batch, vec![v7, v7]),
            Err(CommitRejection::Attachment(
                AttachmentRefusal::TextViewAlreadyAttached
            ))
        );
        assert_eq!(bridge.stats().attachment_refusals, 1);
        assert_eq!(bridge.stats().rejected_commits, 0);
    }

    #[test]
    fn a_slot_at_the_table_length_is_refused_out_of_range() {
        let (mut bridge, _tx, _wake) = test_bridge();
        let v7 = create_view(&mut bridge);
        let batch = attachment_batch(&[(10, 1)]);

        assert_eq!(
            deliver_commit(&mut bridge, batch, vec![v7]),
            Err(CommitRejection::Attachment(
                AttachmentRefusal::AttachmentOutOfRange
            ))
        );
        assert_eq!(bridge.stats().attachment_refusals, 1);
    }

    #[test]
    fn slot_u16_max_is_refused_out_of_range_against_a_short_table() {
        let (mut bridge, _tx, _wake) = test_bridge();
        let v7 = create_view(&mut bridge);
        let batch = attachment_batch(&[(10, u16::MAX)]);

        assert_eq!(
            deliver_commit(&mut bridge, batch, vec![v7]),
            Err(CommitRejection::Attachment(
                AttachmentRefusal::AttachmentOutOfRange
            ))
        );
    }

    /// Resolution precedes decoding, so a batch that is ALSO malformed gets
    /// the attachment verdict and nothing else. The fault being caught here is
    /// decoding before resolving: a key this generation does not own must not
    /// buy parser work, even when the parser would have refused anyway.
    #[test]
    fn an_unowned_key_is_refused_before_a_malformed_batch_is_decoded() {
        let (mut bridge, _tx, _wake) = test_bridge();
        let key = OpaqueResourceKey {
            slot: 1,
            incarnation: 0,
        };

        assert_eq!(
            deliver_commit(&mut bridge, b"not a batch".to_vec(), vec![key]),
            Err(CommitRejection::Attachment(
                AttachmentRefusal::UnavailableTextView
            ))
        );
        assert_eq!(bridge.stats().attachment_refusals, 1);
        assert_eq!(
            bridge.stats().rejected_commits,
            0,
            "decode never ran, so the batch's malformation is never observed"
        );
    }

    /// Switching which view a node shows, with the tree byte-identical.
    ///
    /// The tree diff is empty and the attachment diff is not, which is the one
    /// shape the old no-op gate could not see: it returned early on an empty
    /// *tree* diff, so the new view was never promoted while the guest was
    /// told its commit had been accepted. An editor pane switching documents
    /// is exactly this commit.
    #[test]
    fn an_attachment_only_change_is_promoted() {
        let (mut bridge, _tx, _wake) = test_bridge();
        let v7 = create_view(&mut bridge);
        let v12 = create_view(&mut bridge);
        let batch = attachment_batch(&[(10, 0)]);

        assert!(deliver_commit(&mut bridge, batch.clone(), vec![v7]).is_ok());
        assert!(
            deliver_commit(&mut bridge, batch, vec![v12]).is_ok(),
            "the same bytes, a different capability -- still an accepted commit"
        );

        let v12_id = bridge
            .host
            .resolve_attachment_table(GEN, &[v12])
            .expect("leased")[0];
        assert_eq!(
            bridge
                .host
                .window(WINDOW)
                .unwrap()
                .text_attachments()
                .get(&NodeKey::first(10)),
            Some(&v12_id),
            "the node must now name V12; an accepted commit that changed \
             nothing is a commit the host lied about"
        );
    }

    /// A **live** view another generation owns is refused at the attachment
    /// seam.
    ///
    /// The distinction `docs/PHASE-3.md` draws as "the registry is
    /// authoritative, not the id", carried to the commit path. The sibling
    /// test above names a resource that never existed, so the identity check
    /// alone refuses it — which proves resolution happens before decoding, and
    /// proves nothing at all about *authority*. Here the `TextViewId` is
    /// genuinely live, and the only thing wrong is who is asking.
    ///
    /// This is the test the phase rule demands: deleting the ownership check
    /// in `TextHost::resolve_view_lease` must break a commit, not only a
    /// `text_host` unit test.
    #[test]
    fn a_live_view_another_generation_owns_is_refused() {
        let (mut bridge, _tx, _wake) = test_bridge();

        // Created for a generation that is not the one committing. Served
        // directly, because the whole point is that no route exists for the
        // committing generation to have acquired it.
        let stranger = GenerationId(GEN.0 + 1);
        let mut serve = |operation: TextOperation| {
            let (request, wait) = text_request(stranger, operation);
            let screened = request.screen(stranger).expect("current for the stranger");
            bridge.host.text_resources_mut().serve(screened);
            wait.blocking_recv().expect("answered")
        };
        let buffer = match serve(TextOperation::CreateBuffer) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a buffer, got {other:?}"),
        };
        let key = match serve(TextOperation::CreateView { buffer }) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a view, got {other:?}"),
        };

        // Live, and not this generation's to name.
        assert!(
            bridge
                .host
                .text_resources()
                .resolve_view_lease(stranger, key)
                .is_ok(),
            "the view really is live -- otherwise identity would refuse it and \
             this test would prove nothing about authority"
        );

        let batch = attachment_batch(&[(10, 0)]);
        assert_eq!(
            deliver_commit(&mut bridge, batch, vec![key]),
            Err(CommitRejection::Attachment(
                AttachmentRefusal::UnavailableTextView
            )),
            "a live id answers which resource, never whose"
        );
        assert!(
            bridge
                .host
                .window(WINDOW)
                .is_none_or(|window| window.text_attachments().is_empty()),
            "and nothing was attached"
        );
    }

    /// The happy path all the gates exist for: a `TextView` node whose slot
    /// resolves ends up in the window's retained attachment map, keyed by the
    /// node, with the resolved `TextViewId`.
    #[test]
    fn a_resolved_text_view_reaches_the_window_attachment_map() {
        let (mut bridge, _tx, _wake) = test_bridge();
        let v7 = create_view(&mut bridge);
        let batch = attachment_batch(&[(10, 0)]);

        assert!(deliver_commit(&mut bridge, batch, vec![v7]).is_ok());
        let view = bridge
            .host()
            .window(WINDOW)
            .unwrap()
            .text_attachments()
            .get(&NodeKey::first(10))
            .copied()
            .expect("the text view node is attached");
        assert_eq!(
            view,
            TextViewId {
                id: 0,
                generation: 0
            }
        );
    }
}
