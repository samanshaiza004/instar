# Limitations

Developer Preview 1 proves the first Utility Suite application on the runtime.
It is not a general application platform yet.

- Rendering and its framebuffer fixtures are provisional.
- Layout is limited to deterministic columns, rows, and equal-track grids;
  styling, spans, and arbitrary constraints are absent.
- Mouse and bounded logical keyboard input are supported. Native accessibility
  projection is still absent (0%); focus remains host-owned so it can be added
  without changing guests.
- Development uses validated rebuild-and-restart, not hot reload.
- An application owns one native window.
- The current application protocol is `0.0.3`; the host also runs `0.0.2`.
- Guests are Rust-only and target `wasm32-wasip2`.
- `youth build` emits a bare validated component, not an installable package.
- Packaging, publishing, registries, and SDK upgrades are absent.

Reactive UI, text input/IME, images, animation, multiple windows, arbitrary
SQL, expression parsing, and scientific calculator behavior remain outside
DP1.
