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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolVersion {
    V002,
    V003,
}

impl ProtocolVersion {
    pub(crate) const fn world(self) -> &'static str {
        match self {
            Self::V002 => "youth:app/application@0.0.2",
            Self::V003 => "youth:app/application@0.0.3",
        }
    }
}

pub(crate) enum ApplicationBindings {
    V002(v002::Application),
    V003(v003::Application),
}

impl ApplicationBindings {
    pub(crate) const fn version(&self) -> ProtocolVersion {
        match self {
            Self::V002(_) => ProtocolVersion::V002,
            Self::V003(_) => ProtocolVersion::V003,
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
        }
    }

    pub(crate) fn call_handle(
        &self,
        store: &mut wasmtime::Store<crate::host::HostState>,
        revision: u64,
        events: &[(u64, u64)],
    ) -> wasmtime::Result<Result<RawPatchBatch, GuestError>> {
        match self {
            Self::V002(bindings) => {
                let events = v002::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events: events
                        .iter()
                        .map(|(sequence, node)| v002::youth::app::ui::Event {
                            sequence: *sequence,
                            kind: v002::youth::app::ui::EventKind::Activate(*node),
                        })
                        .collect(),
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
                let events = v003::youth::app::ui::EventBatch {
                    tree_revision: revision,
                    events: events
                        .iter()
                        .map(|(sequence, node)| v003::youth::app::ui::Event {
                            sequence: *sequence,
                            kind: v003::youth::app::ui::EventKind::Activate(*node),
                        })
                        .collect(),
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
        }
    }
}

pub(crate) enum RawTreeSnapshot {
    V002(v002::youth::app::ui::TreeSnapshot),
    V003(v003::youth::app::ui::TreeSnapshot),
}

pub(crate) enum RawPatchBatch {
    V002(v002::youth::app::ui::PatchBatch),
    V003(v003::youth::app::ui::PatchBatch),
}

impl RawPatchBatch {
    pub(crate) const fn processed_through(&self) -> u64 {
        match self {
            Self::V002(value) => value.processed_through,
            Self::V003(value) => value.processed_through,
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
}
