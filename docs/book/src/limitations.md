# Limitations

Developer Preview 3 proves the external workflow and three Utility Suite
applications on the runtime; Milestone 2 adds a host-owned Editor capability
and modifier-aware shortcuts. Scratchpad Gate B adds one explicitly granted
existing UTF-8 text document and a post-commit Save effect. Youth is still an architecture-probing
platform, not a general application platform.

- Rendering and its framebuffer fixtures are provisional outside the Editor's
  Parley/Swash text path (`crates/youth-editor-engine`,
  `crates/youth-text-render-cpu`); other node kinds still use the earlier
  provisional bitmap-font renderer.
- Layout is limited to deterministic columns, rows, and equal-track grids;
  styling, spans, and arbitrary constraints are absent.
- Mouse, bounded logical keyboard input, and one Editor node's real text
  entry (typing, selection, IME composition, undo/redo, clipboard, scrolling)
  are supported. Native accessibility (AccessKit) covers the Editor's text
  ranges, selection, cursor, and editing actions; other node kinds have no
  accessibility projection yet, and no standardized completeness inventory
  has been collected. Focus remains host-owned.
- Only one modifier (`primary` — `Cmd` on macOS, `Control` on Windows/Linux)
  is supported on declared shortcuts; no other modifier combinations, and no
  chord sequences.
- Development uses validated rebuild-and-restart, not hot reload.
- An application owns one native window.
- The latest application protocol is `0.0.9`; the host also runs `0.0.8`, `0.0.7`,
  `0.0.6`, `0.0.5`, `0.0.4`, `0.0.3`, and `0.0.2` simultaneously. Generated
  projects default to `0.0.9`; older profiles remain available for compatibility.
  Protocol `0.0.9` adds a wire-node `grow` field for forthcoming responsive
  layout, but the current host layout engine does not use it yet.
- Guests are Rust-only and target `wasm32-wasip2`.
- `youth build` emits a bare validated component; `youth-cli` itself is
  packaged for release via `dist` (see
  [docs/DISTRIBUTION.md](https://github.com/samanshaiza004/youth/blob/master/docs/DISTRIBUTION.md)),
  but application components have no installable-package or registry story.
- Publishing, registries, and SDK upgrade tooling for applications are absent.

Scratchpad's text-document grant is deliberately narrow: one existing regular
file, no picker, listing, creation, rename, delete, watching, reload/merge,
overwrite confirmation, Save Copy, or multiple documents. The file must be
valid UTF-8 and at most 1 MiB after its optional BOM. Replacement preserves
ordinary permissions where supported but does not promise universal ACL,
xattr, ownership, resource-fork, or power-loss durability. A durable external-
effect journal is deferred.

Reactive UI, images, animation, multiple windows, arbitrary SQL, expression
parsing, and application-level packaging/publishing remain outside the
current previews. The Editor node is one whole-buffer plain-text surface —
rich text, multiple cursors, spell-check, find/replace, and syntax
highlighting are absent. Todo additionally does not provide text entry,
scrolling, list nodes, structured state, state enumeration, or automatic tree
diffing.
