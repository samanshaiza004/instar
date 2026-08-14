//! C4a: the kernel's hostcall-fuel budget stays comfortably above
//! `instar-text`'s buffer size ceiling.
//!
//! `instar-kernel` cannot name `instar_text::MAX_TEXT_BUFFER_BYTES`: it is
//! forbidden from depending on `instar-text` (the whole point of the
//! `OpaqueResourceKey` bridge vocabulary), so `TEXT_TRANSFER_HOSTCALL_FUEL` is
//! a literal with the relationship asserted here, in the only crate where
//! both sides are visible -- the same pattern `attachment_bound.rs` uses for
//! `MAX_TEXT_ATTACHMENTS` and `MAX_NODES`.
//!
//! Strictly greater, not merely greater-or-equal: a maximum legal
//! `MAX_TEXT_BUFFER_BYTES`-sized transfer must still fit inside the fuel
//! budget with room to spare, or a legitimate `create-buffer` at the ceiling
//! would be indistinguishable from a hostile one that Wasmtime is meant to
//! stop before `TextHost` ever sees it.

/// The fuel budget must have real headroom above the size ceiling it
/// contains, not just cover it exactly.
#[test]
fn hostcall_fuel_has_headroom_above_the_buffer_ceiling() {
    assert!(
        instar_kernel::resource::TEXT_TRANSFER_HOSTCALL_FUEL > instar_text::MAX_TEXT_BUFFER_BYTES,
        "a maximum legal bootstrap ({} bytes) must lift comfortably inside \
         the fuel budget ({} bytes), or there is no room left for the \
         oversized-but-liftable case Instar's own ceiling is supposed to \
         refuse",
        instar_text::MAX_TEXT_BUFFER_BYTES,
        instar_kernel::resource::TEXT_TRANSFER_HOSTCALL_FUEL,
    );
}
