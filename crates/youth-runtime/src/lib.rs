//! Youth host runtime.
//!
//! Owns the Wasmtime engine, component compilation, WIT bindings,
//! closed WASI context, per-app store and worker, resource limits,
//! event sequencing, and wire conversion to `youth-tree` values.

#![forbid(unsafe_code)]

mod bindings;
mod engine;
mod error;
mod host;
mod limits;
mod wire;
mod worker;

pub use engine::{configured_engine, shared_engine};
pub use error::{ErrorContext, RuntimeError, RuntimeErrorCategory};
pub use host::{AppFault, AppInspection, AppLifecycle, TurnReceipt, YouthApp, component_imports};
pub use limits::{CallBudget, RuntimeLimits};
pub use worker::YouthAppHandle;
// Public because they are the associated `Error` type of the fallible
// wire conversions into `youth_tree` values (spec section 7).
pub use wire::from_guest::{WireError, WireErrorKind};
