use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::{
    AppFault, AppInspection, AppLifecycle, ErrorContext, RuntimeError, RuntimeLimits, ScheduleWake,
    TurnReceipt, WakeDisposition, YouthAppConfig,
};

const COMMAND_CAPACITY: usize = 64;
const OBSERVER_CAPACITY: usize = 64;

pub type RequestId = u64;
pub type ScheduleId = u64;
pub type Generation = u64;

#[derive(Clone)]
pub struct PresentationReader {
    records: Arc<std::sync::RwLock<HashMap<u64, youth_state::ScheduleRecord>>>,
    deadline_clock: Arc<dyn youth_state::DeadlineClock>,
}

impl std::fmt::Debug for PresentationReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PresentationReader")
            .finish_non_exhaustive()
    }
}

impl PresentationReader {
    #[must_use]
    pub fn schedule(&self, id: u64) -> Option<youth_state::ScheduleRecord> {
        self.records
            .read()
            .expect("presentation-record lock is not poisoned")
            .get(&id)
            .cloned()
    }

    #[must_use]
    pub fn now_epoch_millis(&self) -> u64 {
        self.deadline_clock.now_epoch_millis()
    }
}

enum AppCommand {
    Mount,
    Activate {
        request_id: RequestId,
        node: youth_tree::NodeId,
    },
    EditorLocalEdit {
        editor: youth_tree::NodeId,
        edit: crate::EditorLocalEdit,
    },
    Resync,
    VerifyView,
    Snapshot,
    Inspect,
    #[cfg(feature = "test-support")]
    FailNextStateCommit,
    Stop,
}

enum ReplySender {
    Snapshot(oneshot::Sender<Result<youth_tree::TreeSnapshot, RuntimeError>>),
    Turn(oneshot::Sender<Result<TurnReceipt, RuntimeError>>),
    EditorLocalEdit(oneshot::Sender<Result<crate::EditorLocalEditResult, RuntimeError>>),
    Inspection(oneshot::Sender<Result<AppInspection, RuntimeError>>),
    ViewVerification(oneshot::Sender<Result<crate::ViewVerification, RuntimeError>>),
    Stop(oneshot::Sender<Result<(), RuntimeError>>),
}

enum WorkerMessage {
    Request {
        command: AppCommand,
        reply: ReplySender,
    },
    Wake(youth_state::WakeToken),
    Reconcile,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    pub origin: TurnOrigin,
    pub receipt: TurnReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnOrigin {
    Requested(RequestId),
    ScheduleElapsed {
        schedule_id: ScheduleId,
        generation: Generation,
    },
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    TurnCommitted(TurnOutcome),
    Faulted(AppFault),
    SnapshotReplaced(youth_tree::TreeSnapshot),
}

#[derive(Debug)]
struct MailboxWakeSink {
    mailbox: mpsc::WeakSender<WorkerMessage>,
}

impl youth_state::WakeSink for MailboxWakeSink {
    fn push(&self, token: youth_state::WakeToken) {
        // Wake producers may wait for mailbox capacity, but observers never
        // can. The weak sender also lets shutdown disconnect cleanly.
        if let Some(mailbox) = self.mailbox.upgrade() {
            let _ = mailbox.blocking_send(WorkerMessage::Wake(token));
        }
    }
}

/// An asynchronous, cloneable connection to one serialized application worker.
#[derive(Clone, Debug)]
pub struct YouthAppHandle {
    component_id: String,
    mailbox_tx: mpsc::Sender<WorkerMessage>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    next_request_id: Arc<AtomicU64>,
    presentation: PresentationReader,
}

impl YouthAppHandle {
    /// Starts a configured dedicated application worker.
    pub fn spawn(config: YouthAppConfig) -> Result<Self, RuntimeError> {
        let component_id = component_identity(&config.component_path);
        let (mailbox_tx, mailbox_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, _) = broadcast::channel(OBSERVER_CAPACITY);
        let presentation = PresentationReader {
            records: Arc::new(std::sync::RwLock::new(HashMap::new())),
            deadline_clock: Arc::clone(&config.limits.time.deadline_clock),
        };
        config
            .limits
            .time
            .wake_driver
            .set_sink(Arc::new(MailboxWakeSink {
                mailbox: mailbox_tx.downgrade(),
            }));
        mailbox_tx
            .try_send(WorkerMessage::Reconcile)
            .expect("a new worker mailbox has room for reconciliation");
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel(1);
        let thread_component_id = component_id.clone();
        let thread_events = event_tx.clone();
        let thread_presentation = presentation.clone();
        std::thread::Builder::new()
            .name(format!("youth-app-{thread_component_id}"))
            .spawn(move || {
                worker_main(
                    config,
                    mailbox_rx,
                    thread_events,
                    thread_presentation,
                    init_tx,
                );
            })
            .map_err(|source| {
                RuntimeError::WorkerStopped(
                    ErrorContext::new(
                        "failed to start application worker",
                        &component_id,
                        AppLifecycle::Loaded,
                        None,
                    )
                    .with_source(source),
                )
            })?;
        init_rx.recv().map_err(|source| {
            RuntimeError::WorkerStopped(
                ErrorContext::new(
                    "application worker stopped during startup",
                    &component_id,
                    AppLifecycle::Loaded,
                    None,
                )
                .with_source(source),
            )
        })??;
        Ok(Self {
            component_id,
            mailbox_tx,
            event_tx,
            next_request_id: Arc::new(AtomicU64::new(1)),
            presentation,
        })
    }

    /// Preserves the Milestone 0 path-only in-memory behavior.
    pub fn spawn_ephemeral(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        Self::spawn(YouthAppConfig::ephemeral(path))
    }

    pub fn spawn_ephemeral_with_limits(
        path: impl AsRef<Path>,
        limits: RuntimeLimits,
    ) -> Result<Self, RuntimeError> {
        let mut config = YouthAppConfig::ephemeral(path);
        config.limits = limits;
        Self::spawn(config)
    }

    /// Subscribes to committed runtime changes.
    ///
    /// Publication is bounded and non-blocking. If a receiver falls more than
    /// 64 events behind, `recv` returns `Lagged`; the observer must resync.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.event_tx.subscribe()
    }

    /// Returns a synchronous, presentation-only view of host-owned schedules.
    #[must_use]
    pub fn presentation(&self) -> PresentationReader {
        self.presentation.clone()
    }

    pub async fn mount(&self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.send_request(AppCommand::Mount, ReplySender::Snapshot(reply))
            .await?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn activate(&self, node: youth_tree::NodeId) -> Result<TurnReceipt, RuntimeError> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = oneshot::channel();
        self.send_request(
            AppCommand::Activate { request_id, node },
            ReplySender::Turn(reply),
        )
        .await?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    /// Applies a host-local Editor mutation on the serialized worker without
    /// invoking the guest or creating a turn receipt.
    pub async fn edit_editor_locally(
        &self,
        editor: youth_tree::NodeId,
        edit: crate::EditorLocalEdit,
    ) -> Result<crate::EditorLocalEditResult, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.send_request(
            AppCommand::EditorLocalEdit { editor, edit },
            ReplySender::EditorLocalEdit(reply),
        )
        .await?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn resync(&self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.send_request(AppCommand::Resync, ReplySender::Snapshot(reply))
            .await?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn snapshot(&self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.send_request(AppCommand::Snapshot, ReplySender::Snapshot(reply))
            .await?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    /// Reconstructs a read-only guest view without publishing or installing it.
    #[doc(hidden)]
    pub async fn verify_view(&self) -> Result<crate::ViewVerification, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.send_request(AppCommand::VerifyView, ReplySender::ViewVerification(reply))
            .await?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn inspect(&self) -> Result<AppInspection, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.send_request(AppCommand::Inspect, ReplySender::Inspection(reply))
            .await?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    #[cfg(feature = "test-support")]
    pub async fn fail_next_state_commit(&self) -> Result<(), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.send_request(AppCommand::FailNextStateCommit, ReplySender::Stop(reply))
            .await?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn stop(&self) -> Result<(), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.send_request(AppCommand::Stop, ReplySender::Stop(reply))
            .await?;
        response.await.map_err(|_| self.worker_stopped())??;
        self.mailbox_tx
            .send(WorkerMessage::Shutdown)
            .await
            .map_err(|_| self.worker_stopped())
    }

    async fn send_request(
        &self,
        command: AppCommand,
        reply: ReplySender,
    ) -> Result<(), RuntimeError> {
        self.mailbox_tx
            .send(WorkerMessage::Request { command, reply })
            .await
            .map_err(|_| self.worker_stopped())
    }

    fn worker_stopped(&self) -> RuntimeError {
        RuntimeError::WorkerStopped(ErrorContext::new(
            "application worker is no longer running",
            &self.component_id,
            AppLifecycle::Stopped,
            None,
        ))
    }
}

fn reconcile_without_guest(config: &YouthAppConfig) -> Result<(), RuntimeError> {
    let component_id = component_identity(&config.component_path);
    let mut state = youth_state::StateStore::open_for_app(
        config.state.clone(),
        config.limits.state,
        config.app_id.clone(),
    )
    .map_err(|source| {
        RuntimeError::StateUnavailable(
            ErrorContext::new(
                "application state could not be opened for reconciliation",
                &component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let outputs = state
        .reconcile_overdue(config.limits.time.deadline_clock.now_epoch_millis())
        .map_err(|source| {
            RuntimeError::StateUnavailable(
                ErrorContext::new(
                    "durable schedules could not be reconciled",
                    &component_id,
                    AppLifecycle::Loaded,
                    None,
                )
                .with_source(source),
            )
        })?;
    youth_state::execute_wake_outputs(config.limits.time.wake_driver.as_ref(), &outputs);
    crate::host::dispatch_schedule_notifications(
        &outputs,
        config.limits.time.notification_dispatcher.as_ref(),
        &state,
    );
    Ok(())
}

fn worker_main(
    config: YouthAppConfig,
    mut mailbox_rx: mpsc::Receiver<WorkerMessage>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    presentation: PresentationReader,
    init_tx: std::sync::mpsc::SyncSender<Result<(), RuntimeError>>,
) {
    if !matches!(mailbox_rx.blocking_recv(), Some(WorkerMessage::Reconcile)) {
        let _ = init_tx.send(Err(RuntimeError::WorkerStopped(ErrorContext::new(
            "application worker lost its startup reconciliation",
            component_identity(&config.component_path),
            AppLifecycle::Stopped,
            None,
        ))));
        return;
    }
    if let Err(error) = reconcile_without_guest(&config) {
        let _ = init_tx.send(Err(error));
        return;
    }
    let mut app = match crate::YouthApp::load_config_deferred_reconcile(config) {
        Ok(app) => app,
        Err(error) => {
            let _ = init_tx.send(Err(error));
            return;
        }
    };
    if init_tx.send(Ok(())).is_err() {
        return;
    }
    sync_presentation(&app, &presentation);

    while let Some(message) = mailbox_rx.blocking_recv() {
        match message {
            WorkerMessage::Request { command, reply } => {
                if handle_request(&mut app, command, reply, &event_tx, &presentation) {
                    break;
                }
            }
            WorkerMessage::Wake(token) => handle_wake(&mut app, token, &event_tx, &presentation),
            WorkerMessage::Reconcile => {
                if let Err(error) = app.reconcile_schedules() {
                    publish_fault_if_any(&app, &event_tx, &error);
                }
            }
            WorkerMessage::Shutdown => break,
        }
        sync_presentation(&app, &presentation);
    }
}

fn sync_presentation(app: &crate::YouthApp, presentation: &PresentationReader) {
    let records = app
        .tree()
        .into_iter()
        .flat_map(|tree| tree.to_snapshot().nodes)
        .filter_map(|node| {
            node.data
                .countdown_ref()
                .map(|(schedule, _, _)| schedule.id)
        })
        .filter_map(|id| app.schedule(id).ok().flatten().map(|record| (id, record)))
        .collect();
    *presentation
        .records
        .write()
        .expect("presentation-record lock is not poisoned") = records;
}

fn handle_request(
    app: &mut crate::YouthApp,
    command: AppCommand,
    reply: ReplySender,
    event_tx: &broadcast::Sender<RuntimeEvent>,
    presentation: &PresentationReader,
) -> bool {
    match (command, reply) {
        (AppCommand::Mount, ReplySender::Snapshot(reply)) => {
            let result = app.mount();
            sync_presentation(app, presentation);
            let result = if result.is_ok() {
                let delivered = drain_pending_deliveries(app, event_tx, presentation);
                if delivered { app.snapshot() } else { result }
            } else {
                result
            };
            if let Ok(snapshot) = &result {
                let _ = event_tx.send(RuntimeEvent::SnapshotReplaced(snapshot.clone()));
            } else if let Err(error) = &result {
                publish_fault_if_any(app, event_tx, error);
            }
            let _ = reply.send(result);
        }
        (AppCommand::Activate { request_id, node }, ReplySender::Turn(reply)) => {
            let result = app.activate(node);
            sync_presentation(app, presentation);
            if let Ok(receipt) = &result {
                let _ = event_tx.send(RuntimeEvent::TurnCommitted(TurnOutcome {
                    origin: TurnOrigin::Requested(request_id),
                    receipt: receipt.clone(),
                }));
            } else if let Err(error) = &result {
                publish_fault_if_any(app, event_tx, error);
            }
            let _ = reply.send(result);
        }
        (AppCommand::EditorLocalEdit { editor, edit }, ReplySender::EditorLocalEdit(reply)) => {
            // This path deliberately touches only the live host-owned Editor
            // registry. It creates no guest call, tree reconciliation, state
            // transaction, turn event, or turn receipt.
            let _ = reply.send(app.edit_editor_locally(editor, edit));
        }
        (AppCommand::Resync, ReplySender::Snapshot(reply)) => {
            let result = app.resync();
            sync_presentation(app, presentation);
            if let Ok(snapshot) = &result {
                let _ = event_tx.send(RuntimeEvent::SnapshotReplaced(snapshot.clone()));
            } else if let Err(error) = &result {
                publish_fault_if_any(app, event_tx, error);
            }
            let _ = reply.send(result);
        }
        (AppCommand::VerifyView, ReplySender::ViewVerification(reply)) => {
            // Verification is deliberately invisible to observers and
            // presentation. The host-owned authoritative tree is untouched.
            let _ = reply.send(app.verify_view());
        }
        (AppCommand::Snapshot, ReplySender::Snapshot(reply)) => {
            let _ = reply.send(app.snapshot());
        }
        (AppCommand::Inspect, ReplySender::Inspection(reply)) => {
            let _ = reply.send(Ok(app.inspect()));
        }
        #[cfg(feature = "test-support")]
        (AppCommand::FailNextStateCommit, ReplySender::Stop(reply)) => {
            app.fail_next_state_commit();
            let _ = reply.send(Ok(()));
        }
        (AppCommand::Stop, ReplySender::Stop(reply)) => {
            let result = app.stop();
            let _ = reply.send(result);
        }
        _ => unreachable!("request command and reply types are constructed together"),
    }
    false
}

fn handle_wake(
    app: &mut crate::YouthApp,
    token: youth_state::WakeToken,
    event_tx: &broadcast::Sender<RuntimeEvent>,
    presentation: &PresentationReader,
) {
    let wake = ScheduleWake {
        application_id: token.app_id.clone(),
        token,
    };
    let disposition = app.receive_schedule_wake(&wake);
    sync_presentation(app, presentation);
    match disposition {
        Ok(WakeDisposition::Discarded) => {}
        Ok(WakeDisposition::DeliveryQueued) if app.lifecycle() != AppLifecycle::Mounted => {}
        Ok(WakeDisposition::DeliveryQueued) => {
            deliver_one_pending(app, event_tx, presentation);
        }
        Err(error) => publish_fault_if_any(app, event_tx, &error),
    }
}

/// Attempts one queued elapsed delivery, publishing a `TurnCommitted` on
/// success. Returns whether a delivery was actually committed, so a caller
/// (mount's recovery drain, in particular) knows whether the tree it
/// already has is stale and needs re-fetching.
fn deliver_one_pending(
    app: &mut crate::YouthApp,
    event_tx: &broadcast::Sender<RuntimeEvent>,
    presentation: &PresentationReader,
) -> bool {
    let delivery = match app.pending_deliveries() {
        Ok(deliveries) => deliveries.into_iter().next(),
        Err(error) => {
            publish_fault_if_any(app, event_tx, &error);
            return false;
        }
    };
    let Some(delivery) = delivery else {
        return false;
    };
    match app.deliver_next_pending() {
        Ok(Some(receipt)) => {
            sync_presentation(app, presentation);
            let _ = event_tx.send(RuntimeEvent::TurnCommitted(TurnOutcome {
                origin: TurnOrigin::ScheduleElapsed {
                    schedule_id: delivery.schedule_id,
                    generation: delivery.generation,
                },
                receipt,
            }));
            true
        }
        Ok(None) => false,
        Err(error) => {
            publish_fault_if_any(app, event_tx, &error);
            false
        }
    }
}

/// Drains every schedule already queued for elapsed delivery at the moment
/// this application became mounted -- most notably the overdue-on-open
/// case (D4g/Gate C-4): reconciliation queues a delivery for a deadline
/// that already passed while no process was running, but deliberately
/// never forces a guest turn by itself (queueing must not require a
/// guest). Mounting is the first point a guest turn is actually possible,
/// so this is where any such backlog gets a real delivery attempt, exactly
/// once each, stopping at the first failure rather than retrying a fault
/// in a loop. Returns whether at least one delivery committed.
fn drain_pending_deliveries(
    app: &mut crate::YouthApp,
    event_tx: &broadcast::Sender<RuntimeEvent>,
    presentation: &PresentationReader,
) -> bool {
    let mut delivered_any = false;
    loop {
        match app.pending_deliveries() {
            Ok(deliveries) if deliveries.is_empty() => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if !deliver_one_pending(app, event_tx, presentation) {
            break;
        }
        delivered_any = true;
    }
    delivered_any
}

fn publish_fault_if_any(
    app: &crate::YouthApp,
    event_tx: &broadcast::Sender<RuntimeEvent>,
    error: &RuntimeError,
) {
    if error.category() == crate::RuntimeErrorCategory::StateCommitFailed {
        // A commit failure rolled the whole turn back, so observers receive no
        // publication derived from that uncommitted turn.
        return;
    }
    if let Some(fault) = app.inspect().fault {
        let _ = event_tx.send(RuntimeEvent::Faulted(fault));
    }
}

fn component_identity(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.display().to_string(), String::from)
}
