# Instar Phase 3 — Text

> **The question:** does Instar's ownership model survive a real text editor?

Nothing here is built. This is the contract, frozen before implementation, in
the order Phase 2 established as worth following.

Phase 2 answered whether a guest can describe a normal desktop interface while
the host owns presentation and transient interaction. Text is where that model
is actually tested, because typing is the fastest, most latency-sensitive,
most continuous interaction there is — and because an editor's document is
genuinely the guest's, not the host's.

---

## The one decision everything else follows from

```text
OS / IME
   ↓
TextView          caret, selection, IME composition, scrolling
   ↓
TextBuffer        the host's replica
   ↓
layout, caret, pixels
   ↓
async edit notification
   ↓
guest             canonical document, Tree-sitter, search, application truth
```

Typing must not be:

```text
OS → guest → document mutation → UI commit → host
```

That path puts a Wasm round trip between a keystroke and a glyph, and it
negates everything the last two phases built. A guest that stalls for 100 ms
would stall the caret.

So the host keeps a replica it can edit immediately, and the guest holds the
canonical document and hears about edits afterwards. That is a synchronization
problem, and pretending otherwise is how it becomes a correctness problem.

## Resources with explicit authority

```text
instar-text
├── TextBuffer     content and revisions
└── TextView       a presentation of a buffer: caret, selection, scroll

Scratchpad guest
└── canonical Document
```

Deliberately not a generic `document` service. A name like that invites both
sides to assume they own it, and the whole difficulty here is that neither
wholly does. Two views of one buffer is a supported case and a good test of
whether the split is real.

## The revision protocol

Host-local edits advance the replica immediately and are reported in order:

```rust
TextEdit {
    base_revision: Revision,
    resulting_revision: Revision,
    range: Range,
    replacement: ...,
}
```

Guest-originated changes state what they expected to be editing:

```text
ApplyEdits(expected_revision, edits)
    -> Applied(new_revision)
    -> Conflict(current_revision)
```

Enough to be rigorous about convergence without resurrecting the predecessor's
transaction machinery. A guest that loses a race is told, and re-reads; it is
never silently applied to text that moved underneath it.

## `textbench` comes before Scratchpad

A hostile experiment, not an application. Build it first and let it fail
cheaply.

```text
1 MB and 10 MB plain text

typing            backspace         selection
IME               paste 100 KB      undo / redo
find and reveal   guest replace-all
two views of one buffer
guest stalls of 20, 50 and 100 ms
```

Four things must hold before Scratchpad starts:

```text
1  input-to-pixel latency stays host-local and low
2  a stalled guest does not stall caret, selection, IME, or scroll
3  host and guest converge after edits and conflicts
4  1-10 MB documents do not create pathological memory or copy costs
```

The third is the one most likely to be quietly wrong, and the second is the one
the whole architecture claims.

## Order

```text
A  TextBuffer / TextView contract
B  host-local editing and IME
C  revision and edit synchronization
D  large-document textbench
E  selection, clipboard, undo
F  annotations and reveal APIs
G  workspace, files, dialogs
H  Scratchpad alpha
```

Navigation stays semantic throughout, as Phase 2 established for focus:
`reveal_range` and `ensure_visible`, never a scroll offset a guest computed
from geometry it does not own.
