//! Builds the counter guest the shell runs and the render tests assert on.

fn main() {
    instar_guest_build::build_guest("counter", "counter", "COUNTER_WASM");
}
