# Milestone 2: Editor Capability and Modifier-Aware Shortcuts

Milestone 2 adds Youth's first text-entry surface: a host-owned `Editor` node
and the `youth:editor` capability (application protocol `0.0.6`), then a
follow-up protocol (`0.0.7`) that gives declared shortcuts a modifier so a
focused Editor and an app-level `Primary+S` can coexist. Both protocols stay
byte-for-byte frozen once shipped; `0.0.2` through `0.0.7` all remain
supported simultaneously.

## Ownership boundary

A stable `Editor` node identity implicitly owns one host editor session
while that node is installed in the tree. The live buffer, cursor,
selection, IME composition, scroll offset, and undo history are entirely
host-local — the guest never receives raw byte offsets, grapheme indexes, or
platform key events for ordinary typing:

```text
first installation of an Editor node       -> create a host session from the
                                                guest's declared text and
                                                document-revision
resync with the same node, same revision    -> preserve the live session
Editor node removed                          -> destroy the host session
same node ID later reintroduced              -> a new host generation (never
                                                 exposed to the guest)
guest reconstructs a stale view()            -> never overwrites live host
                                                 edits
```

## Capability surface

`youth:editor/session@0.0.1` exposes exactly three operations, all
whole-buffer, none exposing patches:

```wit
snapshot: func(editor: node-id) -> result<editor-snapshot, editor-error-code>;
accept: func(editor, expected-document-revision, expected-edit-sequence,
             new-document-revision) -> result<_, editor-error-code>;
replace: func(editor, expected-document-revision, expected-edit-sequence,
              new-document-revision, authoritative-text)
             -> result<_, editor-error-code>;
```

`document-revision` is guest-owned (accepted document meaning);
`edit-sequence` is host-owned (local buffer-change ordering since the last
accept/replace). `accept` and `replace` both fail closed
(`stale-document-revision` / `stale-edit-sequence`) without touching the
buffer when either expected value no longer matches — a stale guest call can
never overwrite a newer host edit. `replace` additionally resets cursor,
selection, active IME composition, scroll, and undo history for the
installed text.

## No host-originated turns for typing

Typing produces zero guest turns and zero durable events: the host buffer,
cursor, and selection change entirely locally. A guest only sees the editor
again when it explicitly asks — a declared `Primary+S` Save command falls
through editor input handling to an ordinary semantic activation, which then
calls `snapshot()` and `accept()`/`replace()` in that one guest turn.
`crates/youth-test/src/lib.rs`'s `measure` command proves this directly from
an application's own `.youth-test` suite (`measure expect "typing"
guest-turns 0`).

## Input precedence

```text
1. Active IME composition
2. Focused Editor text/movement input (Escape always falls through)
3. Tab / Arrow host focus navigation (when no Editor consumes it)
4. Enter / Escape / Backspace declared shortcuts, else the focused target
5. Primary+Character declared shortcuts
6. Plain Character declared shortcuts
```

The guest still receives no native key events, only semantic activation.
`youth_interaction::InteractionState::key` is the single implementation of
this precedence, covered by headless tests independent of any window.

## Host-local editing mechanics

- **Undo/redo** — bounded reversible deltas (not whole-buffer snapshots),
  capped at 64 MiB of history. Continuous typing merges into one unit until
  a cursor move, selection change, paste, or IME commit boundary.
- **Clipboard** — behind a `ClipboardService` seam; a recording headless
  implementation backs tests, a native implementation backs the desktop
  host. Clipboard contents never pass through the guest for ordinary paste.
- **Resource limits** — 1 MiB of committed Editor text
  (`youth_tree::Limits::max_editor_text_len`), 16 KiB of in-progress IME
  preedit (`RuntimeLimits::max_ime_preedit_bytes`). A rejected edit leaves
  the buffer, history, and sequence numbers unchanged.
- **Accessibility-ID allocation** — an `AccessibilityIdRegistry` reserves
  installed semantic IDs and allocates disjoint IDs for editor text runs,
  with collision and exhaustion detection.

## Rendering and accessibility

`crates/youth-text-render-cpu` (`#![forbid(unsafe_code)]`, matching
`youth-desktop`) provides real Unicode text layout, line-breaking, cursor
and selection geometry, and hit-testing via Parley, rasterized through a
Swash glyph cache. `youth-desktop` wires native IME
(`WindowEvent::Ime`/`set_ime_cursor_area`), pointer-driven selection, wheel
and cursor-follow scrolling (host-owned, no guest turn), and AccessKit text
ranges, selection, cursor, and editing actions.

## Modifier-aware shortcuts (protocol 0.0.7)

`0.0.6`'s `shortcut-key` variant had no modifier field, so the host's
shortcut fallback explicitly excluded any Control/Super-held character —
no app could declare a real `Ctrl`/`Cmd+S`. `0.0.7` adds an additive
`shortcut` record wrapping the existing key variant:

```wit
flags shortcut-modifiers { primary }
record shortcut { key: shortcut-key, modifiers: shortcut-modifiers }
```

Only `primary` (the platform convention — `Cmd` on macOS, `Control` on
Windows/Linux, resolved to one logical modifier by the host) is supported
today. `Character("s")` and `Primary+Character("s")` are distinct chords and
may coexist on different buttons; duplicate chords on two buttons remain a
validation error. Protocols `0.0.2` through `0.0.6` map their unmodified
`shortcut-key` onto the same `Shortcut { key, modifiers: empty }` shape with
zero semantic change — this is purely additive.

## Non-goals

This milestone does not add rich text, multiple cursors, spell-check,
find/replace, syntax highlighting, collaborative editing, a second modifier
combination beyond `primary`, or a `Patch::SetEditorRevision`-shaped tree
patch (see the known convergence-checking gap in
[docs/book/src/testing.md](book/src/testing.md)).

## Completion gates

1. Headless tests prove 10,000 synthetic edits produce zero guest turns and
   zero events, and that `accept`/`replace` reject a stale
   `document-revision` or `edit-sequence` without touching the buffer.
2. Undo/redo grouping, the clipboard seam, and `replace`'s reset policy
   (cursor/selection/IME/scroll/undo) are covered by dedicated tests.
3. `youth-text-render-cpu` passes bundled-font snapshot tests (exact on
   Linux CI, glyph-count/geometry-bounded elsewhere) and a native
   no-crash smoke test on all three hosts.
4. Scratchpad, the first Editor-capable application, mounts a real
   component with no raw WIT, zero per-keystroke guest turns, and a Save
   command that exercises `snapshot`/`accept` end to end.
5. Protocol `0.0.7`'s decisive regression (a focused Editor still declines
   `Primary+S` and the fallthrough activates Save with exactly one guest
   call) passes alongside the frozen `0.0.2`–`0.0.6` mount fixtures.
