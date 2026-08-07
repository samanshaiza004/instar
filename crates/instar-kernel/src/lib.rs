//! Pure Wasmtime Component Model async runtime for Instar.
//!
//! `instar-kernel` is deliberately narrow: engine configuration, component
//! loading/validation, WASI context, guest lifecycle, event delivery,
//! operation registry, cancellation, and runtime generations. It has no
//! rendering, windowing, layout, or UI dependency of any kind -- see
//! the Phase 1 plan's forbidden-dependency list (`instar-kernel` must never
//! depend on winit, Taffy, Vello, softbuffer, a text renderer, `instar-ui`,
//! or counter-specific types).
//!
//! As of WP3 this crate is still a spike, not a runtime. What exists:
//!
//! - [`engine`] -- the engine configuration, with Component Model async
//!   enabled and no polling thread.
//! - [`spike`] -- the Gate 0 harness: `bindgen!` wired against
//!   `wit/world.wit`, a linked WASI context, and a guest fixture driven
//!   through real suspend/wake, concurrency, and cancellation.
//!
//! Gate 0 passed on this toolchain; see `docs/GATE-0.md` for the findings and,
//! importantly, the one limitation it turned up (abandoned guest tasks retain
//! runtime state until their `Store` is dropped). The real kernel API --
//! component loading and validation, guest lifecycle, an operation registry,
//! runtime generations -- is not written yet, and the spike's event
//! "protocol" is a test fixture, not a draft of it.

pub mod engine;
pub mod spike;
