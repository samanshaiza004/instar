pub(crate) mod v002 {
    wasmtime::component::bindgen!({
        path: "../../wit/youth-app",
        world: "application",
    });
}

pub(crate) mod v003 {
    wasmtime::component::bindgen!({
        path: "../../wit/youth-app-v0.0.3",
        world: "application",
    });
}

pub(crate) mod v004 {
    wasmtime::component::bindgen!({
        path: "../../wit/youth-app-v0.0.4",
        world: "application",
    });
}

pub(crate) mod v005 {
    wasmtime::component::bindgen!({
        path: "../../wit/youth-app-v0.0.5",
        world: "application",
    });
}

pub(crate) mod v006 {
    wasmtime::component::bindgen!({
        path: "../../wit/youth-app-v0.0.6",
        world: "application",
    });
}

pub(crate) mod v007 {
    wasmtime::component::bindgen!({
        path: "../../wit/youth-app-v0.0.7",
        world: "application",
    });
}

pub(crate) mod v008 {
    wasmtime::component::bindgen!({
        path: "../../wit/youth-app-v0.0.8",
        world: "application",
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolVersion {
    V002,
    V003,
    V004,
    V005,
    V006,
    V007,
    V008,
}

impl ProtocolVersion {
    pub(crate) const fn world(self) -> &'static str {
        match self {
            Self::V002 => "youth:app/application@0.0.2",
            Self::V003 => "youth:app/application@0.0.3",
            Self::V004 => "youth:app/application@0.0.4",
            Self::V005 => "youth:app/application@0.0.5",
            Self::V006 => "youth:app/application@0.0.6",
            Self::V007 => "youth:app/application@0.0.7",
            Self::V008 => "youth:app/application@0.0.8",
        }
    }
}

pub(crate) enum ApplicationBindings {
    V002(v002::Application),
    V003(v003::Application),
    V004(v004::Application),
    V005(v005::Application),
    V006(v006::Application),
    V007(v007::Application),
    V008(v008::Application),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostEvent {
    Activate {
        sequence: u64,
        node: u64,
    },
    ScheduleElapsed {
        sequence: u64,
        schedule: u64,
        generation: u64,
        reason: youth_state::ElapsedReason,
    },
    EditorDirtyChanged {
        sequence: u64,
        editor: u64,
        dirty: bool,
    },
    TextDocumentSaveCompleted {
        sequence: u64,
        completion: crate::text_document::SaveCompletion,
    },
}

impl HostEvent {
    pub(crate) const fn sequence(self) -> u64 {
        match self {
            Self::Activate { sequence, .. }
            | Self::ScheduleElapsed { sequence, .. }
            | Self::EditorDirtyChanged { sequence, .. }
            | Self::TextDocumentSaveCompleted { sequence, .. } => sequence,
        }
    }
}

impl ApplicationBindings {
    pub(crate) const fn version(&self) -> ProtocolVersion {
        match self {
            Self::V002(_) => ProtocolVersion::V002,
            Self::V003(_) => ProtocolVersion::V003,
            Self::V004(_) => ProtocolVersion::V004,
            Self::V005(_) => ProtocolVersion::V005,
            Self::V006(_) => ProtocolVersion::V006,
            Self::V007(_) => ProtocolVersion::V007,
            Self::V008(_) => ProtocolVersion::V008,
        }
    }

    pub(crate) fn call_mount(
        &self,
        store: &mut wasmtime::Store<crate::host::HostState>,
    ) -> wasmtime::Result<Result<RawTreeSnapshot, GuestError>> {
        match self {
            Self::V002(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_mount(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V002)
                            .map_err(GuestError::from_v002)
                    })
            }
            Self::V003(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_mount(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V003)
                            .map_err(GuestError::from_v003)
                    })
            }
            Self::V004(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_mount(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V004)
                            .map_err(GuestError::from_v004)
                    })
            }
            Self::V005(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_mount(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V005)
                            .map_err(GuestError::from_v005)
                    })
            }
            Self::V006(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_mount(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V006)
                            .map_err(GuestError::from_v006)
                    })
            }
            Self::V007(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_mount(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V007)
                            .map_err(GuestError::from_v007)
                    })
            }
            Self::V008(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_mount(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V008)
                            .map_err(GuestError::from_v008)
                    })
            }
        }
    }

    pub(crate) fn call_handle(
        &self,
        store: &mut wasmtime::Store<crate::host::HostState>,
        revision: u64,
        events: &[HostEvent],
    ) -> wasmtime::Result<Result<RawPatchBatch, GuestError>> {
        if !matches!(self, Self::V008(_))
            && events.iter().any(|event| {
                matches!(
                    event,
                    HostEvent::EditorDirtyChanged { .. }
                        | HostEvent::TextDocumentSaveCompleted { .. }
                )
            })
        {
            return Err(wasmtime::Error::msg(
                "protocols before 0.0.8 cannot represent text-document events",
            ));
        }
        match self {
            Self::V002(bindings) => {
                let events = activation_events_v002(events)?;
                let events = v002::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events,
                };
                bindings
                    .youth_app_lifecycle()
                    .call_handle(store, &events)
                    .map(|result| {
                        result
                            .map(RawPatchBatch::V002)
                            .map_err(GuestError::from_v002)
                    })
            }
            Self::V003(bindings) => {
                let events = activation_events_v003(events)?;
                let events = v003::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events,
                };
                bindings
                    .youth_app_lifecycle()
                    .call_handle(store, &events)
                    .map(|result| {
                        result
                            .map(RawPatchBatch::V003)
                            .map_err(GuestError::from_v003)
                    })
            }
            Self::V004(bindings) => {
                let events = v004::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events: events
                        .iter()
                        .map(|event| v004::youth::app::ui::Event {
                            sequence: event.sequence(),
                            kind: match *event {
                                HostEvent::Activate { node, .. } => {
                                    v004::youth::app::ui::EventKind::Activate(node)
                                }
                                HostEvent::ScheduleElapsed {
                                    schedule,
                                    generation,
                                    reason,
                                    ..
                                } => v004::youth::app::ui::EventKind::ScheduleElapsed(
                                    v004::youth::app::ui::ElapsedSchedule {
                                        id: schedule,
                                        generation,
                                        reason: match reason {
                                            youth_state::ElapsedReason::Deadline => {
                                                v004::youth::app::ui::ElapsedReason::Deadline
                                            }
                                            youth_state::ElapsedReason::RecoveredOverdue => {
                                                v004::youth::app::ui::ElapsedReason::RecoveredOverdue
                                            }
                                        },
                                    },
                                ),
                                HostEvent::EditorDirtyChanged { .. }
                                | HostEvent::TextDocumentSaveCompleted { .. } => {
                                    unreachable!("text-document events were rejected above")
                                }
                            },
                        })
                        .collect(),
                };
                bindings
                    .youth_app_lifecycle()
                    .call_handle(store, &events)
                    .map(|result| {
                        result
                            .map(RawPatchBatch::V004)
                            .map_err(GuestError::from_v004)
                    })
            }
            Self::V005(bindings) => {
                let events = v005::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events: events
                        .iter()
                        .map(|event| v005::youth::app::ui::Event {
                            sequence: event.sequence(),
                            kind: match *event {
                                HostEvent::Activate { node, .. } => {
                                    v005::youth::app::ui::EventKind::Activate(node)
                                }
                                HostEvent::ScheduleElapsed {
                                    schedule,
                                    generation,
                                    reason,
                                    ..
                                } => v005::youth::app::ui::EventKind::ScheduleElapsed(
                                    v005::youth::app::ui::ElapsedSchedule {
                                        id: schedule,
                                        generation,
                                        reason: match reason {
                                            youth_state::ElapsedReason::Deadline => {
                                                v005::youth::app::ui::ElapsedReason::Deadline
                                            }
                                            youth_state::ElapsedReason::RecoveredOverdue => {
                                                v005::youth::app::ui::ElapsedReason::RecoveredOverdue
                                            }
                                        },
                                    },
                                ),
                                HostEvent::EditorDirtyChanged { .. }
                                | HostEvent::TextDocumentSaveCompleted { .. } => {
                                    unreachable!("text-document events were rejected above")
                                }
                            },
                        })
                        .collect(),
                };
                bindings
                    .youth_app_lifecycle()
                    .call_handle(store, &events)
                    .map(|result| {
                        result
                            .map(RawPatchBatch::V005)
                            .map_err(GuestError::from_v005)
                    })
            }
            Self::V006(bindings) => {
                let events = v006::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events: events
                        .iter()
                        .map(|event| v006::youth::app::ui::Event {
                            sequence: event.sequence(),
                            kind: match *event {
                                HostEvent::Activate { node, .. } => {
                                    v006::youth::app::ui::EventKind::Activate(node)
                                }
                                HostEvent::ScheduleElapsed {
                                    schedule,
                                    generation,
                                    reason,
                                    ..
                                } => v006::youth::app::ui::EventKind::ScheduleElapsed(
                                    v006::youth::app::ui::ElapsedSchedule {
                                        id: schedule,
                                        generation,
                                        reason: match reason {
                                            youth_state::ElapsedReason::Deadline => {
                                                v006::youth::app::ui::ElapsedReason::Deadline
                                            }
                                            youth_state::ElapsedReason::RecoveredOverdue => {
                                                v006::youth::app::ui::ElapsedReason::RecoveredOverdue
                                            }
                                        },
                                    },
                                ),
                                HostEvent::EditorDirtyChanged { .. }
                                | HostEvent::TextDocumentSaveCompleted { .. } => {
                                    unreachable!("text-document events were rejected above")
                                }
                            },
                        })
                        .collect(),
                };
                bindings
                    .youth_app_lifecycle()
                    .call_handle(store, &events)
                    .map(|result| {
                        result
                            .map(RawPatchBatch::V006)
                            .map_err(GuestError::from_v006)
                    })
            }
            Self::V007(bindings) => {
                let events = v007::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events: events
                        .iter()
                        .map(|event| v007::youth::app::ui::Event {
                            sequence: event.sequence(),
                            kind: match *event {
                                HostEvent::Activate { node, .. } => {
                                    v007::youth::app::ui::EventKind::Activate(node)
                                }
                                HostEvent::ScheduleElapsed {
                                    schedule,
                                    generation,
                                    reason,
                                    ..
                                } => v007::youth::app::ui::EventKind::ScheduleElapsed(
                                    v007::youth::app::ui::ElapsedSchedule {
                                        id: schedule,
                                        generation,
                                        reason: match reason {
                                            youth_state::ElapsedReason::Deadline => {
                                                v007::youth::app::ui::ElapsedReason::Deadline
                                            }
                                            youth_state::ElapsedReason::RecoveredOverdue => {
                                                v007::youth::app::ui::ElapsedReason::RecoveredOverdue
                                            }
                                        },
                                    },
                                ),
                                HostEvent::EditorDirtyChanged { .. }
                                | HostEvent::TextDocumentSaveCompleted { .. } => {
                                    unreachable!("text-document events were rejected above")
                                }
                            },
                        })
                        .collect(),
                };
                bindings
                    .youth_app_lifecycle()
                    .call_handle(store, &events)
                    .map(|result| {
                        result
                            .map(RawPatchBatch::V007)
                            .map_err(GuestError::from_v007)
                    })
            }
            Self::V008(bindings) => {
                let events = v008::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events: events
                        .iter()
                        .map(event_v008)
                        .collect::<wasmtime::Result<Vec<_>>>()?,
                };
                bindings
                    .youth_app_lifecycle()
                    .call_handle(store, &events)
                    .map(|result| {
                        result
                            .map(RawPatchBatch::V008)
                            .map_err(GuestError::from_v008)
                    })
            }
        }
    }

    pub(crate) fn call_resync(
        &self,
        store: &mut wasmtime::Store<crate::host::HostState>,
    ) -> wasmtime::Result<Result<RawTreeSnapshot, GuestError>> {
        match self {
            Self::V002(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_resync(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V002)
                            .map_err(GuestError::from_v002)
                    })
            }
            Self::V003(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_resync(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V003)
                            .map_err(GuestError::from_v003)
                    })
            }
            Self::V004(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_resync(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V004)
                            .map_err(GuestError::from_v004)
                    })
            }
            Self::V005(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_resync(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V005)
                            .map_err(GuestError::from_v005)
                    })
            }
            Self::V006(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_resync(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V006)
                            .map_err(GuestError::from_v006)
                    })
            }
            Self::V007(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_resync(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V007)
                            .map_err(GuestError::from_v007)
                    })
            }
            Self::V008(bindings) => {
                bindings
                    .youth_app_lifecycle()
                    .call_resync(store)
                    .map(|result| {
                        result
                            .map(RawTreeSnapshot::V008)
                            .map_err(GuestError::from_v008)
                    })
            }
        }
    }
}

fn activation_events_v002(
    events: &[HostEvent],
) -> wasmtime::Result<Vec<v002::youth::app::ui::Event>> {
    events
        .iter()
        .map(|event| match *event {
            HostEvent::Activate { sequence, node } => Ok(v002::youth::app::ui::Event {
                sequence,
                kind: v002::youth::app::ui::EventKind::Activate(node),
            }),
            HostEvent::ScheduleElapsed { .. }
            | HostEvent::EditorDirtyChanged { .. }
            | HostEvent::TextDocumentSaveCompleted { .. } => Err(wasmtime::Error::msg(
                "protocol 0.0.2 cannot represent schedule-elapsed events",
            )),
        })
        .collect()
}

fn activation_events_v003(
    events: &[HostEvent],
) -> wasmtime::Result<Vec<v003::youth::app::ui::Event>> {
    events
        .iter()
        .map(|event| match *event {
            HostEvent::Activate { sequence, node } => Ok(v003::youth::app::ui::Event {
                sequence,
                kind: v003::youth::app::ui::EventKind::Activate(node),
            }),
            HostEvent::ScheduleElapsed { .. }
            | HostEvent::EditorDirtyChanged { .. }
            | HostEvent::TextDocumentSaveCompleted { .. } => Err(wasmtime::Error::msg(
                "protocol 0.0.3 cannot represent schedule-elapsed events",
            )),
        })
        .collect()
}

pub(crate) enum RawTreeSnapshot {
    V002(v002::youth::app::ui::TreeSnapshot),
    V003(v003::youth::app::ui::TreeSnapshot),
    V004(v004::youth::app::ui::TreeSnapshot),
    V005(v005::youth::app::ui::TreeSnapshot),
    V006(v006::youth::app::ui::TreeSnapshot),
    V007(v007::youth::app::ui::TreeSnapshot),
    V008(v008::youth::app::ui::TreeSnapshot),
}

pub(crate) enum RawPatchBatch {
    V002(v002::youth::app::ui::PatchBatch),
    V003(v003::youth::app::ui::PatchBatch),
    V004(v004::youth::app::ui::PatchBatch),
    V005(v005::youth::app::ui::PatchBatch),
    V006(v006::youth::app::ui::PatchBatch),
    V007(v007::youth::app::ui::PatchBatch),
    V008(v008::youth::app::ui::PatchBatch),
}

impl RawPatchBatch {
    pub(crate) const fn processed_through(&self) -> u64 {
        match self {
            Self::V002(value) => value.processed_through,
            Self::V003(value) => value.processed_through,
            Self::V004(value) => value.processed_through,
            Self::V005(value) => value.processed_through,
            Self::V006(value) => value.processed_through,
            Self::V007(value) => value.processed_through,
            Self::V008(value) => value.processed_through,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuestErrorCode {
    InvalidState,
    RejectedEvent,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuestError {
    pub(crate) code: GuestErrorCode,
    pub(crate) message: Option<String>,
}

impl GuestError {
    fn from_v002(value: v002::youth::app::ui::AppError) -> Self {
        use v002::youth::app::ui::AppErrorCode;
        Self {
            code: match value.code {
                AppErrorCode::InvalidState => GuestErrorCode::InvalidState,
                AppErrorCode::RejectedEvent => GuestErrorCode::RejectedEvent,
                AppErrorCode::Internal => GuestErrorCode::Internal,
            },
            message: value.message,
        }
    }

    fn from_v003(value: v003::youth::app::ui::AppError) -> Self {
        use v003::youth::app::ui::AppErrorCode;
        Self {
            code: match value.code {
                AppErrorCode::InvalidState => GuestErrorCode::InvalidState,
                AppErrorCode::RejectedEvent => GuestErrorCode::RejectedEvent,
                AppErrorCode::Internal => GuestErrorCode::Internal,
            },
            message: value.message,
        }
    }

    fn from_v004(value: v004::youth::app::ui::AppError) -> Self {
        use v004::youth::app::ui::AppErrorCode;
        Self {
            code: match value.code {
                AppErrorCode::InvalidState => GuestErrorCode::InvalidState,
                AppErrorCode::RejectedEvent => GuestErrorCode::RejectedEvent,
                AppErrorCode::Internal => GuestErrorCode::Internal,
            },
            message: value.message,
        }
    }

    fn from_v005(value: v005::youth::app::ui::AppError) -> Self {
        use v005::youth::app::ui::AppErrorCode;
        Self {
            code: match value.code {
                AppErrorCode::InvalidState => GuestErrorCode::InvalidState,
                AppErrorCode::RejectedEvent => GuestErrorCode::RejectedEvent,
                AppErrorCode::Internal => GuestErrorCode::Internal,
            },
            message: value.message,
        }
    }

    fn from_v006(value: v006::youth::app::ui::AppError) -> Self {
        use v006::youth::app::ui::AppErrorCode;
        Self {
            code: match value.code {
                AppErrorCode::InvalidState => GuestErrorCode::InvalidState,
                AppErrorCode::RejectedEvent => GuestErrorCode::RejectedEvent,
                AppErrorCode::Internal => GuestErrorCode::Internal,
            },
            message: value.message,
        }
    }

    fn from_v007(value: v007::youth::app::ui::AppError) -> Self {
        use v007::youth::app::ui::AppErrorCode;
        Self {
            code: match value.code {
                AppErrorCode::InvalidState => GuestErrorCode::InvalidState,
                AppErrorCode::RejectedEvent => GuestErrorCode::RejectedEvent,
                AppErrorCode::Internal => GuestErrorCode::Internal,
            },
            message: value.message,
        }
    }

    fn from_v008(value: v008::youth::app::ui::AppError) -> Self {
        use v008::youth::app::ui::AppErrorCode;
        Self {
            code: match value.code {
                AppErrorCode::InvalidState => GuestErrorCode::InvalidState,
                AppErrorCode::RejectedEvent => GuestErrorCode::RejectedEvent,
                AppErrorCode::Internal => GuestErrorCode::Internal,
            },
            message: value.message,
        }
    }
}

fn event_v008(event: &HostEvent) -> wasmtime::Result<v008::youth::app::ui::Event> {
    use v008::youth::app::ui;
    let kind = match event {
        HostEvent::Activate { node, .. } => ui::EventKind::Activate(*node),
        HostEvent::ScheduleElapsed {
            schedule,
            generation,
            reason,
            ..
        } => ui::EventKind::ScheduleElapsed(ui::ElapsedSchedule {
            id: *schedule,
            generation: *generation,
            reason: match reason {
                youth_state::ElapsedReason::Deadline => ui::ElapsedReason::Deadline,
                youth_state::ElapsedReason::RecoveredOverdue => ui::ElapsedReason::RecoveredOverdue,
            },
        }),
        HostEvent::EditorDirtyChanged { editor, dirty, .. } => {
            ui::EventKind::EditorDirtyChanged(ui::EditorDirtyChange {
                editor: *editor,
                dirty: *dirty,
            })
        }
        HostEvent::TextDocumentSaveCompleted { completion, .. } => {
            ui::EventKind::TextDocumentSaveCompleted(completion.as_wire_v008())
        }
    };
    Ok(ui::Event {
        sequence: event.sequence(),
        kind,
    })
}
