use tokio::sync::mpsc;
use youth_interaction::{EditorInput, EditorMovement};
use youth_runtime::{Movement, RuntimeErrorCategory, TurnReceipt, YouthAppHandle};
use youth_tree::{NodeId, TreeSnapshot};

const fn to_engine_movement(movement: EditorMovement) -> Movement {
    match movement {
        EditorMovement::Left => Movement::Left,
        EditorMovement::Right => Movement::Right,
        EditorMovement::Up => Movement::Up,
        EditorMovement::Down => Movement::Down,
        EditorMovement::Home => Movement::Home,
        EditorMovement::End => Movement::End,
    }
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControllerCommand {
    Activate(NodeId),
    EditorInput { editor: NodeId, input: EditorInput },
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
                            ControllerCommand::EditorInput { editor, input } => {
                                // `ImeCommit` becomes two local edits (set the
                                // final compose text, then finish composing)
                                // rather than one, since the host primitive
                                // mirrors Parley's own two-step
                                // `set_compose`/`finish_compose` driver calls;
                                // `local_ime_finish_compose` groups them into
                                // one undo unit regardless.
                                let edits: Vec<youth_runtime::EditorLocalEdit> = match input {
                                    EditorInput::InsertText(text) => {
                                        vec![youth_runtime::EditorLocalEdit::InsertText(text)]
                                    }
                                    EditorInput::Backspace => {
                                        vec![youth_runtime::EditorLocalEdit::Backspace]
                                    }
                                    EditorInput::Undo => vec![youth_runtime::EditorLocalEdit::Undo],
                                    EditorInput::Redo => vec![youth_runtime::EditorLocalEdit::Redo],
                                    EditorInput::Paste => {
                                        vec![youth_runtime::EditorLocalEdit::Paste]
                                    }
                                    EditorInput::MoveCursor(movement) => {
                                        vec![youth_runtime::EditorLocalEdit::MoveCursor(
                                            to_engine_movement(movement),
                                        )]
                                    }
                                    EditorInput::ExtendSelection(movement) => {
                                        vec![youth_runtime::EditorLocalEdit::ExtendSelection(
                                            to_engine_movement(movement),
                                        )]
                                    }
                                    EditorInput::ImeSetCompose { text, cursor } => {
                                        vec![youth_runtime::EditorLocalEdit::ImeSetCompose {
                                            text,
                                            cursor,
                                        }]
                                    }
                                    EditorInput::ImeClearCompose => {
                                        vec![youth_runtime::EditorLocalEdit::ImeClearCompose]
                                    }
                                    EditorInput::ImeCommit(text) => vec![
                                        youth_runtime::EditorLocalEdit::ImeSetCompose {
                                            text,
                                            cursor: None,
                                        },
                                        youth_runtime::EditorLocalEdit::ImeFinishCompose,
                                    ],
                                    // No selection-consuming clipboard semantics yet
                                    // -- deferred alongside real Cut/Copy behavior.
                                    EditorInput::Cut | EditorInput::Copy => Vec::new(),
                                };
                                for edit in edits {
                                    if let Err(error) =
                                        handle.edit_editor_locally(editor, edit).await
                                    {
                                        sink.send(DesktopEvent::AppFaulted(RuntimeErrorSummary {
                                            category: error.category(),
                                        }));
                                        break;
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
