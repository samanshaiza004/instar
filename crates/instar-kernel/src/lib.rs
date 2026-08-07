//! Pure Wasmtime Component Model async runtime for Instar.
//!
//! `instar-kernel` is deliberately narrow: engine configuration, component
//! loading/validation, WASI context, guest lifecycle, event delivery,
//! operation registry, cancellation, and runtime generations. It has no
//! rendering, windowing, layout, or UI dependency of any kind -- see
//! docs/PHASE-1.md's forbidden-dependency list (`instar-kernel` must never
//! depend on winit, Taffy, Vello, softbuffer, a text renderer, `instar-ui`,
//! or counter-specific types).
//!
//! As of WP2, this crate is a scaffold: the engine configuration (see
//! [`engine`]) is proven to build with Component Model async enabled and
//! no polling thread, and the Gate 0 spike WIT world lives at
//! `wit/world.wit`. Wiring `wasmtime::component::bindgen!` against that
//! world, instantiating a guest, and driving the actual suspend/wake proof
//! is WP3's job (docs/PHASE-1.md) -- getting the exact `bindgen!`
//! configuration right for genuinely async WIT imports is itself part of
//! what Gate 0 is testing, not something to assume in advance.

pub mod engine;
