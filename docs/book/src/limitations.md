# Limitations

Developer Preview 0 deliberately proves tooling around the existing runtime.
It is not a general application platform yet.

- Rendering and its framebuffer fixtures are provisional.
- Layout is column-only.
- Input is mouse-only; keyboard and accessibility are absent.
- Development uses validated rebuild-and-restart, not hot reload.
- An application owns one native window.
- The application protocol is fixed at `0.0.2`.
- Guests are Rust-only and target `wasm32-wasip2`.
- `youth build` emits a bare validated component, not an installable package.
- Packaging, publishing, registries, and SDK upgrades are absent.

Reactive UI, row composition, images, animation, multiple windows, arbitrary
SQL, and other runtime features remain outside DP0.
