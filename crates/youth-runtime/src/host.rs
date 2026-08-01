use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::info_span;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, ResourceLimiter, Store};
use wasmtime_wasi::clocks::HostMonotonicClock;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::{
    ApplicationBindings, GuestError, GuestErrorCode, HostEvent, ProtocolVersion,
};
use crate::engine::deadline_ticks;
use crate::error::ErrorContext;
use crate::wire::from_guest::{self, WireErrorKind};
use crate::{
    CallBudget, RuntimeError, RuntimeErrorCategory, RuntimeLimits, StateLocation, YouthAppConfig,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppLifecycle {
    Loaded,
    Mounted,
    Faulted,
    Stopped,
}

impl fmt::Display for AppLifecycle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Loaded => "loaded",
            Self::Mounted => "mounted",
            Self::Faulted => "faulted",
            Self::Stopped => "stopped",
        })
    }
}

/// Metrics and protocol coordinates for a committed event turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnReceipt {
    pub turn_id: u64,
    pub first_event_sequence: u64,
    pub processed_through: u64,
    pub event_sequence: u64,
    pub base_revision: u64,
    pub next_revision: u64,
    pub patch_batch: youth_tree::PatchBatch,
    pub patch_count: usize,
    pub state_writes: u32,
    pub elapsed: std::time::Duration,
    pub committed: bool,
}

/// Stable information retained when an application becomes faulted.
#[derive(Clone, Debug)]
pub struct AppFault {
    pub category: RuntimeErrorCategory,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleWake {
    pub application_id: youth_state::AppId,
    pub token: youth_state::WakeToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeDisposition {
    DeliveryQueued,
    Discarded,
}

/// A synchronous snapshot of host-owned application state.
#[derive(Clone, Debug)]
pub struct AppInspection {
    pub app_id: String,
    pub state_location: &'static str,
    pub state_summary: Option<youth_state::StateSummary>,
    pub state_transaction_active: bool,
    pub state_phase: youth_state::GuestCallPhase,
    pub state_metrics: youth_state::TurnStateMetrics,
    pub lifecycle: AppLifecycle,
    pub world: String,
    pub current_revision: Option<u64>,
    pub next_event_sequence: Option<u64>,
    pub last_event_sequence: Option<u64>,
    pub node_count: usize,
    pub depth: usize,
    pub last_turn: Option<TurnReceipt>,
    /// Number of lifecycle guest calls made after component instantiation.
    pub guest_call_count: u64,
    pub fault: Option<AppFault>,
    pub canonical_tree: String,
}

/// Atomic retained/reconstructed snapshot pair produced only for test tooling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewVerification {
    pub retained: youth_tree::TreeSnapshot,
    pub reconstructed: youth_tree::TreeSnapshot,
}

struct MemoryLimiter {
    maximum: usize,
    max_table_elements: usize,
    limit_hit: bool,
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = desired <= self.maximum;
        self.limit_hit |= !allowed;
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = desired <= self.max_table_elements;
        self.limit_hit |= !allowed;
        Ok(allowed)
    }
}

pub(crate) struct HostState {
    table: ResourceTable,
    wasi: WasiCtx,
    limiter: MemoryLimiter,
    state: youth_state::StateStore,
    deadline_clock: Arc<dyn youth_state::DeadlineClock>,
    wake_driver: Arc<dyn youth_state::WakeDriver>,
    notification_dispatcher: Arc<dyn crate::NotificationDispatcher>,
    clipboard_service: Arc<dyn crate::ClipboardService>,
    staged_schedule_outputs: Vec<youth_state::SchedulerOutput>,
    editor_sessions: crate::editor_session::EditorSessionRegistry,
    staged_editor_sessions: Option<crate::editor_session::EditorSessionRegistry>,
}

struct GuestClockAdapter(Arc<dyn crate::GuestMonotonicClock>);

impl HostMonotonicClock for GuestClockAdapter {
    fn resolution(&self) -> u64 {
        self.0.resolution_nanoseconds()
    }

    fn now(&self) -> u64 {
        self.0.now_nanoseconds()
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl HostState {
    fn apply_committed_schedule_outputs(&mut self) {
        youth_state::execute_wake_outputs(self.wake_driver.as_ref(), &self.staged_schedule_outputs);
        dispatch_schedule_notifications(
            &self.staged_schedule_outputs,
            self.notification_dispatcher.as_ref(),
            &self.state,
        );
        self.staged_schedule_outputs.clear();
    }

    fn discard_staged_schedule_outputs(&mut self) {
        self.staged_schedule_outputs.clear();
    }

    fn editor_sessions_for_call(&self) -> &crate::editor_session::EditorSessionRegistry {
        self.staged_editor_sessions
            .as_ref()
            .unwrap_or(&self.editor_sessions)
    }

    /// As [`Self::editor_sessions_for_call`], but mutable. Only needed by
    /// calls whose read requires refreshing the engine's internal layout
    /// cache (e.g. `snapshot`); does not itself stage a copy-on-write
    /// mutation the way [`Self::staged_editor_sessions_mut`] does.
    fn editor_sessions_for_call_mut(
        &mut self,
    ) -> &mut crate::editor_session::EditorSessionRegistry {
        self.staged_editor_sessions
            .as_mut()
            .unwrap_or(&mut self.editor_sessions)
    }

    fn staged_editor_sessions_mut(&mut self) -> &mut crate::editor_session::EditorSessionRegistry {
        if self.staged_editor_sessions.is_none() {
            self.staged_editor_sessions = Some(self.editor_sessions.clone());
        }
        self.staged_editor_sessions
            .as_mut()
            .expect("staged Editor registry was initialized")
    }

    fn install_editor_sessions(
        &mut self,
        editor_sessions: crate::editor_session::EditorSessionRegistry,
    ) {
        self.editor_sessions = editor_sessions;
        self.staged_editor_sessions = None;
    }

    fn discard_staged_editor_sessions(&mut self) {
        self.staged_editor_sessions = None;
    }
}

pub(crate) fn dispatch_schedule_notifications(
    outputs: &[youth_state::SchedulerOutput],
    dispatcher: &dyn crate::NotificationDispatcher,
    state: &youth_state::StateStore,
) {
    for output in outputs {
        let youth_state::SchedulerOutput::QueueElapsedDelivery { token, .. } = output else {
            continue;
        };
        match state.schedule(token.schedule_id) {
            Ok(Some(record)) => {
                if let Some((title, body)) = record.notification {
                    dispatcher.dispatch(&title, &body);
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    schedule_id = token.schedule_id,
                    %error,
                    "schedule notification lookup failed"
                );
            }
        }
    }
}

impl crate::bindings::v002::youth::app::ui::Host for HostState {}

impl crate::bindings::v002::youth::state::store::Host for HostState {
    fn get(
        &mut self,
        key: String,
    ) -> Result<
        Option<crate::bindings::v002::youth::state::store::Value>,
        crate::bindings::v002::youth::state::store::StateError,
    > {
        self.state
            .get(&key)
            .map(|value| value.map(to_wire_state_value_v002))
            .map_err(to_wire_state_error_v002)
    }

    fn set(
        &mut self,
        key: String,
        value: crate::bindings::v002::youth::state::store::Value,
    ) -> Result<(), crate::bindings::v002::youth::state::store::StateError> {
        self.state
            .set(&key, from_wire_state_value_v002(value))
            .map_err(to_wire_state_error_v002)
    }

    fn delete(
        &mut self,
        key: String,
    ) -> Result<bool, crate::bindings::v002::youth::state::store::StateError> {
        self.state.delete(&key).map_err(to_wire_state_error_v002)
    }
}

fn to_wire_state_value_v002(
    value: youth_state::StateValue,
) -> crate::bindings::v002::youth::state::store::Value {
    use crate::bindings::v002::youth::state::store::Value;
    match value {
        youth_state::StateValue::Boolean(value) => Value::Boolean(value),
        youth_state::StateValue::Integer(value) => Value::Integer(value),
        youth_state::StateValue::Text(value) => Value::Text(value),
        youth_state::StateValue::Bytes(value) => Value::Bytes(value),
    }
}

fn from_wire_state_value_v002(
    value: crate::bindings::v002::youth::state::store::Value,
) -> youth_state::StateValue {
    use crate::bindings::v002::youth::state::store::Value;
    match value {
        Value::Boolean(value) => youth_state::StateValue::Boolean(value),
        Value::Integer(value) => youth_state::StateValue::Integer(value),
        Value::Text(value) => youth_state::StateValue::Text(value),
        Value::Bytes(value) => youth_state::StateValue::Bytes(value),
    }
}

fn to_wire_state_error_v002(
    error: youth_state::StateError,
) -> crate::bindings::v002::youth::state::store::StateError {
    use crate::bindings::v002::youth::state::store::ErrorCode;
    let code = match error {
        youth_state::StateError::InvalidKey => ErrorCode::InvalidKey,
        youth_state::StateError::InvalidValue => ErrorCode::InvalidValue,
        youth_state::StateError::ReadOnly => ErrorCode::ReadOnly,
        youth_state::StateError::QuotaExceeded => ErrorCode::QuotaExceeded,
        ref error if error.is_busy() => ErrorCode::Busy,
        youth_state::StateError::Idle
        | youth_state::StateError::NoTransaction
        | youth_state::StateError::TransactionActive => ErrorCode::Unavailable,
        youth_state::StateError::Database(_)
        | youth_state::StateError::Filesystem(_)
        | youth_state::StateError::Corrupt(_)
        | youth_state::StateError::UsageMismatch
        | youth_state::StateError::InvalidScheduleDuration
        | youth_state::StateError::InvalidScheduleNotification
        | youth_state::StateError::TooManySchedules
        | youth_state::StateError::UnknownSchedule
        | youth_state::StateError::StaleScheduleGeneration
        | youth_state::StateError::InvalidScheduleState
        | youth_state::StateError::BackupExists
        | youth_state::StateError::InjectedCommitFailure => ErrorCode::Internal,
    };
    crate::bindings::v002::youth::state::store::StateError {
        code,
        message: None,
    }
}

impl crate::bindings::v003::youth::app::ui::Host for HostState {}

impl crate::bindings::v003::youth::state::store::Host for HostState {
    fn get(
        &mut self,
        key: String,
    ) -> Result<
        Option<crate::bindings::v003::youth::state::store::Value>,
        crate::bindings::v003::youth::state::store::StateError,
    > {
        self.state
            .get(&key)
            .map(|value| value.map(to_wire_state_value_v003))
            .map_err(to_wire_state_error_v003)
    }

    fn set(
        &mut self,
        key: String,
        value: crate::bindings::v003::youth::state::store::Value,
    ) -> Result<(), crate::bindings::v003::youth::state::store::StateError> {
        self.state
            .set(&key, from_wire_state_value_v003(value))
            .map_err(to_wire_state_error_v003)
    }

    fn delete(
        &mut self,
        key: String,
    ) -> Result<bool, crate::bindings::v003::youth::state::store::StateError> {
        self.state.delete(&key).map_err(to_wire_state_error_v003)
    }
}

fn to_wire_state_value_v003(
    value: youth_state::StateValue,
) -> crate::bindings::v003::youth::state::store::Value {
    use crate::bindings::v003::youth::state::store::Value;
    match value {
        youth_state::StateValue::Boolean(value) => Value::Boolean(value),
        youth_state::StateValue::Integer(value) => Value::Integer(value),
        youth_state::StateValue::Text(value) => Value::Text(value),
        youth_state::StateValue::Bytes(value) => Value::Bytes(value),
    }
}

fn from_wire_state_value_v003(
    value: crate::bindings::v003::youth::state::store::Value,
) -> youth_state::StateValue {
    use crate::bindings::v003::youth::state::store::Value;
    match value {
        Value::Boolean(value) => youth_state::StateValue::Boolean(value),
        Value::Integer(value) => youth_state::StateValue::Integer(value),
        Value::Text(value) => youth_state::StateValue::Text(value),
        Value::Bytes(value) => youth_state::StateValue::Bytes(value),
    }
}

fn to_wire_state_error_v003(
    error: youth_state::StateError,
) -> crate::bindings::v003::youth::state::store::StateError {
    use crate::bindings::v003::youth::state::store::ErrorCode;
    let code = match error {
        youth_state::StateError::InvalidKey => ErrorCode::InvalidKey,
        youth_state::StateError::InvalidValue => ErrorCode::InvalidValue,
        youth_state::StateError::ReadOnly => ErrorCode::ReadOnly,
        youth_state::StateError::QuotaExceeded => ErrorCode::QuotaExceeded,
        ref error if error.is_busy() => ErrorCode::Busy,
        youth_state::StateError::Idle
        | youth_state::StateError::NoTransaction
        | youth_state::StateError::TransactionActive => ErrorCode::Unavailable,
        youth_state::StateError::Database(_)
        | youth_state::StateError::Filesystem(_)
        | youth_state::StateError::Corrupt(_)
        | youth_state::StateError::UsageMismatch
        | youth_state::StateError::InvalidScheduleDuration
        | youth_state::StateError::InvalidScheduleNotification
        | youth_state::StateError::TooManySchedules
        | youth_state::StateError::UnknownSchedule
        | youth_state::StateError::StaleScheduleGeneration
        | youth_state::StateError::InvalidScheduleState
        | youth_state::StateError::BackupExists
        | youth_state::StateError::InjectedCommitFailure => ErrorCode::Internal,
    };
    crate::bindings::v003::youth::state::store::StateError {
        code,
        message: None,
    }
}

impl crate::bindings::v004::youth::app::ui::Host for HostState {}

impl crate::bindings::v004::youth::state::store::Host for HostState {
    fn get(
        &mut self,
        key: String,
    ) -> Result<
        Option<crate::bindings::v004::youth::state::store::Value>,
        crate::bindings::v004::youth::state::store::StateError,
    > {
        self.state
            .get(&key)
            .map(|value| value.map(to_wire_state_value_v004))
            .map_err(to_wire_state_error_v004)
    }

    fn set(
        &mut self,
        key: String,
        value: crate::bindings::v004::youth::state::store::Value,
    ) -> Result<(), crate::bindings::v004::youth::state::store::StateError> {
        self.state
            .set(&key, from_wire_state_value_v004(value))
            .map_err(to_wire_state_error_v004)
    }

    fn delete(
        &mut self,
        key: String,
    ) -> Result<bool, crate::bindings::v004::youth::state::store::StateError> {
        self.state.delete(&key).map_err(to_wire_state_error_v004)
    }
}

fn to_wire_state_value_v004(
    value: youth_state::StateValue,
) -> crate::bindings::v004::youth::state::store::Value {
    use crate::bindings::v004::youth::state::store::Value;
    match value {
        youth_state::StateValue::Boolean(value) => Value::Boolean(value),
        youth_state::StateValue::Integer(value) => Value::Integer(value),
        youth_state::StateValue::Text(value) => Value::Text(value),
        youth_state::StateValue::Bytes(value) => Value::Bytes(value),
    }
}

fn from_wire_state_value_v004(
    value: crate::bindings::v004::youth::state::store::Value,
) -> youth_state::StateValue {
    use crate::bindings::v004::youth::state::store::Value;
    match value {
        Value::Boolean(value) => youth_state::StateValue::Boolean(value),
        Value::Integer(value) => youth_state::StateValue::Integer(value),
        Value::Text(value) => youth_state::StateValue::Text(value),
        Value::Bytes(value) => youth_state::StateValue::Bytes(value),
    }
}

fn to_wire_state_error_v004(
    error: youth_state::StateError,
) -> crate::bindings::v004::youth::state::store::StateError {
    use crate::bindings::v004::youth::state::store::ErrorCode;
    let code = match error {
        youth_state::StateError::InvalidKey => ErrorCode::InvalidKey,
        youth_state::StateError::InvalidValue => ErrorCode::InvalidValue,
        youth_state::StateError::ReadOnly => ErrorCode::ReadOnly,
        youth_state::StateError::QuotaExceeded => ErrorCode::QuotaExceeded,
        ref error if error.is_busy() => ErrorCode::Busy,
        youth_state::StateError::Idle
        | youth_state::StateError::NoTransaction
        | youth_state::StateError::TransactionActive => ErrorCode::Unavailable,
        youth_state::StateError::Database(_)
        | youth_state::StateError::Filesystem(_)
        | youth_state::StateError::Corrupt(_)
        | youth_state::StateError::UsageMismatch
        | youth_state::StateError::InvalidScheduleDuration
        | youth_state::StateError::InvalidScheduleNotification
        | youth_state::StateError::TooManySchedules
        | youth_state::StateError::UnknownSchedule
        | youth_state::StateError::StaleScheduleGeneration
        | youth_state::StateError::InvalidScheduleState
        | youth_state::StateError::BackupExists
        | youth_state::StateError::InjectedCommitFailure => ErrorCode::Internal,
    };
    crate::bindings::v004::youth::state::store::StateError {
        code,
        message: None,
    }
}

impl crate::bindings::v004::youth::time::scheduler::Host for HostState {
    fn schedule_after(
        &mut self,
        millis: u64,
        options: crate::bindings::v004::youth::time::scheduler::ScheduleOptions,
    ) -> Result<
        crate::bindings::v004::youth::time::scheduler::Schedule,
        crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let notification = options
            .notification
            .map(|notification| (notification.title, notification.body));
        let record = self
            .state
            .schedule_create(now_epoch_millis, millis, notification)
            .map_err(to_wire_schedule_error_v004)?;
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Create {
                app_id: self.state.app_id().clone(),
                record: record.clone(),
                now_epoch_millis,
            },
        ));
        Ok(to_wire_schedule_v004(&record))
    }

    fn pause(
        &mut self,
        value: crate::bindings::v004::youth::time::scheduler::Schedule,
    ) -> Result<
        crate::bindings::v004::youth::time::scheduler::Schedule,
        crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v004)?
            .ok_or(
                crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        let paused = self
            .state
            .schedule_pause(now_epoch_millis, value.id, value.generation)
            .map_err(to_wire_schedule_error_v004)?;
        let wire_paused = to_wire_schedule_v004(&paused);
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Pause {
                app_id: self.state.app_id().clone(),
                previous,
                paused,
            },
        ));
        Ok(wire_paused)
    }

    fn resume(
        &mut self,
        value: crate::bindings::v004::youth::time::scheduler::Schedule,
    ) -> Result<
        crate::bindings::v004::youth::time::scheduler::Schedule,
        crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v004)?
            .ok_or(
                crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        let resumed = self
            .state
            .schedule_resume(now_epoch_millis, value.id, value.generation)
            .map_err(to_wire_schedule_error_v004)?;
        let wire_resumed = to_wire_schedule_v004(&resumed);
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Resume {
                app_id: self.state.app_id().clone(),
                previous,
                resumed,
                now_epoch_millis,
            },
        ));
        Ok(wire_resumed)
    }

    fn cancel(
        &mut self,
        value: crate::bindings::v004::youth::time::scheduler::Schedule,
    ) -> Result<(), crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode> {
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v004)?
            .ok_or(
                crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        self.state
            .schedule_cancel(value.id, value.generation)
            .map_err(to_wire_schedule_error_v004)?;
        let cancelled = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v004)?
            .ok_or(crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode::Internal)?;
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Cancel {
                app_id: self.state.app_id().clone(),
                previous,
                cancelled,
            },
        ));
        Ok(())
    }
}

fn to_wire_schedule_v004(
    record: &youth_state::ScheduleRecord,
) -> crate::bindings::v004::youth::time::scheduler::Schedule {
    crate::bindings::v004::youth::time::scheduler::Schedule {
        id: record.id,
        generation: record.generation,
    }
}

fn to_wire_schedule_error_v004(
    error: youth_state::StateError,
) -> crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode {
    use crate::bindings::v004::youth::time::scheduler::ScheduleErrorCode;
    match error {
        youth_state::StateError::InvalidScheduleDuration => ScheduleErrorCode::InvalidDuration,
        youth_state::StateError::TooManySchedules => ScheduleErrorCode::TooManySchedules,
        youth_state::StateError::UnknownSchedule => ScheduleErrorCode::UnknownSchedule,
        youth_state::StateError::StaleScheduleGeneration => ScheduleErrorCode::StaleGeneration,
        youth_state::StateError::InvalidScheduleNotification
        | youth_state::StateError::InvalidScheduleState => ScheduleErrorCode::InvalidState,
        youth_state::StateError::ReadOnly
        | youth_state::StateError::Idle
        | youth_state::StateError::NoTransaction
        | youth_state::StateError::TransactionActive => ScheduleErrorCode::Unavailable,
        ref error if error.is_busy() => ScheduleErrorCode::Unavailable,
        youth_state::StateError::Database(_)
        | youth_state::StateError::Filesystem(_)
        | youth_state::StateError::Corrupt(_)
        | youth_state::StateError::UsageMismatch
        | youth_state::StateError::InvalidKey
        | youth_state::StateError::InvalidValue
        | youth_state::StateError::QuotaExceeded
        | youth_state::StateError::BackupExists
        | youth_state::StateError::InjectedCommitFailure => ScheduleErrorCode::Internal,
    }
}

impl crate::bindings::v005::youth::app::ui::Host for HostState {}

impl crate::bindings::v005::youth::state::store::Host for HostState {
    fn get(
        &mut self,
        key: String,
    ) -> Result<
        Option<crate::bindings::v005::youth::state::store::Value>,
        crate::bindings::v005::youth::state::store::StateError,
    > {
        self.state
            .get(&key)
            .map(|value| value.map(to_wire_state_value_v005))
            .map_err(to_wire_state_error_v005)
    }

    fn set(
        &mut self,
        key: String,
        value: crate::bindings::v005::youth::state::store::Value,
    ) -> Result<(), crate::bindings::v005::youth::state::store::StateError> {
        self.state
            .set(&key, from_wire_state_value_v005(value))
            .map_err(to_wire_state_error_v005)
    }

    fn delete(
        &mut self,
        key: String,
    ) -> Result<bool, crate::bindings::v005::youth::state::store::StateError> {
        self.state.delete(&key).map_err(to_wire_state_error_v005)
    }
}

fn to_wire_state_value_v005(
    value: youth_state::StateValue,
) -> crate::bindings::v005::youth::state::store::Value {
    use crate::bindings::v005::youth::state::store::Value;
    match value {
        youth_state::StateValue::Boolean(value) => Value::Boolean(value),
        youth_state::StateValue::Integer(value) => Value::Integer(value),
        youth_state::StateValue::Text(value) => Value::Text(value),
        youth_state::StateValue::Bytes(value) => Value::Bytes(value),
    }
}

fn from_wire_state_value_v005(
    value: crate::bindings::v005::youth::state::store::Value,
) -> youth_state::StateValue {
    use crate::bindings::v005::youth::state::store::Value;
    match value {
        Value::Boolean(value) => youth_state::StateValue::Boolean(value),
        Value::Integer(value) => youth_state::StateValue::Integer(value),
        Value::Text(value) => youth_state::StateValue::Text(value),
        Value::Bytes(value) => youth_state::StateValue::Bytes(value),
    }
}

fn to_wire_state_error_v005(
    error: youth_state::StateError,
) -> crate::bindings::v005::youth::state::store::StateError {
    use crate::bindings::v005::youth::state::store::ErrorCode;
    let code = match error {
        youth_state::StateError::InvalidKey => ErrorCode::InvalidKey,
        youth_state::StateError::InvalidValue => ErrorCode::InvalidValue,
        youth_state::StateError::ReadOnly => ErrorCode::ReadOnly,
        youth_state::StateError::QuotaExceeded => ErrorCode::QuotaExceeded,
        ref error if error.is_busy() => ErrorCode::Busy,
        youth_state::StateError::Idle
        | youth_state::StateError::NoTransaction
        | youth_state::StateError::TransactionActive => ErrorCode::Unavailable,
        youth_state::StateError::Database(_)
        | youth_state::StateError::Filesystem(_)
        | youth_state::StateError::Corrupt(_)
        | youth_state::StateError::UsageMismatch
        | youth_state::StateError::InvalidScheduleDuration
        | youth_state::StateError::InvalidScheduleNotification
        | youth_state::StateError::TooManySchedules
        | youth_state::StateError::UnknownSchedule
        | youth_state::StateError::StaleScheduleGeneration
        | youth_state::StateError::InvalidScheduleState
        | youth_state::StateError::BackupExists
        | youth_state::StateError::InjectedCommitFailure => ErrorCode::Internal,
    };
    crate::bindings::v005::youth::state::store::StateError {
        code,
        message: None,
    }
}

impl crate::bindings::v005::youth::time::scheduler::Host for HostState {
    fn schedule_after(
        &mut self,
        millis: u64,
        options: crate::bindings::v005::youth::time::scheduler::ScheduleOptions,
    ) -> Result<
        crate::bindings::v005::youth::time::scheduler::Schedule,
        crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let notification = options
            .notification
            .map(|notification| (notification.title, notification.body));
        let record = self
            .state
            .schedule_create(now_epoch_millis, millis, notification)
            .map_err(to_wire_schedule_error_v005)?;
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Create {
                app_id: self.state.app_id().clone(),
                record: record.clone(),
                now_epoch_millis,
            },
        ));
        Ok(to_wire_schedule_v005(&record))
    }

    fn pause(
        &mut self,
        value: crate::bindings::v005::youth::time::scheduler::Schedule,
    ) -> Result<
        crate::bindings::v005::youth::time::scheduler::Schedule,
        crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v005)?
            .ok_or(
                crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        let paused = self
            .state
            .schedule_pause(now_epoch_millis, value.id, value.generation)
            .map_err(to_wire_schedule_error_v005)?;
        let wire_paused = to_wire_schedule_v005(&paused);
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Pause {
                app_id: self.state.app_id().clone(),
                previous,
                paused,
            },
        ));
        Ok(wire_paused)
    }

    fn resume(
        &mut self,
        value: crate::bindings::v005::youth::time::scheduler::Schedule,
    ) -> Result<
        crate::bindings::v005::youth::time::scheduler::Schedule,
        crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v005)?
            .ok_or(
                crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        let resumed = self
            .state
            .schedule_resume(now_epoch_millis, value.id, value.generation)
            .map_err(to_wire_schedule_error_v005)?;
        let wire_resumed = to_wire_schedule_v005(&resumed);
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Resume {
                app_id: self.state.app_id().clone(),
                previous,
                resumed,
                now_epoch_millis,
            },
        ));
        Ok(wire_resumed)
    }

    fn cancel(
        &mut self,
        value: crate::bindings::v005::youth::time::scheduler::Schedule,
    ) -> Result<(), crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode> {
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v005)?
            .ok_or(
                crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        self.state
            .schedule_cancel(value.id, value.generation)
            .map_err(to_wire_schedule_error_v005)?;
        let cancelled = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v005)?
            .ok_or(crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode::Internal)?;
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Cancel {
                app_id: self.state.app_id().clone(),
                previous,
                cancelled,
            },
        ));
        Ok(())
    }
}

fn to_wire_schedule_v005(
    record: &youth_state::ScheduleRecord,
) -> crate::bindings::v005::youth::time::scheduler::Schedule {
    crate::bindings::v005::youth::time::scheduler::Schedule {
        id: record.id,
        generation: record.generation,
    }
}

fn to_wire_schedule_error_v005(
    error: youth_state::StateError,
) -> crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode {
    use crate::bindings::v005::youth::time::scheduler::ScheduleErrorCode;
    match error {
        youth_state::StateError::InvalidScheduleDuration => ScheduleErrorCode::InvalidDuration,
        youth_state::StateError::TooManySchedules => ScheduleErrorCode::TooManySchedules,
        youth_state::StateError::UnknownSchedule => ScheduleErrorCode::UnknownSchedule,
        youth_state::StateError::StaleScheduleGeneration => ScheduleErrorCode::StaleGeneration,
        youth_state::StateError::InvalidScheduleNotification
        | youth_state::StateError::InvalidScheduleState => ScheduleErrorCode::InvalidState,
        youth_state::StateError::ReadOnly
        | youth_state::StateError::Idle
        | youth_state::StateError::NoTransaction
        | youth_state::StateError::TransactionActive => ScheduleErrorCode::Unavailable,
        ref error if error.is_busy() => ScheduleErrorCode::Unavailable,
        youth_state::StateError::Database(_)
        | youth_state::StateError::Filesystem(_)
        | youth_state::StateError::Corrupt(_)
        | youth_state::StateError::UsageMismatch
        | youth_state::StateError::InvalidKey
        | youth_state::StateError::InvalidValue
        | youth_state::StateError::QuotaExceeded
        | youth_state::StateError::BackupExists
        | youth_state::StateError::InjectedCommitFailure => ScheduleErrorCode::Internal,
    }
}

impl crate::bindings::v006::youth::app::ui::Host for HostState {}

impl crate::bindings::v006::youth::state::store::Host for HostState {
    fn get(
        &mut self,
        key: String,
    ) -> Result<
        Option<crate::bindings::v006::youth::state::store::Value>,
        crate::bindings::v006::youth::state::store::StateError,
    > {
        self.state
            .get(&key)
            .map(|value| value.map(to_wire_state_value_v006))
            .map_err(to_wire_state_error_v006)
    }

    fn set(
        &mut self,
        key: String,
        value: crate::bindings::v006::youth::state::store::Value,
    ) -> Result<(), crate::bindings::v006::youth::state::store::StateError> {
        self.state
            .set(&key, from_wire_state_value_v006(value))
            .map_err(to_wire_state_error_v006)
    }

    fn delete(
        &mut self,
        key: String,
    ) -> Result<bool, crate::bindings::v006::youth::state::store::StateError> {
        self.state.delete(&key).map_err(to_wire_state_error_v006)
    }
}

fn to_wire_state_value_v006(
    value: youth_state::StateValue,
) -> crate::bindings::v006::youth::state::store::Value {
    use crate::bindings::v006::youth::state::store::Value;
    match value {
        youth_state::StateValue::Boolean(value) => Value::Boolean(value),
        youth_state::StateValue::Integer(value) => Value::Integer(value),
        youth_state::StateValue::Text(value) => Value::Text(value),
        youth_state::StateValue::Bytes(value) => Value::Bytes(value),
    }
}

fn from_wire_state_value_v006(
    value: crate::bindings::v006::youth::state::store::Value,
) -> youth_state::StateValue {
    use crate::bindings::v006::youth::state::store::Value;
    match value {
        Value::Boolean(value) => youth_state::StateValue::Boolean(value),
        Value::Integer(value) => youth_state::StateValue::Integer(value),
        Value::Text(value) => youth_state::StateValue::Text(value),
        Value::Bytes(value) => youth_state::StateValue::Bytes(value),
    }
}

fn to_wire_state_error_v006(
    error: youth_state::StateError,
) -> crate::bindings::v006::youth::state::store::StateError {
    use crate::bindings::v006::youth::state::store::ErrorCode;
    let code = match error {
        youth_state::StateError::InvalidKey => ErrorCode::InvalidKey,
        youth_state::StateError::InvalidValue => ErrorCode::InvalidValue,
        youth_state::StateError::ReadOnly => ErrorCode::ReadOnly,
        youth_state::StateError::QuotaExceeded => ErrorCode::QuotaExceeded,
        ref error if error.is_busy() => ErrorCode::Busy,
        youth_state::StateError::Idle
        | youth_state::StateError::NoTransaction
        | youth_state::StateError::TransactionActive => ErrorCode::Unavailable,
        youth_state::StateError::Database(_)
        | youth_state::StateError::Filesystem(_)
        | youth_state::StateError::Corrupt(_)
        | youth_state::StateError::UsageMismatch
        | youth_state::StateError::InvalidScheduleDuration
        | youth_state::StateError::InvalidScheduleNotification
        | youth_state::StateError::TooManySchedules
        | youth_state::StateError::UnknownSchedule
        | youth_state::StateError::StaleScheduleGeneration
        | youth_state::StateError::InvalidScheduleState
        | youth_state::StateError::BackupExists
        | youth_state::StateError::InjectedCommitFailure => ErrorCode::Internal,
    };
    crate::bindings::v006::youth::state::store::StateError {
        code,
        message: None,
    }
}

impl crate::bindings::v006::youth::editor::session::Host for HostState {
    fn snapshot(
        &mut self,
        editor: u64,
    ) -> Result<
        crate::bindings::v006::youth::editor::session::EditorSnapshot,
        crate::bindings::v006::youth::editor::session::EditorErrorCode,
    > {
        let editor = editor_node_id_v006(editor)?;
        crate::editor_session::snapshot_editor_session(self.editor_sessions_for_call_mut(), editor)
            .map(to_wire_editor_snapshot_v006)
            .map_err(to_wire_editor_error_v006)
    }

    fn accept(
        &mut self,
        editor: u64,
        expected_document_revision: u64,
        expected_edit_sequence: u64,
        new_document_revision: u64,
    ) -> Result<(), crate::bindings::v006::youth::editor::session::EditorErrorCode> {
        let editor = editor_node_id_v006(editor)?;
        crate::editor_session::accept_editor_session(
            self.staged_editor_sessions_mut(),
            editor,
            expected_document_revision,
            expected_edit_sequence,
            new_document_revision,
        )
        .map_err(to_wire_editor_error_v006)
    }

    fn replace(
        &mut self,
        editor: u64,
        expected_document_revision: u64,
        expected_edit_sequence: u64,
        new_document_revision: u64,
        authoritative_text: String,
    ) -> Result<(), crate::bindings::v006::youth::editor::session::EditorErrorCode> {
        let editor = editor_node_id_v006(editor)?;
        crate::editor_session::replace_editor_session(
            self.staged_editor_sessions_mut(),
            editor,
            expected_document_revision,
            expected_edit_sequence,
            new_document_revision,
            authoritative_text,
        )
        .map_err(to_wire_editor_error_v006)
    }
}

fn editor_node_id_v006(
    value: u64,
) -> Result<youth_tree::NodeId, crate::bindings::v006::youth::editor::session::EditorErrorCode> {
    youth_tree::NodeId::new(value)
        .ok_or(crate::bindings::v006::youth::editor::session::EditorErrorCode::UnknownEditor)
}

fn to_wire_editor_snapshot_v006(
    snapshot: crate::editor_session::EditorSessionSnapshot,
) -> crate::bindings::v006::youth::editor::session::EditorSnapshot {
    crate::bindings::v006::youth::editor::session::EditorSnapshot {
        document_revision: snapshot.document_revision,
        edit_sequence: snapshot.edit_sequence,
        text: snapshot.text,
    }
}

fn to_wire_editor_error_v006(
    error: crate::editor_session::EditorSessionError,
) -> crate::bindings::v006::youth::editor::session::EditorErrorCode {
    use crate::bindings::v006::youth::editor::session::EditorErrorCode;
    match error {
        crate::editor_session::EditorSessionError::UnknownEditor => EditorErrorCode::UnknownEditor,
        crate::editor_session::EditorSessionError::StaleDocumentRevision => {
            EditorErrorCode::StaleDocumentRevision
        }
        crate::editor_session::EditorSessionError::StaleEditSequence => {
            EditorErrorCode::StaleEditSequence
        }
    }
}

impl crate::bindings::v006::youth::time::scheduler::Host for HostState {
    fn schedule_after(
        &mut self,
        millis: u64,
        options: crate::bindings::v006::youth::time::scheduler::ScheduleOptions,
    ) -> Result<
        crate::bindings::v006::youth::time::scheduler::Schedule,
        crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let notification = options
            .notification
            .map(|notification| (notification.title, notification.body));
        let record = self
            .state
            .schedule_create(now_epoch_millis, millis, notification)
            .map_err(to_wire_schedule_error_v006)?;
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Create {
                app_id: self.state.app_id().clone(),
                record: record.clone(),
                now_epoch_millis,
            },
        ));
        Ok(to_wire_schedule_v006(&record))
    }

    fn pause(
        &mut self,
        value: crate::bindings::v006::youth::time::scheduler::Schedule,
    ) -> Result<
        crate::bindings::v006::youth::time::scheduler::Schedule,
        crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v006)?
            .ok_or(
                crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        let paused = self
            .state
            .schedule_pause(now_epoch_millis, value.id, value.generation)
            .map_err(to_wire_schedule_error_v006)?;
        let wire_paused = to_wire_schedule_v006(&paused);
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Pause {
                app_id: self.state.app_id().clone(),
                previous,
                paused,
            },
        ));
        Ok(wire_paused)
    }

    fn resume(
        &mut self,
        value: crate::bindings::v006::youth::time::scheduler::Schedule,
    ) -> Result<
        crate::bindings::v006::youth::time::scheduler::Schedule,
        crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode,
    > {
        let now_epoch_millis = self.deadline_clock.now_epoch_millis();
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v006)?
            .ok_or(
                crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        let resumed = self
            .state
            .schedule_resume(now_epoch_millis, value.id, value.generation)
            .map_err(to_wire_schedule_error_v006)?;
        let wire_resumed = to_wire_schedule_v006(&resumed);
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Resume {
                app_id: self.state.app_id().clone(),
                previous,
                resumed,
                now_epoch_millis,
            },
        ));
        Ok(wire_resumed)
    }

    fn cancel(
        &mut self,
        value: crate::bindings::v006::youth::time::scheduler::Schedule,
    ) -> Result<(), crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode> {
        let previous = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v006)?
            .ok_or(
                crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode::UnknownSchedule,
            )?;
        self.state
            .schedule_cancel(value.id, value.generation)
            .map_err(to_wire_schedule_error_v006)?;
        let cancelled = self
            .state
            .schedule(value.id)
            .map_err(to_wire_schedule_error_v006)?
            .ok_or(crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode::Internal)?;
        self.staged_schedule_outputs.extend(youth_state::transition(
            youth_state::SchedulerInput::Cancel {
                app_id: self.state.app_id().clone(),
                previous,
                cancelled,
            },
        ));
        Ok(())
    }
}

fn to_wire_schedule_v006(
    record: &youth_state::ScheduleRecord,
) -> crate::bindings::v006::youth::time::scheduler::Schedule {
    crate::bindings::v006::youth::time::scheduler::Schedule {
        id: record.id,
        generation: record.generation,
    }
}

fn to_wire_schedule_error_v006(
    error: youth_state::StateError,
) -> crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode {
    use crate::bindings::v006::youth::time::scheduler::ScheduleErrorCode;
    match error {
        youth_state::StateError::InvalidScheduleDuration => ScheduleErrorCode::InvalidDuration,
        youth_state::StateError::TooManySchedules => ScheduleErrorCode::TooManySchedules,
        youth_state::StateError::UnknownSchedule => ScheduleErrorCode::UnknownSchedule,
        youth_state::StateError::StaleScheduleGeneration => ScheduleErrorCode::StaleGeneration,
        youth_state::StateError::InvalidScheduleNotification
        | youth_state::StateError::InvalidScheduleState => ScheduleErrorCode::InvalidState,
        youth_state::StateError::ReadOnly
        | youth_state::StateError::Idle
        | youth_state::StateError::NoTransaction
        | youth_state::StateError::TransactionActive => ScheduleErrorCode::Unavailable,
        ref error if error.is_busy() => ScheduleErrorCode::Unavailable,
        youth_state::StateError::Database(_)
        | youth_state::StateError::Filesystem(_)
        | youth_state::StateError::Corrupt(_)
        | youth_state::StateError::UsageMismatch
        | youth_state::StateError::InvalidKey
        | youth_state::StateError::InvalidValue
        | youth_state::StateError::QuotaExceeded
        | youth_state::StateError::BackupExists
        | youth_state::StateError::InjectedCommitFailure => ScheduleErrorCode::Internal,
    }
}

/// One synchronous, single-owner Youth component instance.
pub struct YouthApp {
    component_id: String,
    app_id: youth_state::AppId,
    limits: RuntimeLimits,
    store: Store<HostState>,
    bindings: ApplicationBindings,
    tree: Option<youth_tree::Tree>,
    lifecycle: AppLifecycle,
    last_event_sequence: Option<u64>,
    last_turn: Option<TurnReceipt>,
    guest_call_count: u64,
    fault: Option<AppFault>,
}

impl fmt::Debug for YouthApp {
    /// Reports identity and protocol state only. Tree payloads are
    /// excluded so that debug output never leaks node contents
    /// (spec section 18).
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YouthApp")
            .field("component_id", &self.component_id)
            .field("lifecycle", &self.lifecycle)
            .field(
                "revision",
                &self.tree.as_ref().map(youth_tree::Tree::revision),
            )
            .finish_non_exhaustive()
    }
}

impl YouthApp {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        Self::load_config(YouthAppConfig::ephemeral(path))
    }

    pub fn load_with_limits(
        path: impl AsRef<Path>,
        limits: RuntimeLimits,
    ) -> Result<Self, RuntimeError> {
        let mut config = YouthAppConfig::ephemeral(path);
        config.limits = limits;
        Self::load_config(config)
    }

    pub fn load_config(config: YouthAppConfig) -> Result<Self, RuntimeError> {
        Self::load_with_engine(config, crate::shared_engine(), true)
    }

    pub(crate) fn load_config_deferred_reconcile(
        config: YouthAppConfig,
    ) -> Result<Self, RuntimeError> {
        Self::load_with_engine(config, crate::shared_engine(), false)
    }

    fn load_with_engine(
        config: YouthAppConfig,
        engine: &Engine,
        reconcile_on_open: bool,
    ) -> Result<Self, RuntimeError> {
        let path = config.component_path.as_path();
        let component_id = component_identity(path);
        let load_span = info_span!("component.load", component_id = %component_id);
        let bytes = load_span.in_scope(|| read_component(path, &component_id, &config.limits))?;

        let compile_span = info_span!("component.compile", component_id = %component_id);
        let component = compile_span.in_scope(|| {
            Component::new(engine, &bytes).map_err(|source| {
                RuntimeError::InvalidComponent(
                    ErrorContext::new(
                        "file is not a valid WebAssembly component",
                        &component_id,
                        AppLifecycle::Loaded,
                        None,
                    )
                    .with_source(source),
                )
            })
        })?;

        let instantiate_span = info_span!("component.instantiate", component_id = %component_id);
        instantiate_span.in_scope(|| {
            instantiate(
                engine,
                component,
                component_id,
                config.app_id,
                config.state,
                config.limits,
                reconcile_on_open,
            )
        })
    }

    #[must_use]
    pub const fn lifecycle(&self) -> AppLifecycle {
        self.lifecycle
    }

    #[must_use]
    pub fn tree(&self) -> Option<&youth_tree::Tree> {
        self.tree.as_ref()
    }

    /// Test-only introspection for live host Editor sessions.
    #[cfg(test)]
    pub(crate) fn editor_session_test_state(
        &mut self,
    ) -> Vec<(youth_tree::NodeId, u64, u64, u64, String)> {
        let mut sessions = self
            .store
            .data_mut()
            .editor_sessions
            .iter_mut()
            .filter_map(|(&node_id, slot)| {
                let generation = slot.generation();
                slot.session()
                    .map(|(document_revision, edit_sequence, text)| {
                        (node_id, generation, document_revision, edit_sequence, text)
                    })
            })
            .collect::<Vec<_>>();
        sessions.sort_unstable_by_key(|(node_id, ..)| node_id.get());
        sessions
    }

    /// Test-only introspection for retained per-node generation history.
    #[cfg(test)]
    pub(crate) fn editor_session_test_generation(
        &self,
        node_id: youth_tree::NodeId,
    ) -> Option<u64> {
        self.store
            .data()
            .editor_sessions
            .get(&node_id)
            .map(crate::editor_session::EditorSessionSlot::generation)
    }

    /// Test-only introspection for the latest declaration seen by a live slot.
    #[cfg(test)]
    pub(crate) fn editor_session_test_last_declared_revision(
        &self,
        node_id: youth_tree::NodeId,
    ) -> Option<u64> {
        self.store
            .data()
            .editor_sessions
            .get(&node_id)
            .and_then(crate::editor_session::EditorSessionSlot::last_declared_document_revision)
    }

    pub fn snapshot(&self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        self.tree
            .as_ref()
            .filter(|_| self.lifecycle == AppLifecycle::Mounted)
            .map(youth_tree::Tree::to_snapshot)
            .ok_or_else(|| {
                RuntimeError::InvalidLifecycle(self.context(
                    format!(
                        "snapshot is not available while the app is {}",
                        self.lifecycle
                    ),
                    None,
                ))
            })
    }

    /// Applies one cursor-free host-local Editor mutation without a guest turn,
    /// tree reconciliation, or durable-state transaction.
    pub fn edit_editor_locally(
        &mut self,
        editor: youth_tree::NodeId,
        edit: crate::EditorLocalEdit,
    ) -> Result<crate::EditorLocalEditResult, RuntimeError> {
        if self.lifecycle != AppLifecycle::Mounted {
            return Err(RuntimeError::InvalidLifecycle(self.context(
                format!(
                    "local Editor input is not allowed while the app is {}",
                    self.lifecycle
                ),
                None,
            )));
        }
        let result = match edit {
            crate::EditorLocalEdit::InsertText(text) => crate::editor_session::local_insert_text(
                &mut self.store.data_mut().editor_sessions,
                editor,
                &text,
            ),
            crate::EditorLocalEdit::Backspace => crate::editor_session::local_backspace(
                &mut self.store.data_mut().editor_sessions,
                editor,
            ),
            crate::EditorLocalEdit::Undo => crate::editor_session::local_undo(
                &mut self.store.data_mut().editor_sessions,
                editor,
            ),
            crate::EditorLocalEdit::Redo => crate::editor_session::local_redo(
                &mut self.store.data_mut().editor_sessions,
                editor,
            ),
            crate::EditorLocalEdit::Paste => {
                let clipboard_text =
                    self.store
                        .data()
                        .clipboard_service
                        .read_text()
                        .map_err(|source| {
                            RuntimeError::Internal(
                                self.context("host clipboard text could not be read", None)
                                    .with_source(source),
                            )
                        })?;
                crate::editor_session::local_paste_text(
                    &mut self.store.data_mut().editor_sessions,
                    editor,
                    clipboard_text.as_deref().unwrap_or_default(),
                )
            }
            crate::EditorLocalEdit::MoveCursor(movement) => {
                crate::editor_session::local_move_cursor(
                    &mut self.store.data_mut().editor_sessions,
                    editor,
                    movement,
                )
            }
            crate::EditorLocalEdit::ExtendSelection(movement) => {
                crate::editor_session::local_extend_selection(
                    &mut self.store.data_mut().editor_sessions,
                    editor,
                    movement,
                )
            }
            crate::EditorLocalEdit::ImeSetCompose { text, cursor } => {
                crate::editor_session::local_ime_set_compose(
                    &mut self.store.data_mut().editor_sessions,
                    editor,
                    &text,
                    cursor,
                )
            }
            crate::EditorLocalEdit::ImeClearCompose => {
                crate::editor_session::local_ime_clear_compose(
                    &mut self.store.data_mut().editor_sessions,
                    editor,
                )
            }
            crate::EditorLocalEdit::ImeFinishCompose => {
                crate::editor_session::local_ime_finish_compose(
                    &mut self.store.data_mut().editor_sessions,
                    editor,
                )
            }
            crate::EditorLocalEdit::MoveToPoint { x, y } => {
                crate::editor_session::local_move_to_point(
                    &mut self.store.data_mut().editor_sessions,
                    editor,
                    x,
                    y,
                )
            }
            crate::EditorLocalEdit::ExtendSelectionToPoint {
                anchor_x,
                anchor_y,
                x,
                y,
            } => crate::editor_session::local_extend_selection_to_point(
                &mut self.store.data_mut().editor_sessions,
                editor,
                anchor_x,
                anchor_y,
                x,
                y,
            ),
        };
        result.map_err(|_| {
            RuntimeError::InvalidLifecycle(self.context(
                format!("node {} does not have a live Editor session", editor.get()),
                None,
            ))
        })
    }

    /// Returns the host-owned dirty fact for a live Editor session.
    ///
    /// Dirty means the live edit sequence differs from the sequence most
    /// recently acknowledged by a successful guest `accept`.
    #[must_use]
    pub fn editor_locally_dirty(&self, editor: youth_tree::NodeId) -> Option<bool> {
        crate::editor_session::editor_locally_dirty(&self.store.data().editor_sessions, editor)
    }

    /// Revalidates a process-local wake against durable state without
    /// constructing or invoking a guest turn.
    pub fn receive_schedule_wake(
        &mut self,
        wake: &ScheduleWake,
    ) -> Result<WakeDisposition, RuntimeError> {
        if wake.application_id != self.app_id {
            return Ok(WakeDisposition::Discarded);
        }
        let now_epoch_millis = self.store.data().deadline_clock.now_epoch_millis();
        let outputs = self
            .store
            .data_mut()
            .state
            .receive_wake(wake.token.clone(), now_epoch_millis)
            .map_err(|source| {
                RuntimeError::StateUnavailable(
                    self.context(
                        "schedule wake could not be checked against durable state",
                        None,
                    )
                    .with_source(source),
                )
            })?;
        youth_state::execute_wake_outputs(self.store.data().wake_driver.as_ref(), &outputs);
        dispatch_schedule_notifications(
            &outputs,
            self.store.data().notification_dispatcher.as_ref(),
            &self.store.data().state,
        );
        Ok(
            if outputs.iter().any(|output| {
                matches!(
                    output,
                    youth_state::SchedulerOutput::QueueElapsedDelivery { .. }
                )
            }) {
                WakeDisposition::DeliveryQueued
            } else {
                WakeDisposition::Discarded
            },
        )
    }

    pub fn pending_deliveries(&self) -> Result<Vec<youth_state::PendingDelivery>, RuntimeError> {
        self.store
            .data()
            .state
            .pending_deliveries()
            .map_err(|source| {
                RuntimeError::StateUnavailable(
                    self.context("pending schedule deliveries could not be read", None)
                        .with_source(source),
                )
            })
    }

    pub fn schedule(&self, id: u64) -> Result<Option<youth_state::ScheduleRecord>, RuntimeError> {
        self.store.data().state.schedule(id).map_err(|source| {
            RuntimeError::StateUnavailable(
                self.context("schedule could not be read", None)
                    .with_source(source),
            )
        })
    }

    /// Extracts a paintable presentation for every live Editor session, for
    /// synchronous consumption by a desktop renderer. See
    /// [`crate::editor_session::editor_presentations`].
    pub(crate) fn editor_presentations(
        &mut self,
    ) -> std::collections::HashMap<youth_tree::NodeId, youth_editor_engine::TextPresentation> {
        crate::editor_session::editor_presentations(&mut self.store.data_mut().editor_sessions)
    }

    #[must_use]
    pub fn now_epoch_millis(&self) -> u64 {
        self.store.data().deadline_clock.now_epoch_millis()
    }

    pub(crate) fn reconcile_schedules(&mut self) -> Result<(), RuntimeError> {
        let now_epoch_millis = self.store.data().deadline_clock.now_epoch_millis();
        let outputs = self
            .store
            .data_mut()
            .state
            .reconcile_overdue(now_epoch_millis)
            .map_err(|source| {
                RuntimeError::StateUnavailable(
                    self.context("durable schedules could not be reconciled", None)
                        .with_source(source),
                )
            })?;
        youth_state::execute_wake_outputs(self.store.data().wake_driver.as_ref(), &outputs);
        dispatch_schedule_notifications(
            &outputs,
            self.store.data().notification_dispatcher.as_ref(),
            &self.store.data().state,
        );
        Ok(())
    }

    #[cfg(feature = "test-support")]
    pub fn fail_next_state_commit(&mut self) {
        self.store.data_mut().state.fail_next_commit();
    }

    pub fn mount(&mut self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let turn_id = Some(0);
        let span = info_span!(
            "app.mount",
            app_id = %self.app_id,
            component_id = %self.component_id,
            turn_id = 0_u64,
            call_phase = "mount",
            event_count = 0_u64,
            base_revision = 0_u64,
            next_revision = 0_u64,
            patch_count = 0_u64,
            fuel_before = self.limits.mount.fuel,
            fuel_after = tracing::field::Empty,
            elapsed_microseconds = tracing::field::Empty,
            state_call_count = tracing::field::Empty,
            state_write_count = tracing::field::Empty,
            state_bytes_before = tracing::field::Empty,
            state_bytes_after = tracing::field::Empty,
            transaction_result = tracing::field::Empty,
            result = tracing::field::Empty,
        );
        let result = span.in_scope(|| self.mount_inner(turn_id));
        span.record("result", if result.is_ok() { "ok" } else { "error" });
        result
    }

    /// Delivers one host-sequenced activation event and atomically commits its patches.
    pub fn activate(&mut self, node: youth_tree::NodeId) -> Result<TurnReceipt, RuntimeError> {
        if self.lifecycle != AppLifecycle::Mounted {
            return Err(RuntimeError::InvalidLifecycle(self.context(
                format!(
                    "activate is not allowed while the app is {}",
                    self.lifecycle
                ),
                None,
            )));
        }
        let sequence = self
            .last_event_sequence
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                RuntimeError::Internal(self.context("event sequence space is exhausted", None))
            })?;
        self.last_event_sequence = Some(sequence);
        let base_revision = self
            .tree
            .as_ref()
            .expect("mounted applications retain a tree")
            .revision();
        self.last_turn = Some(TurnReceipt {
            turn_id: sequence,
            first_event_sequence: sequence,
            processed_through: sequence,
            event_sequence: sequence,
            base_revision,
            next_revision: base_revision,
            patch_batch: youth_tree::PatchBatch {
                base_revision,
                next_revision: base_revision,
                patches: Vec::new(),
            },
            patch_count: 0,
            state_writes: 0,
            elapsed: std::time::Duration::ZERO,
            committed: false,
        });
        let span = info_span!(
            "app.turn",
            app_id = %self.app_id,
            component_id = %self.component_id,
            turn_id = sequence,
            call_phase = "handle",
            event_count = 1_u64,
            first_event_sequence = sequence,
            last_event_sequence = sequence,
            base_revision,
            next_revision = tracing::field::Empty,
            patch_count = tracing::field::Empty,
            fuel_before = self.limits.handle.fuel,
            fuel_after = tracing::field::Empty,
            elapsed_microseconds = tracing::field::Empty,
            state_call_count = tracing::field::Empty,
            state_write_count = tracing::field::Empty,
            state_bytes_before = tracing::field::Empty,
            state_bytes_after = tracing::field::Empty,
            transaction_result = tracing::field::Empty,
            result = tracing::field::Empty,
        );
        let event = HostEvent::Activate {
            sequence,
            node: node.get(),
        };
        let result = span.in_scope(|| self.handle_inner(event, None, base_revision));
        span.record("result", if result.is_ok() { "ok" } else { "error" });
        result
    }

    /// Delivers the oldest durable elapsed event, if one exists.
    ///
    /// This method is intentionally synchronous and explicit. It does not
    /// arrange a wake or retry itself.
    pub fn deliver_next_pending(&mut self) -> Result<Option<TurnReceipt>, RuntimeError> {
        if self.lifecycle != AppLifecycle::Mounted {
            return Err(RuntimeError::InvalidLifecycle(self.context(
                format!(
                    "pending delivery is not allowed while the app is {}",
                    self.lifecycle
                ),
                None,
            )));
        }
        let Some(delivery) = self.pending_deliveries()?.into_iter().next() else {
            return Ok(None);
        };
        if delivery.required_protocol == youth_state::DeliveryProtocol::V004
            && !matches!(
                self.bindings.version(),
                ProtocolVersion::V004 | ProtocolVersion::V005 | ProtocolVersion::V006
            )
        {
            let error = RuntimeError::UnsupportedWorld(self.context(
                format!(
                    "pending schedule delivery requires protocol 0.0.4, but the loaded component exports {}",
                    self.bindings.version().world()
                ),
                None,
            ));
            return Err(self.enter_fault(error));
        }
        let sequence = self
            .last_event_sequence
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                RuntimeError::Internal(self.context("event sequence space is exhausted", None))
            })?;
        self.last_event_sequence = Some(sequence);
        let base_revision = self
            .tree
            .as_ref()
            .expect("mounted applications retain a tree")
            .revision();
        let event = HostEvent::ScheduleElapsed {
            sequence,
            schedule: delivery.schedule_id,
            generation: delivery.generation,
            reason: delivery.reason,
        };
        let result = self.handle_inner(event, Some(delivery), base_revision);
        result.map(Some)
    }

    fn handle_inner(
        &mut self,
        event: HostEvent,
        delivery: Option<youth_state::PendingDelivery>,
        base_revision: u64,
    ) -> Result<TurnReceipt, RuntimeError> {
        let sequence = event.sequence();
        let turn_id = Some(sequence);
        self.prepare_call(self.limits.handle, "handle", turn_id)?;
        let mut staged_tree = self
            .tree
            .as_ref()
            .expect("mounted applications retain a tree")
            .clone();
        if self.limits.max_event_batch == 0 {
            return Err(RuntimeError::Internal(
                self.context("event batch exceeds the configured limit", turn_id),
            ));
        }
        let events = [event];
        self.begin_state(youth_state::GuestCallPhase::Handle, turn_id)?;

        let started = std::time::Instant::now();
        let guest_result = self
            .bindings
            .call_handle(&mut self.store, base_revision, &events);
        let elapsed = started.elapsed();
        self.record_call_metrics(elapsed);
        let guest_result = match guest_result {
            Ok(result) => result,
            Err(source) => {
                let error = self.classify_guest_failure(source, turn_id, "handling events");
                return Err(self.rollback_and_fault(error));
            }
        };
        let wire_batch = match guest_result {
            Ok(batch) => batch,
            Err(error) => {
                let writes = self.store.data().state.metrics().writes;
                let recoverable = delivery.is_none()
                    && error.code == GuestErrorCode::RejectedEvent
                    && writes == 0;
                let error =
                    RuntimeError::GuestRejected(self.guest_error_context(error, turn_id, "handle"));
                let _ = self.store.data_mut().state.rollback();
                self.store.data_mut().discard_staged_schedule_outputs();
                self.store.data_mut().discard_staged_editor_sessions();
                self.record_state_metrics("rolled_back");
                return if recoverable {
                    Err(error)
                } else {
                    Err(self.enter_fault(error))
                };
            }
        };
        if wire_batch.processed_through() != sequence {
            let error = RuntimeError::EventSequenceViolation(self.context(
                format!(
                    "guest processed through event {}, but the last event sent was {sequence}",
                    wire_batch.processed_through()
                ),
                turn_id,
            ));
            return Err(self.rollback_and_fault(error));
        }
        let batch = match from_guest::patch_batch(wire_batch, &self.limits) {
            Ok(batch) => batch,
            Err(source) => {
                let category = source.kind;
                let context = self
                    .context("guest returned a malformed patch batch", turn_id)
                    .with_source(source);
                let error = if category == WireErrorKind::TransferLimit {
                    RuntimeError::TransferLimitExceeded(context)
                } else {
                    RuntimeError::InvalidPatchBatch(context)
                };
                return Err(self.rollback_and_fault(error));
            }
        };
        if batch.base_revision != base_revision {
            let error = RuntimeError::RevisionMismatch(self.context(
                format!(
                    "patch base revision {} does not match live revision {base_revision}",
                    batch.base_revision
                ),
                turn_id,
            ));
            return Err(self.rollback_and_fault(error));
        }

        let next_revision = batch.next_revision;
        let patch_count = batch.patches.len();
        if let Some(receipt) = &mut self.last_turn {
            receipt.next_revision = next_revision;
            receipt.patch_count = patch_count;
            receipt.patch_batch = batch.clone();
        }
        tracing::Span::current().record("next_revision", next_revision);
        tracing::Span::current().record("patch_count", patch_count as u64);
        let apply_span = info_span!(
            "tree.apply",
            component_id = %self.component_id,
            turn_id = sequence,
            base_revision,
            next_revision,
            patch_count = patch_count as u64,
        );
        let apply_result =
            apply_span.in_scope(|| staged_tree.apply(batch.clone(), &self.limits.tree));
        if let Err(source) = apply_result {
            let revision_error = matches!(
                source,
                youth_tree::PatchError::RevisionMismatch { .. }
                    | youth_tree::PatchError::InvalidRevisionTransition { .. }
            );
            let context = self
                .context("guest returned an invalid patch batch", turn_id)
                .with_source(source);
            let error = if revision_error {
                RuntimeError::RevisionMismatch(context)
            } else {
                RuntimeError::InvalidPatchBatch(context)
            };
            return Err(self.rollback_and_fault(error));
        }
        if let Err(error) = self.validate_countdown_references(&staged_tree, turn_id) {
            return Err(self.rollback_and_fault(error));
        }
        let editor_sessions = crate::editor_session::reconcile_editor_sessions(
            self.store.data().editor_sessions_for_call(),
            &staged_tree,
        );

        let state_writes = self.store.data().state.metrics().writes;
        if let Some(delivery) = delivery
            && let Err(source) = self
                .store
                .data_mut()
                .state
                .acknowledge_pending_delivery(delivery)
        {
            let error = RuntimeError::StateUnavailable(
                self.context(
                    "pending schedule delivery could not be acknowledged in its turn",
                    turn_id,
                )
                .with_source(source),
            );
            return Err(self.rollback_and_fault(error));
        }
        if let Err(source) = self.store.data_mut().state.commit() {
            self.store.data_mut().discard_staged_schedule_outputs();
            self.store.data_mut().discard_staged_editor_sessions();
            self.record_state_metrics("commit_failed");
            let error = RuntimeError::StateCommitFailed(
                self.context("state commit failed after patch validation", turn_id)
                    .with_source(source),
            );
            return Err(self.enter_fault(error));
        }
        self.store.data_mut().apply_committed_schedule_outputs();
        self.store
            .data_mut()
            .install_editor_sessions(editor_sessions);
        self.record_state_metrics("committed");
        self.tree = Some(staged_tree);
        let elapsed = started.elapsed();

        let receipt = TurnReceipt {
            turn_id: sequence,
            first_event_sequence: sequence,
            processed_through: sequence,
            event_sequence: sequence,
            base_revision,
            next_revision,
            patch_batch: batch,
            patch_count,
            state_writes,
            elapsed,
            committed: true,
        };
        self.last_turn = Some(receipt.clone());
        Ok(receipt)
    }

    /// Replaces the retained tree with a validated snapshot at the live revision.
    pub fn resync(&mut self) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        let base_revision = match self.tree.as_ref() {
            Some(tree) if self.lifecycle == AppLifecycle::Mounted => tree.revision(),
            _ => {
                return Err(RuntimeError::InvalidLifecycle(self.context(
                    format!("resync is not allowed while the app is {}", self.lifecycle),
                    None,
                )));
            }
        };
        let turn_id = self.last_event_sequence;
        let span = info_span!(
            "app.resync",
            app_id = %self.app_id,
            component_id = %self.component_id,
            turn_id = turn_id.unwrap_or(0),
            call_phase = "resync",
            event_count = 0_u64,
            first_event_sequence = self.last_event_sequence.unwrap_or(0),
            last_event_sequence = self.last_event_sequence.unwrap_or(0),
            base_revision,
            next_revision = base_revision,
            patch_count = 0_u64,
            fuel_before = self.limits.resync.fuel,
            fuel_after = tracing::field::Empty,
            elapsed_microseconds = tracing::field::Empty,
            state_call_count = tracing::field::Empty,
            state_write_count = tracing::field::Empty,
            state_bytes_before = tracing::field::Empty,
            state_bytes_after = tracing::field::Empty,
            transaction_result = tracing::field::Empty,
            result = tracing::field::Empty,
        );
        let result = span.in_scope(|| self.resync_inner(base_revision, turn_id, true));
        span.record("result", if result.is_ok() { "ok" } else { "error" });
        result
    }

    fn resync_inner(
        &mut self,
        live_revision: u64,
        turn_id: Option<u64>,
        install: bool,
    ) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        self.prepare_call(self.limits.resync, "resync", turn_id)?;
        self.begin_state(youth_state::GuestCallPhase::Resync, turn_id)?;
        let started = std::time::Instant::now();
        let guest_result = self.bindings.call_resync(&mut self.store);
        let elapsed = started.elapsed();
        self.record_call_metrics(elapsed);
        let guest_result = match guest_result {
            Ok(result) => result,
            Err(source) => {
                let error = self.classify_guest_failure(source, turn_id, "resynchronizing");
                return Err(self.rollback_and_fault(error));
            }
        };
        let wire_snapshot = match guest_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let error =
                    RuntimeError::GuestRejected(self.guest_error_context(error, turn_id, "resync"));
                let _ = self.store.data_mut().state.rollback();
                self.store.data_mut().discard_staged_schedule_outputs();
                self.store.data_mut().discard_staged_editor_sessions();
                self.record_state_metrics("rolled_back");
                return Err(error);
            }
        };
        let snapshot = match from_guest::tree_snapshot(wire_snapshot, &self.limits) {
            Ok(snapshot) => snapshot,
            Err(source) => {
                let category = source.kind;
                let context = self
                    .context("guest returned a malformed resync snapshot", turn_id)
                    .with_source(source);
                let error = if category == WireErrorKind::TransferLimit {
                    RuntimeError::TransferLimitExceeded(context)
                } else {
                    RuntimeError::InvalidSnapshot(context)
                };
                return Err(self.rollback_and_fault(error));
            }
        };
        if snapshot.revision != live_revision {
            let error = RuntimeError::RevisionMismatch(self.context(
                format!(
                    "resync revision {} does not match live revision {live_revision}",
                    snapshot.revision
                ),
                turn_id,
            ));
            return Err(self.rollback_and_fault(error));
        }
        let validation_span = info_span!(
            "tree.validate",
            component_id = %self.component_id,
            turn_id = turn_id.unwrap_or(0),
        );
        let tree = match validation_span
            .in_scope(|| youth_tree::Tree::from_snapshot(snapshot, &self.limits.tree))
        {
            Ok(tree) => tree,
            Err(source) => {
                let error = RuntimeError::InvalidSnapshot(
                    self.context("guest returned an invalid resync tree", turn_id)
                        .with_source(source),
                );
                return Err(self.rollback_and_fault(error));
            }
        };
        let snapshot = tree.to_snapshot();
        if let Err(error) = self.validate_countdown_references(&tree, turn_id) {
            return Err(self.rollback_and_fault(error));
        }
        let editor_sessions = install.then(|| {
            crate::editor_session::reconcile_editor_sessions(
                self.store.data().editor_sessions_for_call(),
                &tree,
            )
        });
        if let Err(source) = self.store.data_mut().state.rollback() {
            self.store.data_mut().discard_staged_schedule_outputs();
            self.store.data_mut().discard_staged_editor_sessions();
            let error = RuntimeError::StateUnavailable(
                self.context("read-only resync transaction could not close", turn_id)
                    .with_source(source),
            );
            return Err(self.enter_fault(error));
        }
        self.store.data_mut().discard_staged_schedule_outputs();
        self.record_state_metrics("read_only");
        if install {
            self.tree = Some(tree);
            self.store.data_mut().install_editor_sessions(
                editor_sessions.expect("installing resync has staged sessions"),
            );
        } else {
            self.store.data_mut().discard_staged_editor_sessions();
        }
        Ok(snapshot)
    }

    /// Reconstructs the guest view without replacing host authority or
    /// changing externally visible turn accounting.
    #[doc(hidden)]
    pub fn verify_view(&mut self) -> Result<ViewVerification, RuntimeError> {
        let retained = self.snapshot()?;
        let metrics = self.store.data().state.metrics();
        let result = self
            .resync_inner(retained.revision, self.last_event_sequence, false)
            .map(|reconstructed| ViewVerification {
                retained,
                reconstructed,
            });
        self.store
            .data_mut()
            .state
            .restore_verification_metrics(metrics);
        result
    }

    /// Stops a mounted application and releases its retained tree.
    pub fn stop(&mut self) -> Result<(), RuntimeError> {
        if self.lifecycle != AppLifecycle::Mounted {
            return Err(RuntimeError::InvalidLifecycle(self.context(
                format!("stop is not allowed while the app is {}", self.lifecycle),
                None,
            )));
        }
        self.tree = None;
        self.lifecycle = AppLifecycle::Stopped;
        Ok(())
    }

    /// Returns host-owned state without entering the guest.
    #[must_use]
    pub fn inspect(&self) -> AppInspection {
        let tree = self.tree.as_ref();
        AppInspection {
            app_id: self.app_id.to_string(),
            state_location: match self.store.data().state.location() {
                StateLocation::Memory => "memory",
                StateLocation::File(_) => "file",
            },
            state_summary: self.store.data().state.summary().ok(),
            state_transaction_active: self.store.data().state.transaction_active(),
            state_phase: self.store.data().state.phase(),
            state_metrics: self.store.data().state.metrics(),
            lifecycle: self.lifecycle,
            world: self.bindings.version().world().to_owned(),
            current_revision: tree.map(youth_tree::Tree::revision),
            next_event_sequence: self.last_event_sequence.unwrap_or(0).checked_add(1),
            last_event_sequence: self.last_event_sequence,
            node_count: tree.map_or(0, youth_tree::Tree::node_count),
            depth: tree.map_or(0, youth_tree::Tree::depth),
            last_turn: self.last_turn.clone(),
            guest_call_count: self.guest_call_count,
            fault: self.fault.clone(),
            canonical_tree: tree.map_or_else(String::new, youth_tree::Tree::canonical),
        }
    }

    fn mount_inner(
        &mut self,
        turn_id: Option<u64>,
    ) -> Result<youth_tree::TreeSnapshot, RuntimeError> {
        if self.lifecycle != AppLifecycle::Loaded {
            return Err(RuntimeError::InvalidLifecycle(self.context(
                format!("mount is not allowed while the app is {}", self.lifecycle),
                turn_id,
            )));
        }
        self.prepare_call(self.limits.mount, "mount", turn_id)?;
        self.begin_state(youth_state::GuestCallPhase::Mount, turn_id)?;

        let started = std::time::Instant::now();
        let guest_result = self.bindings.call_mount(&mut self.store);
        let elapsed = started.elapsed();
        self.record_call_metrics(elapsed);
        let guest_result = match guest_result {
            Ok(result) => result,
            Err(source) => {
                let error = self.classify_guest_failure(source, turn_id, "mounting");
                return Err(self.rollback_and_fault(error));
            }
        };
        let wire_snapshot = match guest_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let error =
                    RuntimeError::GuestRejected(self.guest_error_context(error, turn_id, "mount"));
                return Err(self.rollback_and_fault(error));
            }
        };
        let snapshot = match from_guest::tree_snapshot(wire_snapshot, &self.limits) {
            Ok(snapshot) => snapshot,
            Err(source) => {
                let category = source.kind;
                let context = self
                    .context("guest returned a malformed initial snapshot", turn_id)
                    .with_source(source);
                let error = if category == WireErrorKind::TransferLimit {
                    RuntimeError::TransferLimitExceeded(context)
                } else {
                    RuntimeError::InvalidSnapshot(context)
                };
                return Err(self.rollback_and_fault(error));
            }
        };
        if snapshot.revision != 0 {
            let error = RuntimeError::RevisionMismatch(self.context(
                format!(
                    "initial snapshot revision must be 0, got {}",
                    snapshot.revision
                ),
                turn_id,
            ));
            return Err(self.rollback_and_fault(error));
        }

        let validation_span =
            info_span!("tree.validate", component_id = %self.component_id, turn_id = 0_u64);
        let tree = validation_span
            .in_scope(|| youth_tree::Tree::from_snapshot(snapshot, &self.limits.tree))
            .map_err(|source| {
                RuntimeError::InvalidSnapshot(
                    self.context("guest returned an invalid initial tree", turn_id)
                        .with_source(source),
                )
            });
        let tree = match tree {
            Ok(tree) => tree,
            Err(error) => return Err(self.rollback_and_fault(error)),
        };
        let snapshot = tree.to_snapshot();
        if let Err(error) = self.validate_countdown_references(&tree, turn_id) {
            return Err(self.rollback_and_fault(error));
        }
        let editor_sessions = crate::editor_session::reconcile_editor_sessions(
            self.store.data().editor_sessions_for_call(),
            &tree,
        );
        if let Err(source) = self.store.data_mut().state.commit() {
            self.store.data_mut().discard_staged_schedule_outputs();
            self.store.data_mut().discard_staged_editor_sessions();
            self.record_state_metrics("commit_failed");
            let error = RuntimeError::StateCommitFailed(
                self.context("state commit failed after mount validation", turn_id)
                    .with_source(source),
            );
            return Err(self.enter_fault(error));
        }
        self.store.data_mut().apply_committed_schedule_outputs();
        self.store
            .data_mut()
            .install_editor_sessions(editor_sessions);
        self.record_state_metrics("committed");
        self.tree = Some(tree);
        self.lifecycle = AppLifecycle::Mounted;
        Ok(snapshot)
    }

    fn validate_countdown_references(
        &self,
        tree: &youth_tree::Tree,
        turn_id: Option<u64>,
    ) -> Result<(), RuntimeError> {
        for node in tree.to_snapshot().nodes {
            let Some((schedule, _, _)) = node.data.countdown_ref() else {
                continue;
            };
            let record = self
                .store
                .data()
                .state
                .schedule(schedule.id)
                .map_err(|source| {
                    RuntimeError::StateUnavailable(
                        self.context(
                            format!(
                                "countdown schedule {} could not be validated before tree install",
                                schedule.id
                            ),
                            turn_id,
                        )
                        .with_source(source),
                    )
                })?;
            match record {
                Some(record) if record.generation == schedule.generation => {}
                Some(record) => {
                    return Err(RuntimeError::GuestRejected(self.context(
                        format!(
                            "countdown references stale schedule {} generation {}; live generation is {}",
                            schedule.id, schedule.generation, record.generation
                        ),
                        turn_id,
                    )));
                }
                None => {
                    return Err(RuntimeError::GuestRejected(self.context(
                        format!(
                            "countdown references unknown schedule {} generation {}",
                            schedule.id, schedule.generation
                        ),
                        turn_id,
                    )));
                }
            }
        }
        Ok(())
    }

    fn guest_error_context(
        &self,
        error: GuestError,
        turn_id: Option<u64>,
        operation: &str,
    ) -> ErrorContext {
        let code = format!("{:?}", error.code);
        let message = error.message.map_or_else(
            || format!("guest rejected {operation} with {code}"),
            |message| {
                if message.len() > self.limits.max_guest_error_message {
                    format!("guest rejected {operation} with {code}; error message exceeded the configured limit")
                } else {
                    format!("guest rejected {operation} with {code}: {message}")
                }
            },
        );
        self.context(message, turn_id)
    }

    fn prepare_call(
        &mut self,
        budget: CallBudget,
        operation: &str,
        turn_id: Option<u64>,
    ) -> Result<(), RuntimeError> {
        self.store.set_fuel(budget.fuel).map_err(|source| {
            RuntimeError::Internal(
                self.context(format!("failed to set {operation} fuel"), turn_id)
                    .with_source(source),
            )
        })?;
        self.store
            .set_hostcall_fuel(self.limits.max_guest_to_host_transfer);
        self.store
            .set_epoch_deadline(deadline_ticks(budget.deadline));
        self.store.epoch_deadline_trap();
        self.store.data_mut().limiter.limit_hit = false;
        Ok(())
    }

    fn begin_state(
        &mut self,
        phase: youth_state::GuestCallPhase,
        turn_id: Option<u64>,
    ) -> Result<(), RuntimeError> {
        self.store.data_mut().state.begin(phase).map_err(|source| {
            let error = RuntimeError::StateUnavailable(
                self.context("state transaction could not begin", turn_id)
                    .with_source(source),
            );
            self.enter_fault(error)
        })
    }

    fn rollback_and_fault(&mut self, error: RuntimeError) -> RuntimeError {
        let _ = self.store.data_mut().state.rollback();
        self.store.data_mut().discard_staged_schedule_outputs();
        self.store.data_mut().discard_staged_editor_sessions();
        self.record_state_metrics("rolled_back");
        self.enter_fault(error)
    }

    fn record_state_metrics(&self, result: &'static str) {
        let metrics = self.store.data().state.metrics();
        tracing::Span::current().record("state_call_count", u64::from(metrics.calls));
        tracing::Span::current().record("state_write_count", u64::from(metrics.writes));
        tracing::Span::current().record("state_bytes_before", metrics.bytes_before);
        tracing::Span::current().record("state_bytes_after", metrics.bytes_after);
        tracing::Span::current().record("transaction_result", result);
    }

    fn record_call_metrics(&mut self, elapsed: std::time::Duration) {
        self.guest_call_count = self
            .guest_call_count
            .checked_add(1)
            .expect("guest call count exhausted");
        tracing::Span::current().record("elapsed_microseconds", elapsed.as_micros() as u64);
        if let Ok(fuel_after) = self.store.get_fuel() {
            tracing::Span::current().record("fuel_after", fuel_after);
        }
    }

    fn classify_guest_failure(
        &self,
        source: wasmtime::Error,
        turn_id: Option<u64>,
        operation: &str,
    ) -> RuntimeError {
        let trap = source.downcast_ref::<wasmtime::Trap>().copied();
        let hostcall_fuel_exhausted = source
            .root_cause()
            .to_string()
            .contains("fuel allocated for hostcalls has been exhausted");
        let context = self
            .context(format!("guest trapped while {operation}"), turn_id)
            .with_source(source);
        if self.store.data().limiter.limit_hit {
            RuntimeError::MemoryLimitExceeded(context)
        } else if hostcall_fuel_exhausted {
            RuntimeError::TransferLimitExceeded(context)
        } else if trap == Some(wasmtime::Trap::OutOfFuel) {
            RuntimeError::FuelExhausted(context)
        } else if trap == Some(wasmtime::Trap::Interrupt) {
            RuntimeError::DeadlineExceeded(context)
        } else {
            RuntimeError::GuestTrap(context)
        }
    }

    fn enter_fault(&mut self, error: RuntimeError) -> RuntimeError {
        self.fault = Some(AppFault {
            category: error.category(),
            message: error.context().message.clone(),
        });
        self.lifecycle = AppLifecycle::Faulted;
        let span = info_span!(
            "app.fault",
            app_id = %self.app_id,
            component_id = %self.component_id,
            category = ?error.category(),
            turn_id = error.context().turn_id.unwrap_or(0),
        );
        span.in_scope(|| {});
        error
    }

    fn context(&self, message: impl Into<String>, turn_id: Option<u64>) -> ErrorContext {
        ErrorContext::new(message, &self.component_id, self.lifecycle, turn_id)
    }
}

fn instantiate(
    engine: &Engine,
    component: Component,
    component_id: String,
    app_id: youth_state::AppId,
    state_location: StateLocation,
    limits: RuntimeLimits,
    reconcile_on_open: bool,
) -> Result<YouthApp, RuntimeError> {
    let deadline_clock = Arc::clone(&limits.time.deadline_clock);
    let wake_driver = Arc::clone(&limits.time.wake_driver);
    let guest_monotonic_clock = Arc::clone(&limits.time.guest_monotonic_clock);
    let notification_dispatcher = Arc::clone(&limits.time.notification_dispatcher);
    let clipboard_service = Arc::clone(&limits.time.clipboard_service);
    let protocol = detect_protocol(engine, &component).ok_or_else(|| {
        RuntimeError::UnsupportedWorld(ErrorContext::new(
            "component does not export a supported Youth application world",
            &component_id,
            AppLifecycle::Loaded,
            None,
        ))
    })?;
    let mut state_store =
        youth_state::StateStore::open_for_app(state_location, limits.state, app_id.clone())
            .map_err(|source| {
                RuntimeError::StateUnavailable(
                    ErrorContext::new(
                        "application state could not be opened",
                        &component_id,
                        AppLifecycle::Loaded,
                        None,
                    )
                    .with_source(source),
                )
            })?;
    if reconcile_on_open {
        let scheduler_outputs = state_store
            .reconcile_overdue(deadline_clock.now_epoch_millis())
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
        youth_state::execute_wake_outputs(wake_driver.as_ref(), &scheduler_outputs);
        dispatch_schedule_notifications(
            &scheduler_outputs,
            notification_dispatcher.as_ref(),
            &state_store,
        );
    }
    let mut wasi = WasiCtxBuilder::new();
    wasi.monotonic_clock(GuestClockAdapter(guest_monotonic_clock));
    let state = HostState {
        table: ResourceTable::new(),
        wasi: wasi.build(),
        limiter: MemoryLimiter {
            maximum: limits.max_linear_memory,
            max_table_elements: limits.max_table_elements,
            limit_hit: false,
        },
        state: state_store,
        deadline_clock,
        wake_driver,
        notification_dispatcher,
        clipboard_service,
        staged_schedule_outputs: Vec::new(),
        editor_sessions: crate::editor_session::EditorSessionRegistry::new(),
        staged_editor_sessions: None,
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limiter);
    // Instantiation runs guest start code, so it is budgeted like any
    // other guest call. A sentinel deadline is not usable here:
    // Wasmtime adds the delta to the current epoch and would overflow.
    store.set_epoch_deadline(deadline_ticks(limits.mount.deadline));
    store.epoch_deadline_trap();
    store.set_hostcall_fuel(limits.max_guest_to_host_transfer);
    store.set_fuel(limits.mount.fuel).map_err(|source| {
        RuntimeError::Internal(
            ErrorContext::new(
                "failed to budget fuel for instantiation",
                &component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let bindings = match protocol {
        ProtocolVersion::V002 => {
            let mut linker = Linker::<HostState>::new(engine);
            configure_wasi(&mut linker, &component_id)?;
            crate::bindings::v002::Application::add_to_linker::<_, HasSelf<HostState>>(
                &mut linker,
                |state| state,
            )
            .map_err(|source| link_configuration_error(&component_id, source))?;
            let pre = linker
                .instantiate_pre(&component)
                .map_err(|source| link_error(&component_id, source))?;
            let pre = crate::bindings::v002::ApplicationPre::new(pre)
                .map_err(|source| unsupported_world_error(&component_id, protocol, source))?;
            let value = pre
                .instantiate(&mut store)
                .map_err(|source| instantiation_error(&component_id, &store, source))?;
            ApplicationBindings::V002(value)
        }
        ProtocolVersion::V003 => {
            let mut linker = Linker::<HostState>::new(engine);
            configure_wasi(&mut linker, &component_id)?;
            crate::bindings::v003::Application::add_to_linker::<_, HasSelf<HostState>>(
                &mut linker,
                |state| state,
            )
            .map_err(|source| link_configuration_error(&component_id, source))?;
            let pre = linker
                .instantiate_pre(&component)
                .map_err(|source| link_error(&component_id, source))?;
            let pre = crate::bindings::v003::ApplicationPre::new(pre)
                .map_err(|source| unsupported_world_error(&component_id, protocol, source))?;
            let value = pre
                .instantiate(&mut store)
                .map_err(|source| instantiation_error(&component_id, &store, source))?;
            ApplicationBindings::V003(value)
        }
        ProtocolVersion::V004 => {
            let mut linker = Linker::<HostState>::new(engine);
            configure_wasi(&mut linker, &component_id)?;
            crate::bindings::v004::Application::add_to_linker::<_, HasSelf<HostState>>(
                &mut linker,
                |state| state,
            )
            .map_err(|source| link_configuration_error(&component_id, source))?;
            let pre = linker
                .instantiate_pre(&component)
                .map_err(|source| link_error(&component_id, source))?;
            let pre = crate::bindings::v004::ApplicationPre::new(pre)
                .map_err(|source| unsupported_world_error(&component_id, protocol, source))?;
            let value = pre
                .instantiate(&mut store)
                .map_err(|source| instantiation_error(&component_id, &store, source))?;
            ApplicationBindings::V004(value)
        }
        ProtocolVersion::V005 => {
            let mut linker = Linker::<HostState>::new(engine);
            configure_wasi(&mut linker, &component_id)?;
            crate::bindings::v005::Application::add_to_linker::<_, HasSelf<HostState>>(
                &mut linker,
                |state| state,
            )
            .map_err(|source| link_configuration_error(&component_id, source))?;
            let pre = linker
                .instantiate_pre(&component)
                .map_err(|source| link_error(&component_id, source))?;
            let pre = crate::bindings::v005::ApplicationPre::new(pre)
                .map_err(|source| unsupported_world_error(&component_id, protocol, source))?;
            let value = pre
                .instantiate(&mut store)
                .map_err(|source| instantiation_error(&component_id, &store, source))?;
            ApplicationBindings::V005(value)
        }
        ProtocolVersion::V006 => {
            let mut linker = Linker::<HostState>::new(engine);
            configure_wasi(&mut linker, &component_id)?;
            crate::bindings::v006::Application::add_to_linker::<_, HasSelf<HostState>>(
                &mut linker,
                |state| state,
            )
            .map_err(|source| link_configuration_error(&component_id, source))?;
            let pre = linker
                .instantiate_pre(&component)
                .map_err(|source| link_error(&component_id, source))?;
            let pre = crate::bindings::v006::ApplicationPre::new(pre)
                .map_err(|source| unsupported_world_error(&component_id, protocol, source))?;
            let value = pre
                .instantiate(&mut store)
                .map_err(|source| instantiation_error(&component_id, &store, source))?;
            ApplicationBindings::V006(value)
        }
    };
    Ok(YouthApp {
        component_id,
        app_id,
        limits,
        store,
        bindings,
        tree: None,
        lifecycle: AppLifecycle::Loaded,
        last_event_sequence: None,
        last_turn: None,
        guest_call_count: 0,
        fault: None,
    })
}

fn detect_protocol(engine: &Engine, component: &Component) -> Option<ProtocolVersion> {
    let component_type = component.component_type();
    let exports = component_type.exports(engine);
    let mut v002 = false;
    let mut v003 = false;
    let mut v004 = false;
    let mut v005 = false;
    let mut v006 = false;
    for (name, _) in exports {
        v002 |= name == "youth:app/lifecycle@0.0.2";
        v003 |= name == "youth:app/lifecycle@0.0.3";
        v004 |= name == "youth:app/lifecycle@0.0.4";
        v005 |= name == "youth:app/lifecycle@0.0.5";
        v006 |= name == "youth:app/lifecycle@0.0.6";
    }
    match (v002, v003, v004, v005, v006) {
        (false, false, false, false, true) => Some(ProtocolVersion::V006),
        (false, false, false, true, false) => Some(ProtocolVersion::V005),
        (false, false, true, false, false) => Some(ProtocolVersion::V004),
        (false, true, false, false, false) => Some(ProtocolVersion::V003),
        (true, false, false, false, false) => Some(ProtocolVersion::V002),
        _ => None,
    }
}

fn configure_wasi(linker: &mut Linker<HostState>, component_id: &str) -> Result<(), RuntimeError> {
    wasmtime_wasi::p2::add_to_linker_sync(linker).map_err(|source| {
        RuntimeError::LinkFailure(
            ErrorContext::new(
                "failed to configure closed WASIp2 imports",
                component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })
}

fn link_configuration_error(component_id: &str, source: wasmtime::Error) -> RuntimeError {
    RuntimeError::LinkFailure(
        ErrorContext::new(
            "failed to configure Youth state imports",
            component_id,
            AppLifecycle::Loaded,
            None,
        )
        .with_source(source),
    )
}

fn link_error(component_id: &str, source: wasmtime::Error) -> RuntimeError {
    RuntimeError::LinkFailure(
        ErrorContext::new(
            "component imports are incompatible with the Youth host",
            component_id,
            AppLifecycle::Loaded,
            None,
        )
        .with_source(source),
    )
}

fn unsupported_world_error(
    component_id: &str,
    protocol: ProtocolVersion,
    source: wasmtime::Error,
) -> RuntimeError {
    RuntimeError::UnsupportedWorld(
        ErrorContext::new(
            format!("component does not export {}", protocol.world()),
            component_id,
            AppLifecycle::Loaded,
            None,
        )
        .with_source(source),
    )
}

fn instantiation_error(
    component_id: &str,
    store: &Store<HostState>,
    source: wasmtime::Error,
) -> RuntimeError {
    let context = ErrorContext::new(
        "component instantiation failed",
        component_id,
        AppLifecycle::Loaded,
        None,
    )
    .with_source(source);
    if store.data().limiter.limit_hit {
        RuntimeError::MemoryLimitExceeded(context)
    } else {
        RuntimeError::InstantiationFailure(context)
    }
}

/// Lists the interfaces a component imports, sorted and deduplicated.
///
/// Youth's WASI context is closed, so these imports are inert: the host
/// decides what they return. They are still a real linking and
/// compatibility surface, so the import list is treated as a dependency
/// budget rather than left implicit (see `docs/GUEST-PROFILE.md`).
///
/// Names are returned without their `@version` suffix so that a patch
/// bump in the WASI snapshot does not read as a new capability.
pub fn component_imports(path: impl AsRef<Path>) -> Result<Vec<String>, RuntimeError> {
    let path = path.as_ref();
    let component_id = component_identity(path);
    let limits = RuntimeLimits::default();
    let engine = crate::shared_engine();
    let bytes = read_component(path, &component_id, &limits)?;
    let component = Component::new(engine, &bytes).map_err(|source| {
        RuntimeError::InvalidComponent(
            ErrorContext::new(
                "file is not a valid WebAssembly component",
                &component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let mut imports: Vec<String> = component
        .component_type()
        .imports(engine)
        .map(|(name, _)| {
            name.split_once('@')
                .map_or(name, |(base, _)| base)
                .to_owned()
        })
        .collect();
    imports.sort_unstable();
    imports.dedup();
    Ok(imports)
}

fn read_component(
    path: &Path,
    component_id: &str,
    limits: &RuntimeLimits,
) -> Result<Vec<u8>, RuntimeError> {
    let file = File::open(path).map_err(|source| {
        RuntimeError::InvalidComponent(
            ErrorContext::new(
                "component file could not be opened",
                component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    let metadata = file.metadata().map_err(|source| {
        RuntimeError::InvalidComponent(
            ErrorContext::new(
                "component file metadata could not be read",
                component_id,
                AppLifecycle::Loaded,
                None,
            )
            .with_source(source),
        )
    })?;
    if metadata.len() > limits.max_component_size as u64 {
        return Err(RuntimeError::ComponentTooLarge(ErrorContext::new(
            format!(
                "component is {} bytes, exceeding the {}-byte limit",
                metadata.len(),
                limits.max_component_size
            ),
            component_id,
            AppLifecycle::Loaded,
            None,
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(limits.max_component_size as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| {
            RuntimeError::InvalidComponent(
                ErrorContext::new(
                    "component file could not be read",
                    component_id,
                    AppLifecycle::Loaded,
                    None,
                )
                .with_source(source),
            )
        })?;
    if bytes.len() > limits.max_component_size {
        return Err(RuntimeError::ComponentTooLarge(ErrorContext::new(
            format!(
                "component exceeds the {}-byte limit",
                limits.max_component_size
            ),
            component_id,
            AppLifecycle::Loaded,
            None,
        )));
    }
    Ok(bytes)
}

fn component_identity(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| PathBuf::from(path).display().to_string(), String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_growth_is_bounded_and_recorded_as_a_resource_limit() {
        let mut limiter = MemoryLimiter {
            maximum: 1024,
            max_table_elements: 4,
            limit_hit: false,
        };

        assert!(limiter.table_growing(0, 4, None).unwrap());
        assert!(!limiter.table_growing(4, 5, None).unwrap());
        assert!(limiter.limit_hit);
    }
}
