# Limitations

Developer Preview 3 proves the external workflow and three Utility Suite
applications on the runtime. Youth is still an architecture-probing platform,
not a general application platform.

- Rendering and its framebuffer fixtures are provisional.
- Layout is limited to deterministic columns, rows, and equal-track grids;
  styling, spans, and arbitrary constraints are absent.
- Mouse and bounded logical keyboard input are supported. Native accessibility
  projection is not yet available; no standardized completeness inventory has
  been collected. Focus remains host-owned so it can be added without changing
  guests.
- Development uses validated rebuild-and-restart, not hot reload.
- An application owns one native window.
- The latest application protocol is `0.0.5`; the host also runs `0.0.4`,
  `0.0.3`, and `0.0.2`. Generated Tally projects still default to `0.0.4`.
- Guests are Rust-only and target `wasm32-wasip2`.
- `youth build` emits a bare validated component, not an installable package.
- Packaging, publishing, registries, and SDK upgrades are absent.

Reactive UI, text input/IME, images, animation, multiple windows, arbitrary
SQL, expression parsing, packaging, publishing, and SDK upgrade tooling remain
outside the current previews. Todo additionally does not provide text entry,
scrolling, list nodes, structured state, state enumeration, or automatic tree
diffing.
