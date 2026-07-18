//! Pure retained semantic-tree engine.
//!
//! This crate must not depend on Wasmtime, WASI, Tokio, rendering
//! libraries, or platform APIs. The tree correctness core is testable
//! and reusable without executing Wasm.

#![forbid(unsafe_code)]
