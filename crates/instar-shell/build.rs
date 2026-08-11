//! Builds the guests the shell runs and its tests assert on: the counter for
//! the render pipeline, and the Gallery for the input-path integration tests.

fn main() {
    instar_guest_build::build_guest("counter", "counter", "COUNTER_WASM");
    instar_guest_build::build_guest("gallery", "gallery", "GALLERY_WASM");
    instar_guest_build::build_guest("calculator", "calculator", "CALCULATOR_WASM");
}
