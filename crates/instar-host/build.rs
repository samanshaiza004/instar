//! Builds the hostile guest for the bridge acceptance gate.
//!
//! The bridge's properties are about what happens when things go wrong —
//! while a generation is dying, while a hundred events are queued, while a
//! commit is unanswerable — so its guest is the one that misbehaves on
//! request. See `guests/hostile`.

fn main() {
    instar_guest_build::build_guest("hostile", "hostile", "HOSTILE_WASM");
}
