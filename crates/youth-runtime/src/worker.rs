use std::path::Path;

use tokio::sync::{mpsc, oneshot};

use crate::{
    AppInspection, AppLifecycle, ErrorContext, RuntimeError, RuntimeLimits, TurnReceipt,
    YouthAppConfig,
};

const COMMAND_CAPACITY: usize = 64;

enum AppCommand {
    Mount(oneshot::Sender<Result<youth_tree::TreeSnapshot, RuntimeError>>),
    Activate {
        node: youth_tree::NodeId,
        reply: oneshot::Sender<Result<TurnReceipt, RuntimeError>>,
    },
    Resync(oneshot::Sender<Result<youth_tree::TreeSnapshot, RuntimeError>>),
    Snapshot(oneshot::Sender<Result<youth_tree::TreeSnapshot, RuntimeError>>),
    Inspect(oneshot::Sender<Result<AppInspection, RuntimeError>>),
    Stop(oneshot::Sender<Result<(), RuntimeError>>),
}

/// An asynchronous, cloneable connection to one serialized application worker.
#[derive(Clone, Debug)]
pub struct YouthAppHandle {
    component_id: String,
    command_tx: mpsc::Sender<AppCommand>,
}

impl YouthAppHandle {
    /// Starts a configured dedicated application worker.
    pub fn spawn(config: YouthAppConfig) -> Result<Self, RuntimeError> {
        let component_id = component_identity(&config.component_path);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel(1);
        let thread_component_id = component_id.clone();
        std::thread::Builder::new()
            .name(format!("youth-app-{thread_component_id}"))
            .spawn(move || worker_main(config, command_rx, init_tx))
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
            command_tx,
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

    pub async fn mount(&self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(AppCommand::Mount(reply))
            .await
            .map_err(|_| self.worker_stopped())?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn activate(&self, node: youth_tree::NodeId) -> Result<TurnReceipt, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(AppCommand::Activate { node, reply })
            .await
            .map_err(|_| self.worker_stopped())?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn resync(&self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(AppCommand::Resync(reply))
            .await
            .map_err(|_| self.worker_stopped())?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn snapshot(&self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(AppCommand::Snapshot(reply))
            .await
            .map_err(|_| self.worker_stopped())?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn inspect(&self) -> Result<AppInspection, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(AppCommand::Inspect(reply))
            .await
            .map_err(|_| self.worker_stopped())?;
        response.await.map_err(|_| self.worker_stopped())?
    }

    pub async fn stop(&self) -> Result<(), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.command_tx
            .send(AppCommand::Stop(reply))
            .await
            .map_err(|_| self.worker_stopped())?;
        response.await.map_err(|_| self.worker_stopped())?
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

fn worker_main(
    config: YouthAppConfig,
    mut command_rx: mpsc::Receiver<AppCommand>,
    init_tx: std::sync::mpsc::SyncSender<Result<(), RuntimeError>>,
) {
    let mut app = match crate::YouthApp::load_config(config) {
        Ok(app) => app,
        Err(error) => {
            let _ = init_tx.send(Err(error));
            return;
        }
    };
    if init_tx.send(Ok(())).is_err() {
        return;
    }

    while let Some(command) = command_rx.blocking_recv() {
        match command {
            AppCommand::Mount(reply) => {
                let _ = reply.send(app.mount());
            }
            AppCommand::Activate { node, reply } => {
                let _ = reply.send(app.activate(node));
            }
            AppCommand::Resync(reply) => {
                let _ = reply.send(app.resync());
            }
            AppCommand::Snapshot(reply) => {
                let _ = reply.send(app.snapshot());
            }
            AppCommand::Inspect(reply) => {
                let _ = reply.send(Ok(app.inspect()));
            }
            AppCommand::Stop(reply) => {
                let result = app.stop();
                let should_stop = result.is_ok();
                let _ = reply.send(result);
                if should_stop {
                    break;
                }
            }
        }
    }
}

fn component_identity(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.display().to_string(), String::from)
}
