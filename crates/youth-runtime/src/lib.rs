//! Youth host runtime.
//!
//! Owns the Wasmtime engine, component compilation, WIT bindings,
//! closed WASI context, per-app store and worker, resource limits,
//! event sequencing, and wire conversion to `youth-tree` values.

#![forbid(unsafe_code)]

mod bindings;
mod config;
mod editor_session;
mod engine;
mod error;
mod host;
mod limits;
mod presentation;
mod profile;
mod wire;
mod worker;

pub use config::{
    AppId, ClipboardError, ClipboardService, GuestMonotonicClock, NotificationDispatcher,
    RecordingClipboardService, RecordingNotificationDispatcher, RuntimeTimeSeams, StateLocation,
    SystemClipboardService, SystemGuestMonotonicClock, SystemNotificationDispatcher,
    VirtualGuestMonotonicClock, YouthAppConfig,
};
pub use editor_session::{EditorLocalEdit, EditorLocalEditResult};
pub use engine::{configured_engine, shared_engine};
pub use error::{ErrorContext, RuntimeError, RuntimeErrorCategory};
pub use host::{
    AppFault, AppInspection, AppLifecycle, ScheduleWake, TurnReceipt, ViewVerification,
    WakeDisposition, YouthApp, component_imports,
};
pub use limits::{CallBudget, RuntimeLimits};
pub use presentation::{next_display_boundary_epoch_millis, resolve_countdown_display};
pub use profile::{
    APPLICATION_PROTOCOL, APPLICATION_WORLD, ComponentValidation, ComponentValidationError,
    PERMITTED_GUEST_IMPORTS, REQUIRED_GUEST_IMPORTS, SUPPORTED_APPLICATION_PROTOCOLS,
    validate_component,
};
pub use worker::{
    Generation, PresentationReader, RequestId, RuntimeEvent, ScheduleId, TurnOrigin, TurnOutcome,
    YouthAppHandle,
};
pub use youth_state::{
    DeadlineClock, GuestCallPhase, PendingDelivery, SchedulerInput, SchedulerOutput, StateLimits,
    StateSummary, StateValue, SystemDeadlineClock, SystemWakeDriver, TurnStateMetrics,
    VirtualDeadlineClock, VirtualWakeDriver, WakeDriver, WakeSink, WakeToken,
};
// Public because they are the associated `Error` type of the fallible
// wire conversions into `youth_tree` values (spec section 7).
pub use wire::from_guest::{WireError, WireErrorKind};
