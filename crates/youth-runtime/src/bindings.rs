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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolVersion {
    V002,
    V003,
    V004,
    V005,
    V006,
}

impl ProtocolVersion {
    pub(crate) const fn world(self) -> &'static str {
        match self {
            Self::V002 => "youth:app/application@0.0.2",
            Self::V003 => "youth:app/application@0.0.3",
            Self::V004 => "youth:app/application@0.0.4",
            Self::V005 => "youth:app/application@0.0.5",
            Self::V006 => "youth:app/application@0.0.6",
        }
    }
}

pub(crate) enum ApplicationBindings {
    V002(v002::Application),
    V003(v003::Application),
    V004(v004::Application),
    V005(v005::Application),
    V006(v006::Application),
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
}

impl HostEvent {
    pub(crate) const fn sequence(self) -> u64 {
        match self {
            Self::Activate { sequence, .. } | Self::ScheduleElapsed { sequence, .. } => sequence,
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
        }
    }

    pub(crate) fn call_handle(
        &self,
        store: &mut wasmtime::Store<crate::host::HostState>,
        revision: u64,
        events: &[HostEvent],
    ) -> wasmtime::Result<Result<RawPatchBatch, GuestError>> {
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
            HostEvent::ScheduleElapsed { .. } => Err(wasmtime::Error::msg(
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
            HostEvent::ScheduleElapsed { .. } => Err(wasmtime::Error::msg(
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
}

pub(crate) enum RawPatchBatch {
    V002(v002::youth::app::ui::PatchBatch),
    V003(v003::youth::app::ui::PatchBatch),
    V004(v004::youth::app::ui::PatchBatch),
    V005(v005::youth::app::ui::PatchBatch),
    V006(v006::youth::app::ui::PatchBatch),
}

impl RawPatchBatch {
    pub(crate) const fn processed_through(&self) -> u64 {
        match self {
            Self::V002(value) => value.processed_through,
            Self::V003(value) => value.processed_through,
            Self::V004(value) => value.processed_through,
            Self::V005(value) => value.processed_through,
            Self::V006(value) => value.processed_through,
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
}
