//! Builds the two kernel-level guests. See `instar-guest-build` for why guests
//! are compiled from source rather than committed as artifacts.
//!
//! Neither of these touches the UI: they exercise the kernel's own contracts —
//! idle suspension for Gate 0, and generations and operations for WP4.

fn main() {
    instar_guest_build::build_guests(&[
        (
            "kernel-spike-guest",
            "kernel-spike-guest",
            "KERNEL_SPIKE_GUEST_WASM",
        ),
        ("kernel-guest", "kernel-guest", "KERNEL_GUEST_WASM"),
    ]);
}
