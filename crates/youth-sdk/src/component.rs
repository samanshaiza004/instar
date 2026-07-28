use std::cell::RefCell;
use std::marker::PhantomData;

use super::{
    Application, ElapsedReason, Error, ErrorKind, Event, EventContext, Events, FlatNodeData,
    FlatTree, IncomingEvent, ViewContext, decode_incoming_events,
};

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        generate_all,
        pub_export_macro: true,
        default_bindings_module: "youth_sdk::component::bindings",
        world: "application",
        path: "wit",
    });
}

use bindings::exports::youth::app::lifecycle::Guest;
use bindings::youth::app::ui;

#[derive(Default)]
struct LifecycleState {
    mounted: bool,
    revision: u64,
    tree: Option<FlatTree>,
}

thread_local! {
    static STATE: RefCell<LifecycleState> = RefCell::new(LifecycleState::default());
}

pub struct Adapter<A>(PhantomData<A>);

impl<A: Application> Guest for Adapter<A> {
    fn mount() -> std::result::Result<ui::TreeSnapshot, ui::AppError> {
        with_state(|state| {
            if state.mounted {
                return Err(Error::invalid_state());
            }
            let tree = A::view(&ViewContext)?.flatten()?;
            let snapshot = snapshot(&tree, 0);
            state.mounted = true;
            state.revision = 0;
            state.tree = Some(tree);
            Ok(snapshot)
        })
        .map_err(wire_error)
    }

    fn handle(events: ui::EventBatch) -> std::result::Result<ui::PatchBatch, ui::AppError> {
        with_state(|state| {
            if !state.mounted || events.tree_revision != state.revision {
                return Err(Error::invalid_state());
            }
            let incoming = events.events.iter().map(decode_event).collect::<Vec<_>>();
            let decoded = decode_incoming_events(incoming)?;
            let processed_through = events.events.last().map_or(0, |event| event.sequence);
            let tree = state.tree.as_ref().ok_or_else(Error::invalid_state)?;
            let commanded = decoded
                .iter()
                .filter_map(|event| match event {
                    Event::Activated(id) => Some(*id),
                    Event::ScheduleElapsed { .. } => None,
                })
                .filter_map(|id| {
                    tree.nodes.iter().find_map(|node| {
                        if node.id != id {
                            return None;
                        }
                        match node.data {
                            FlatNodeData::Button {
                                command: Some(command),
                                ..
                            } => Some(command.id()),
                            _ => None,
                        }
                    })
                })
                .collect();
            let events = Events {
                events: decoded,
                commanded,
            };
            let update = A::handle(&mut EventContext, &events)?;
            let tree = state.tree.as_mut().ok_or_else(Error::invalid_state)?;
            tree.apply(&update)?;
            let base = state.revision;
            let next = if update.operations.is_empty() {
                base
            } else {
                base.checked_add(1).ok_or_else(Error::internal)?
            };
            let patches = update.operations.into_iter().map(wire_patch).collect();
            state.revision = next;
            Ok(ui::PatchBatch {
                base_tree_revision: base,
                next_tree_revision: next,
                processed_through,
                patches,
            })
        })
        .map_err(wire_error)
    }

    fn resync() -> std::result::Result<ui::TreeSnapshot, ui::AppError> {
        with_state(|state| {
            if !state.mounted {
                return Err(Error::invalid_state());
            }
            let tree = A::view(&ViewContext)?.flatten()?;
            let snapshot = snapshot(&tree, state.revision);
            state.tree = Some(tree);
            Ok(snapshot)
        })
        .map_err(wire_error)
    }
}

fn decode_event(event: &ui::Event) -> IncomingEvent {
    match &event.kind {
        ui::EventKind::Activate(id) => IncomingEvent::Activated(*id),
        ui::EventKind::ScheduleElapsed(elapsed) => IncomingEvent::ScheduleElapsed {
            schedule: elapsed.id,
            generation: elapsed.generation,
            reason: match elapsed.reason {
                ui::ElapsedReason::Deadline => ElapsedReason::Deadline,
                ui::ElapsedReason::RecoveredOverdue => ElapsedReason::RecoveredOverdue,
            },
        },
    }
}

fn with_state<T>(
    operation: impl FnOnce(&mut LifecycleState) -> super::Result<T>,
) -> super::Result<T> {
    STATE.with(|state| {
        let mut state = state.try_borrow_mut().map_err(|_| Error::internal())?;
        operation(&mut state)
    })
}

fn snapshot(tree: &FlatTree, revision: u64) -> ui::TreeSnapshot {
    ui::TreeSnapshot {
        revision,
        root: tree.root,
        nodes: tree
            .nodes
            .iter()
            .map(|node| ui::Node {
                id: node.id,
                data: match &node.data {
                    FlatNodeData::Root => ui::NodeData::Root,
                    FlatNodeData::Box { enabled, layout } => ui::NodeData::Box(ui::BoxData {
                        enabled: *enabled,
                        layout: match layout {
                            super::Layout::Column => ui::BoxLayout::Column,
                            super::Layout::Row => ui::BoxLayout::Row,
                            super::Layout::Grid(columns) => {
                                ui::BoxLayout::Grid(ui::GridLayout { columns: *columns })
                            }
                        },
                    }),
                    FlatNodeData::Text { value, alignment } => ui::NodeData::Text(ui::TextData {
                        content: ui::TextContent::Literal(value.clone()),
                        alignment: match alignment {
                            super::TextAlign::Start => ui::TextAlignment::Start,
                            super::TextAlign::Center => ui::TextAlignment::Center,
                            super::TextAlign::End => ui::TextAlignment::End,
                        },
                    }),
                    FlatNodeData::Countdown {
                        schedule,
                        precision,
                        format,
                        alignment,
                    } => ui::NodeData::Text(ui::TextData {
                        content: ui::TextContent::Countdown(ui::CountdownData {
                            schedule: ui::ScheduleRef {
                                id: schedule.id(),
                                generation: schedule.generation(),
                            },
                            precision: wire_precision(*precision),
                            format: wire_format(*format),
                        }),
                        alignment: match alignment {
                            super::TextAlign::Start => ui::TextAlignment::Start,
                            super::TextAlign::Center => ui::TextAlignment::Center,
                            super::TextAlign::End => ui::TextAlignment::End,
                        },
                    }),
                    FlatNodeData::Button {
                        label,
                        enabled,
                        shortcuts,
                        ..
                    } => ui::NodeData::Button(ui::ButtonData {
                        label: label.clone(),
                        enabled: *enabled,
                        shortcuts: shortcuts.iter().copied().map(wire_shortcut).collect(),
                    }),
                },
                children: node.children.clone(),
            })
            .collect(),
    }
}

fn wire_precision(precision: super::TimePrecision) -> ui::TimePrecision {
    match precision {
        super::TimePrecision::Seconds => ui::TimePrecision::Seconds,
    }
}

fn wire_format(format: super::CountdownFormat) -> ui::CountdownFormat {
    match format {
        super::CountdownFormat::MinutesSeconds => ui::CountdownFormat::MinutesSeconds,
    }
}

fn wire_shortcut(shortcut: super::Shortcut) -> ui::ShortcutKey {
    match shortcut {
        super::Shortcut::Character(value) => ui::ShortcutKey::Character(value.to_string()),
        super::Shortcut::Enter => ui::ShortcutKey::Enter,
        super::Shortcut::Escape => ui::ShortcutKey::Escape,
        super::Shortcut::Backspace => ui::ShortcutKey::Backspace,
    }
}

fn wire_patch(operation: super::UpdateOperation) -> ui::Patch {
    match operation {
        super::UpdateOperation::Text(key, value) => ui::Patch::SetText(ui::SetText {
            id: key.id(),
            value: ui::TextContent::Literal(value),
        }),
        super::UpdateOperation::Countdown(key, schedule, precision, format) => {
            ui::Patch::SetText(ui::SetText {
                id: key.id(),
                value: ui::TextContent::Countdown(ui::CountdownData {
                    schedule: ui::ScheduleRef {
                        id: schedule.id(),
                        generation: schedule.generation(),
                    },
                    precision: wire_precision(precision),
                    format: wire_format(format),
                }),
            })
        }
        super::UpdateOperation::Label(key, value) => ui::Patch::SetLabel(ui::SetLabel {
            id: key.id(),
            value,
        }),
        super::UpdateOperation::Enabled(key, enabled) => ui::Patch::SetEnabled(ui::SetEnabled {
            id: key.id(),
            value: enabled,
        }),
    }
}

fn wire_error(error: Error) -> ui::AppError {
    let code = match error.kind() {
        ErrorKind::InvalidState => ui::AppErrorCode::InvalidState,
        ErrorKind::RejectedEvent => ui::AppErrorCode::RejectedEvent,
        ErrorKind::Internal => ui::AppErrorCode::Internal,
    };
    ui::AppError {
        code,
        message: error.message,
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! __export_adapter {
    ($adapter:ident) => {
        $crate::component::bindings::export!($adapter);
    };
}
