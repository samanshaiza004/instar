use tokio::sync::mpsc;
use youth_runtime::{RuntimeErrorCategory, TurnReceipt, YouthAppHandle};
use youth_tree::{NodeId, TreeSnapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppErrorSummary {
    pub category: RuntimeErrorCategory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeErrorSummary {
    pub category: RuntimeErrorCategory,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DesktopEvent {
    TurnCommitted(TurnReceipt),
    Resynced(TreeSnapshot),
    AppRejected(AppErrorSummary),
    AppFaulted(RuntimeErrorSummary),
    Stopped,
}

pub trait DesktopEventSink: Send + 'static {
    fn send(&self, event: DesktopEvent);
}

impl<F> DesktopEventSink for F
where
    F: Fn(DesktopEvent) + Send + 'static,
{
    fn send(&self, event: DesktopEvent) {
        self(event);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerCommand {
    Activate(NodeId),
    Resync,
    Stop,
}

#[derive(Clone, Debug)]
pub struct Controller {
    sender: mpsc::UnboundedSender<ControllerCommand>,
}

impl Controller {
    pub fn spawn(handle: YouthAppHandle, sink: impl DesktopEventSink) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("youth-desktop-controller".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("controller runtime creation should not fail");
                runtime.block_on(async move {
                    while let Some(command) = receiver.recv().await {
                        match command {
                            ControllerCommand::Activate(node) => {
                                match handle.activate(node).await {
                                    Ok(receipt) => sink.send(DesktopEvent::TurnCommitted(receipt)),
                                    Err(error)
                                        if error.category()
                                            == RuntimeErrorCategory::GuestRejected =>
                                    {
                                        sink.send(DesktopEvent::AppRejected(AppErrorSummary {
                                            category: error.category(),
                                        }));
                                    }
                                    Err(error) => {
                                        sink.send(DesktopEvent::AppFaulted(RuntimeErrorSummary {
                                            category: error.category(),
                                        }));
                                    }
                                }
                            }
                            ControllerCommand::Resync => match handle.snapshot().await {
                                Ok(snapshot) => sink.send(DesktopEvent::Resynced(snapshot)),
                                Err(error) => {
                                    sink.send(DesktopEvent::AppFaulted(RuntimeErrorSummary {
                                        category: error.category(),
                                    }));
                                }
                            },
                            ControllerCommand::Stop => {
                                let _ = handle.stop().await;
                                sink.send(DesktopEvent::Stopped);
                                break;
                            }
                        }
                    }
                });
            })
            .expect("controller thread creation should not fail");
        Self { sender }
    }

    pub fn send(&self, command: ControllerCommand) -> Result<(), ControllerCommand> {
        self.sender.send(command).map_err(|error| error.0)
    }
}
