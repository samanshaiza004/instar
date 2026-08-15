//! Builds the guests the shell runs and its tests assert on: the counter for
//! the render pipeline, and the Gallery for the input-path integration tests.

fn main() {
    instar_guest_build::build_guests(&[
        ("counter", "counter", "COUNTER_WASM"),
        ("gallery", "gallery", "GALLERY_WASM"),
        ("calculator", "calculator", "CALCULATOR_WASM"),
        ("scratchpad", "instar-scratchpad", "SCRATCHPAD_WASM"),
    ]);
}
