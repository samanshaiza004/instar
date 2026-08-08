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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Duration;

use instar_kernel::bridge::{CommitRejection, CommitRequest, CommitSink, GuestEvent};
use instar_kernel::runtime::{GenerationId, Runtime};
use instar_ui::Tree;
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
    dropped: AtomicU64,
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
        wake: Wake,
    ) -> Result<Self, String> {
        let (commands_tx, commands_rx) = mpsc::channel(QUEUE_CAPACITY);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(tokio::sync::Notify::new());
        let thread_stop = Arc::clone(&stop);

        let join = std::thread::Builder::new()
            .name("instar-runtime".to_string())
            .spawn(move || {
                run_thread(
                    component,
                    commands_rx,
                    thread_stop,
                    events,
                    wake,
                    started_tx,
                )
            })
            .map_err(|error| format!("could not start the runtime thread: {error}"))?;

        match started_rx.recv() {
            Ok(Ok(Started { generation, kernel })) => Ok(Self {
                commands: commands_tx,
                stop,
                join: Some(join),
                generation,
                kernel,
                dropped_commands: 0,
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

    /// Asks the guest to stop and waits for the thread.
    ///
    /// The signal jumps the command queue, because the state most in need of
    /// shutting down is exactly the state where that queue is full.
    pub fn shutdown(&mut self) {
        self.stop.notify_one();
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
            .finish_non_exhaustive()
    }
}

/// The runtime thread's whole life.
fn run_thread(
    component: Vec<u8>,
    mut commands: mpsc::Receiver<RuntimeCommand>,
    stop: Arc<tokio::sync::Notify>,
    events: SyncSender<HostUserEvent>,
    wake: Wake,
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
        events,
        wake,
        dropped: AtomicU64::new(0),
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

        let _ = sink.emit(match ending {
            Ending::Exited => HostUserEvent::GuestExited { generation: id },
            Ending::Trapped(error) => HostUserEvent::GuestTrapped {
                generation: id,
                error,
            },
        });
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
    /// Commits that decoded but did not validate, or did not decode.
    pub rejected_commits: u64,
    /// Commits applied.
    pub applied_commits: u64,
    /// Events the winit thread could not queue because the runtime was behind.
    pub dropped_commands: u64,
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
    /// Accepted guest commits, in order.
    ///
    /// The counter the guest sees: `screened.accept(...)` is handed this value,
    /// so it advances on every accepted commit even when the snapshot was
    /// identical to the retained one. That monotonicity is the guest's sync
    /// contract, and it must not be tied to whether the host found the tree
    /// interesting.
    commit_sequence: u64,
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

    /// As [`HostBridge::spawn`], with a host that can draw text.
    ///
    /// A constructor rather than a setter: the glyph source is an input to
    /// every scene the host lowers, and one arriving after the guest's opening
    /// commit would mean the first frame silently had no labels.
    pub fn spawn_with_glyphs(
        component: Vec<u8>,
        window: WindowId,
        wake: Wake,
        glyphs: Arc<dyn crate::GlyphSource>,
    ) -> Result<Self, String> {
        Self::start(Host::with_glyphs(glyphs), component, window, wake)
    }

    fn start(host: Host, component: Vec<u8>, window: WindowId, wake: Wake) -> Result<Self, String> {
        let (events_tx, events_rx) = std::sync::mpsc::sync_channel(QUEUE_CAPACITY);
        let runtime = RuntimeThread::spawn(component, events_tx, wake)?;
        Ok(Self {
            host,
            generation: runtime.generation(),
            window,
            runtime,
            events: events_rx,
            commit_sequence: 0,
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
            ..self.stats
        }
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
        self.host
            .handle(event)
            .into_iter()
            .filter(|effect| match effect {
                HostEffect::SendToGuest(bytes) => {
                    self.runtime
                        .send(RuntimeCommand::DeliverEvent(GuestEvent::new(bytes.clone())));
                    false
                }
                _ => true,
            })
            .collect()
    }

    /// Asks the runtime to cancel one in-flight operation.
    pub fn cancel_operation(&mut self, operation: u64) -> bool {
        self.runtime
            .send(RuntimeCommand::CancelOperation(operation))
    }

    /// Drains everything the runtime thread has queued, without blocking.
    ///
    /// This is what a winit `user_event` handler calls: the proxy only says
    /// *that* something arrived, and the queue says what.
    pub fn pump(&mut self) -> Vec<HostEffect> {
        let mut effects = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            effects.extend(self.on_user_event(event));
        }
        effects
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
        let first = match self.events.recv_timeout(timeout) {
            Ok(event) => event,
            Err(_) => return Vec::new(),
        };
        let mut effects = self.on_user_event(first);
        effects.extend(self.pump());
        effects
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
            HostUserEvent::GuestTrapped { generation, error } => {
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
            HostUserEvent::GuestExited { generation } => {
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

        // 2. Decode, and 3. validate semantics. `Tree::decode` is both: the
        //    wire parser with its hard bounds, then the checks that reject a
        //    structurally decodable but meaningless tree.
        let tree = match Tree::decode(screened.batch()) {
            Ok(tree) => tree,
            Err(error) => {
                self.stats.rejected_commits += 1;
                // Nothing was mutated, so the previous interface still stands.
                screened.reject(CommitRejection::Invalid(error.to_string()));
                return Vec::new();
            }
        };

        // 4. Diff against the retained tree, 5. apply atomically, 6. lay out,
        //    7. lower, 8. ask for a frame. All of it lives inside `apply_tree`,
        //    in that order.
        //
        //    The diff can refuse — a key that named one kind of node and now
        //    names another. That is a semantic rejection like any other and
        //    lands here rather than inside the apply, because it happens
        //    before a single byte of state is touched.
        let effects = match self.host.apply_tree(self.window, tree) {
            Ok(effects) => effects,
            Err(error) => {
                self.stats.rejected_commits += 1;
                screened.reject(CommitRejection::Invalid(error.to_string()));
                return Vec::new();
            }
        };

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
