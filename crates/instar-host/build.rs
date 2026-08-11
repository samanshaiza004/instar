//! Builds the guests the bridge acceptance gate and the HARDEN-3 measurement
//! table run: hostile for failure modes, plus counter and gallery so the
//! resource policy has evidence for every UI fixture in the repository.
//!
//! The bridge's properties are about what happens when things go wrong —
//! while a generation is dying, while a hundred events are queued, while a
//! commit is unanswerable — so its guest is the one that misbehaves on
//! request. See `guests/hostile`.

fn main() {
    instar_guest_build::build_guests(&[
        ("hostile", "hostile", "HOSTILE_WASM"),
        ("counter", "counter", "COUNTER_WASM"),
        ("gallery", "gallery", "GALLERY_WASM"),
    ]);
}
