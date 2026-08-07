//! Gate 0 headless kernel spike (WP3).
//!
//! This module exists to answer one empirical question and then be deleted or
//! rewritten: **can a Wasmtime Component Model guest genuinely suspend on an
//! async host import and be woken by the host, with zero idle polling, while
//! independent async work makes concurrent progress and can be cancelled
//! cleanly?**
//!
//! Nothing here is a draft of Instar's real runtime. The event "protocol" is
//! ASCII commands (see the guest fixture), the host state is a test harness,
//! and `test-support.delay` is a synthetic primitive that does not survive
//! into the real protocol. What *is* meant to survive is the finding: the
//! answer to Gate 0, recorded in `docs/GATE-0.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use wasmtime::Store;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit",
    world: "kernel-spike",
    // `runtime.next-event` and `test-support.delay` are WIT-level `async
    // func`s, which implies ASYNC|STORE on their generated host signatures
    // automatically -- they are not listed here. `ui.commit` stays sync on
    // purpose (see wit/world.wit).
    imports: { default: trappable },
});

/// Counters the idle gates assert against.
///
/// These are the observable that makes "the guest is idle" checkable rather
/// than a vibe: if the runtime were polling `next-event` in a loop, or waking
/// the guest on a timer, `next_event_calls` would climb while the harness
/// deliberately does nothing.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Times the guest entered `runtime.next-event`.
    pub next_event_calls: AtomicU64,
    /// Times the guest entered `test-support.delay`.
    pub delay_calls: AtomicU64,
    /// Times the guest called `ui.commit`.
    pub commit_calls: AtomicU64,
}

impl Metrics {
    pub fn next_event_calls(&self) -> u64 {
        self.next_event_calls.load(Ordering::SeqCst)
    }
    pub fn delay_calls(&self) -> u64 {
        self.delay_calls.load(Ordering::SeqCst)
    }
    pub fn commit_calls(&self) -> u64 {
        self.commit_calls.load(Ordering::SeqCst)
    }
}

/// What the host hands the guest when it asks for an event.
pub enum Event {
    /// Deliver these bytes as a successful `next-event` result.
    Payload(Vec<u8>),
    /// Tell the guest to leave its event loop and return from `run`.
    Shutdown,
}

/// Host state for the spike.
pub struct SpikeState {
    ctx: WasiCtx,
    table: ResourceTable,
    /// Events pending delivery to the guest. An async channel is the whole
    /// trick: `next-event` awaits `recv()`, which parks the guest task on a
    /// waker rather than spinning.
    events: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Event>>>,
    /// Everything the guest has committed, in order, for test assertions.
    commits: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    revision: u64,
    metrics: Arc<Metrics>,
}

impl WasiView for SpikeState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl instar::kernel::ui::Host for SpikeState {
    fn commit(
        &mut self,
        batch: Vec<u8>,
    ) -> wasmtime::Result<
        Result<instar::kernel::types::CommitResult, instar::kernel::types::CommitError>,
    > {
        self.metrics.commit_calls.fetch_add(1, Ordering::SeqCst);
        self.revision += 1;
        self.commits
            .lock()
            .expect("commit log mutex poisoned")
            .push(batch);
        Ok(Ok(instar::kernel::types::CommitResult {
            revision: self.revision,
        }))
    }
}

impl instar::kernel::types::Host for SpikeState {}

// `runtime` and `test-support` expose only WIT-level `async func`s, so their
// per-instance `Host` traits are empty -- the actual methods live on
// `HostWithStore`, which takes an `Accessor` instead of `&mut self` precisely
// because it may suspend and must not hold a store borrow across an await.
impl instar::kernel::runtime::Host for SpikeState {}
impl instar::kernel::test_support::Host for SpikeState {}

impl instar::kernel::runtime::HostWithStore<SpikeState>
    for wasmtime::component::HasSelf<SpikeState>
{
    fn next_event(
        accessor: &wasmtime::component::Accessor<SpikeState, Self>,
    ) -> impl std::future::Future<
        Output = wasmtime::Result<Result<Vec<u8>, instar::kernel::types::RuntimeError>>,
    > + Send {
        // Pull the shared handles out *before* awaiting: the store borrow
        // inside `with` cannot be held across a suspension point, which is
        // the entire reason this signature takes an `Accessor`.
        let (events, metrics) = accessor.with(|mut access| {
            let state = access.get();
            (Arc::clone(&state.events), Arc::clone(&state.metrics))
        });

        async move {
            metrics.next_event_calls.fetch_add(1, Ordering::SeqCst);

            let mut events = events.lock().await;
            // This await is the load-bearing line of the whole spike: the
            // guest task parks here on a waker until the host sends, and
            // nothing polls it in the meantime.
            match events.recv().await {
                Some(Event::Payload(payload)) => Ok(Ok(payload)),
                // Channel closed means the harness dropped its handle, which
                // is a shutdown by another name.
                Some(Event::Shutdown) | None => {
                    Ok(Err(instar::kernel::types::RuntimeError::Shutdown))
                }
            }
        }
    }
}

impl instar::kernel::test_support::HostWithStore<SpikeState>
    for wasmtime::component::HasSelf<SpikeState>
{
    fn delay(
        accessor: &wasmtime::component::Accessor<SpikeState, Self>,
        millis: u32,
    ) -> impl std::future::Future<Output = wasmtime::Result<u32>> + Send {
        let metrics = accessor.with(|mut access| Arc::clone(&access.get().metrics));

        async move {
            metrics.delay_calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(u64::from(millis))).await;
            Ok(millis)
        }
    }
}

/// The harness's handle on a running spike: send events in, read commits out.
pub struct SpikeHandle {
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    commits: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    metrics: Arc<Metrics>,
}

impl SpikeHandle {
    /// Queues an event for the guest. Returns an error only if the guest has
    /// already stopped consuming events.
    pub fn send(&self, payload: impl Into<Vec<u8>>) -> Result<(), &'static str> {
        self.events
            .send(Event::Payload(payload.into()))
            .map_err(|_| "guest is no longer receiving events")
    }

    /// Asks the guest to leave its event loop and return from `run`.
    pub fn shutdown(&self) -> Result<(), &'static str> {
        self.events
            .send(Event::Shutdown)
            .map_err(|_| "guest is no longer receiving events")
    }

    /// Snapshot of everything committed so far, oldest first.
    pub fn commits(&self) -> Vec<Vec<u8>> {
        self.commits
            .lock()
            .expect("commit log mutex poisoned")
            .clone()
    }

    /// Same, decoded as UTF-8 for readable assertions.
    pub fn commits_utf8(&self) -> Vec<String> {
        self.commits()
            .into_iter()
            .map(|c| String::from_utf8_lossy(&c).into_owned())
            .collect()
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }
}

/// A spike instance that has been built but not yet run.
pub struct Spike {
    store: Store<SpikeState>,
    instance: wasmtime::component::Instance,
    bindings: KernelSpike,
}

impl Spike {
    /// Builds an engine, links WASI plus the spike's own imports, and
    /// instantiates the guest fixture -- without running it yet, so tests can
    /// take metrics baselines before the guest does anything.
    pub async fn new(component_bytes: &[u8]) -> wasmtime::Result<(Self, SpikeHandle)> {
        let engine = crate::engine::configured_engine()?;
        let component = Component::from_binary(&engine, component_bytes)?;

        let mut linker: Linker<SpikeState> = Linker::new(&engine);
        // The guest is built for wasm32-wasip2, so Rust's std pulls in WASI
        // 0.2 imports (stdio, clocks, exit) whether or not the spike uses
        // them. They must be linked for instantiation to succeed.
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        KernelSpike::add_to_linker::<_, wasmtime::component::HasSelf<_>>(&mut linker, |s| s)?;

        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let commits: Arc<std::sync::Mutex<Vec<Vec<u8>>>> = Arc::default();
        let metrics: Arc<Metrics> = Arc::default();

        let state = SpikeState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            events: Arc::new(tokio::sync::Mutex::new(events_rx)),
            commits: Arc::clone(&commits),
            revision: 0,
            metrics: Arc::clone(&metrics),
        };

        let mut store = Store::new(&engine, state);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let bindings = KernelSpike::new(&mut store, &instance)?;

        Ok((
            Self {
                store,
                instance,
                bindings,
            },
            SpikeHandle {
                events: events_tx,
                commits,
                metrics,
            },
        ))
    }

    /// Runs the guest's `run` export to completion.
    ///
    /// This is the future the idle gates poll-count: it must not be woken
    /// while the guest is parked in `next-event`.
    pub async fn run(&mut self) -> wasmtime::Result<Result<(), String>> {
        let bindings = &self.bindings;
        self.store
            .run_concurrent(async move |accessor| bindings.call_run(accessor).await)
            .await?
    }

    /// Turns the store's concurrent event loop without calling any guest
    /// export.
    ///
    /// Cancellation of a started guest task is *initiated* by dropping the
    /// host future driving it, but the runtime still has to process that
    /// cancellation; that only happens while its event loop runs. This is the
    /// hook the cancellation gate uses to give it that chance.
    pub async fn drive_event_loop(&mut self) -> wasmtime::Result<()> {
        self.store.run_concurrent(async |_accessor| ()).await
    }

    /// Wasmtime's own view of concurrent bookkeeping. The cancellation gate
    /// asserts this returns to empty, which is what "cancelled cleanly, with
    /// nothing leaked" actually means at the runtime level.
    pub fn assert_concurrent_state_empty(&mut self) {
        self.store.assert_concurrent_state_empty();
    }

    pub fn concurrent_state_table_size(&mut self) -> usize {
        self.store.concurrent_state_table_size()
    }

    pub fn instance(&self) -> wasmtime::component::Instance {
        self.instance
    }
}

/// The guest fixture component, built by `build.rs` for this exact toolchain
/// and OS.
pub fn guest_component_bytes() -> std::io::Result<Vec<u8>> {
    std::fs::read(env!("KERNEL_SPIKE_GUEST_WASM"))
}
