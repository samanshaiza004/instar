//! Youth host runtime.
//!
//! Owns the Wasmtime engine, component compilation, WIT bindings,
//! closed WASI context, per-app store and worker, resource limits,
//! event sequencing, and wire conversion to `youth-tree` values.

#![forbid(unsafe_code)]
