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

## The Editor is the one exception to "the guest decides"

An `Editor` node's live buffer, cursor, selection, IME composition, undo
history, and scroll offset are entirely host-local — the guest never sees a
`handle`/`view` turn for ordinary typing, and never receives byte offsets or
platform key events. The guest only sees the editor through whole-buffer
`snapshot`/`accept`/`replace` calls it makes explicitly, typically from an
ordinary command like Save. This is a deliberate, narrow exception to the
"guest decides semantics, host decides everything else" split above: Youth
treats live text editing as host presentation state, the same way it treats
scrolling or pointer hover, and only asks the guest to agree on accepted
document content at explicit checkpoints. See
[docs/MILESTONE-2.md](../../MILESTONE-2.md) for the full ownership contract.
