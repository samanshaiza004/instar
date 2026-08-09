//! Builds the counter guest for the interaction tests.
//!
//! The same guest the shell ships (WP8): a UI contract worth testing is worth
//! testing against the thing people actually run, and a fixture that drifts
//! from the real guest tests a program nobody has.

fn main() {
    instar_guest_build::build_guest("counter", "counter", "COUNTER_WASM");
}
