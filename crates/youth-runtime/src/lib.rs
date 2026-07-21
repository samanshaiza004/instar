//! Youth host runtime.
//!
//! Owns the Wasmtime engine, component compilation, WIT bindings,
//! closed WASI context, per-app store and worker, resource limits,
//! event sequencing, and wire conversion to `youth-tree` values.

#![forbid(unsafe_code)]

mod bindings;
mod config;
mod engine;
mod error;
mod host;
mod limits;
mod profile;
mod wire;
mod worker;

pub use config::{AppId, StateLocation, YouthAppConfig};
pub use engine::{configured_engine, shared_engine};
pub use error::{ErrorContext, RuntimeError, RuntimeErrorCategory};
pub use host::{AppFault, AppInspection, AppLifecycle, TurnReceipt, YouthApp, component_imports};
pub use limits::{CallBudget, RuntimeLimits};
pub use profile::{
    APPLICATION_PROTOCOL, APPLICATION_WORLD, ComponentValidation, ComponentValidationError,
    PERMITTED_GUEST_IMPORTS, REQUIRED_GUEST_IMPORTS, validate_component,
};
pub use worker::YouthAppHandle;
pub use youth_state::{GuestCallPhase, StateLimits, StateSummary, StateValue, TurnStateMetrics};
// Public because they are the associated `Error` type of the fallible
// wire conversions into `youth_tree` values (spec section 7).
pub use wire::from_guest::{WireError, WireErrorKind};
