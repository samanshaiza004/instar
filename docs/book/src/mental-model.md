# Mental model

A Youth application is a WebAssembly component that returns a retained
semantic tree. The guest decides what the interface means; the host owns the
native window, layout, pixels, input delivery, durable SQLite state, and the
transaction boundary.

During mount, the host opens a state transaction, calls the application's
read-only `view`, validates the complete tree, commits state, and installs the
tree. During an event, the host stages state and tree changes, validates both,
commits SQLite, installs the authoritative tree, and only then publishes the
patch to the renderer. A failed turn cannot expose state that disagrees with
the visible tree.

`youth-sdk` hides WIT bindings, revisions, acknowledgements, raw patches, and
component export plumbing. The WIT files in an application remain an
inspectable, language-neutral protocol snapshot; they are not a second Rust
binding source.
