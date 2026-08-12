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

`instar-text` is a **host resource subsystem independent of the semantic UI
tree**. `instar-host` composes it with `instar-ui`; a UI node may reference a
text-view resource, but UI-tree lifetime does not define buffer lifetime.

```text
              instar-host          composes both
             /           \
      instar-ui       instar-text
      semantic tree    TextBuffer / TextView / TextSystem
      focus, scroll    revisions, editing, selection
```

The criterion that puts it there, rather than inside `instar-ui` beside the
other host-owned state:

> Is this state's lifetime and meaning subordinate to the semantic UI tree?

Yes for `FocusState` and `ScrollState`. A `Scroll` *is* a tree node, focus
points at a `NodeKey`, and both obey the retirement rule — when the key stops
being eligible the state goes with it.

No for text. A buffer must outlive any snapshot that happens to show it; two
views over one buffer is a supported case with no tree analogue; and recovery
after a guest generation dies is resource-lifetime semantics, not tree
semantics. A commit that removes an editor node removes a *presentation*, and
must not be able to destroy a document.

```text
instar-text                        Scratchpad guest
├── TextBuffer   content, revisions └── canonical Document
├── TextView     caret, selection, scroll
└── TextSystem   the registry, and the only thing that
                 knows a buffer has more than one view
```

`instar-ui → instar-text` is deliberately **absent**, not merely unused. If
editor measurement later makes that awkward, that is evidence for a narrow
edge; it is not a reason to authorize one in advance. A layering test holds it.

Deliberately not a generic `document` service. A name like that invites both
sides to assume they own it, and the whole difficulty here is that neither
wholly does.

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

### Coordinates

> Instar text edits use UTF-8 byte ranges. Consumers requiring additional
> coordinate representations derive them from their own text state.

Byte ranges because that is what the storage edits in and what the protocol
carries. Not because downstream consumers need nothing else: Tree-sitter's
`InputEdit` wants three byte offsets *and* three row-column points, and the
guest owns the document it would derive those from. Saying "byte-positioned
throughout" would promise a conversion-free path that does not exist.

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

`D` merged into `A`. The plan above put the large-document benchmark after
synchronization, and that was the wrong order: `textbench` is what decides
whether the storage layer is worth synchronizing, so it went first.

---

## Package A — closed

> `TextBuffer` can maintain an efficient host-local editing replica without
> whole-document materialization, with revisioned local edits, undo and redo,
> bounded generational resources, and coherent position transformation across
> multiple views.

That is the whole claim, and it is deliberately not "editor architecture
proven". A proved storage and local editing *state*. It proved nothing about
presentation or input: no glyph has been shaped, no key has been pressed, and
`TextView` is still a struct holding a caret rather than a surface anyone can
click.

Evidence in `docs/baselines/PERFORMANCE.md`. The result that matters is a
shape, not the 0.2 µs:

```text
healthy                          faulted (rope -> String -> rope)
1 MiB -> 10 MiB                  1 MiB -> 10 MiB
latency flat                     264 µs -> 8,189 µs, ~31x
whole-buffer materializations 0  1 per operation
journal independent of document  26.4 MiB allocated per edit
```

A flat local-edit cost is what a B-tree rope with chunked storage and
byte-offset editing should produce, so this is consistent with the data
structure rather than evidence of a shortcut. The faulted column is what makes
the healthy one mean something.

---

## Package B — `TextView` becomes a surface

> **The question:** can a real native text interaction reach changed pixels
> with no Wasm participation at all?

This is where `TextView` stops being mostly state. The guest stays entirely out
of it — no synchronization, no acknowledgements, no revisions crossing the
boundary. A synthetic stall is enough, because there is no protocol yet to
stall.

### The invariant

> A `TextView` may not shape the whole document merely because its `TextBuffer`
> is large.

The A invariant said no operation may *materialize* the document. This is its
presentation twin, and it is the one a rope does nothing to protect: shaping is
downstream of storage, and a `TextView` that hands Parley everything between
two newlines has defeated the rope without ever calling `to_string`.

### B1 — viewport-bounded presentation

```text
TextView presentation
├── viewport
├── overscan
├── visible paragraph range
├── shaped visible layouts
├── caret geometry
└── selection geometry
```

Parley 0.11 — the version this workspace pins — exposes `editing::Cursor` with
`from_byte_index`, `from_point`, `geometry`, and visual and logical word
navigation. That is much closer to what a `TextView` needs than reimplementing
editor geometry, and it carries bidi and grapheme knowledge Instar should not
be reproducing.

**The seam that has to be right:** `Cursor::from_byte_index(layout, index, _)`
indexes into *the layout's* text, not the buffer's. The moment a view shapes a
window rather than a document, every Parley position needs translating across
that window's origin, and every buffer position needs translating in. A bug
there is a caret that lands in the wrong place only when scrolled, which is
exactly the kind of defect that survives a unit suite.

### Where shaping lives, and why the forbidden edge stays forbidden

This is the moment the layering note above anticipated — "if editor measurement
later makes that awkward, that is evidence for a narrow edge". The awkwardness
is real: `instar-ui::TextContext` owns the `FontContext` and `LayoutContext`,
which are expensive and must not be duplicated. Two font contexts in one
process would mean loading faces twice and, worse, a `Text` node and a
`TextView` potentially resolving the same family to different faces.

It does **not** justify `instar-ui → instar-text`. The dependency actually
wanted is on the shared font stack, not on the semantic tree, and
`TextContext::shape(text, style)` is already keyless — it is private, not
absent. So:

```text
instar-ui     owns the font stack, exposes a keyless shaping primitive
              and knows nothing about TextBuffer
instar-text   decides which bytes, and translates positions across the
              window origin; no font dependency
instar-host   composes them: takes the window, slices the rope, shapes
```

No new crate, no second `FontContext`, and the layering test keeps holding an
edge that stays genuinely absent. That composition is exactly what
`instar-host` is documented to be for.

### The enormous-line decision, made before B1 rather than after

A rope handles 5 MiB on one line. A naive `TextView` still destroys the
architecture by treating one hard line as one indivisible paragraph and handing
all five megabytes to Parley — and it would look fine on every other fixture.

The policy:

> A paragraph longer than `MAX_SHAPED_PARAGRAPH_BYTES` is shaped in bounded
> segments, and shaping context does not cross a segment boundary.

The cost is stated rather than hidden: a ligature or a bidi run spanning a
segment boundary renders as though the text ended there. That is a real
approximation, and it is accepted only where the alternative is shaping
megabytes to draw eighty columns — ordinary prose and code lines never reach
the threshold and are never segmented.

What B1 must *not* claim is exact horizontal geometry deep inside an enormous
unwrapped line. Knowing the x of byte 3,000,000 means knowing the advance of
everything before it, which is O(bytes before it) however the shaping is
chunked. If that turns out to matter, it is a finding for B1 to report, not a
promise made here.

B1 closes when work tracks the visible region:

```text
                        1 MiB, 2 KiB visible   10 MiB, 2 KiB visible
bytes shaped
glyphs lowered
presentation memory
layout time
```

with the 5 MiB single line producing the same bytes-shaped as the others.

### B2 — pointer selection and caret movement

Pointer positions to text positions through Parley's layout-space mapping,
mutating view state through the existing `TextSystem`. Click moves a caret;
drag changes a selection continuously; caret and selection visuals are
host-local and require no guest event.

Phase 2's doctrine carries forward unchanged: transient editor interaction
naming text positions is host-owned and stays responsive independent of guest
progress.

### B3 — keyboard, with commands and text kept apart

```text
KeyboardInput  ->  navigation and editing commands
                   ArrowLeft, ArrowRight, Home, End, Backspace, Delete

Ime::Commit    ->  inserted text
```

**No US-keyboard character mapper, not even temporarily.** Winit has a
dedicated IME path, and a window must explicitly enable it with
`set_ime_allowed`; while IME is enabled winit may suppress ordinary
`KeyboardInput` during preedit. A physical-key-to-character shortcut would make
the first English typing demo easy and the real architecture harder, and the
project has already learned once that the cheap path becomes load-bearing.

### B4 — IME preedit

IME belongs in B rather than C: it is host-local editing and presentation, and
C is guest synchronization.

```text
TextView
├── selection
├── caret
├── preedit: Option<Preedit>
└── IME candidate geometry     -> Window::set_ime_cursor_area
```

The rule:

```text
preedit          does NOT mutate TextBuffer
Ime::Commit      DOES mutate TextBuffer
```

Composing text is transient presentation; committed text becomes an edit. That
keeps the editing history canonical, and it holds until a platform observation
forces something more complicated — at which point the observation goes in the
design ledger.

### The strongest B test

Mirroring Phase 2's stall tests:

```text
guest stalled
real WindowEvent
   -> TextView reacts
   -> TextBuffer changes when appropriate
   -> caret, selection, preedit update
   -> pixels change
all before any Wasm participation
```

### Evidence, layered

```text
TextSystem unit tests        edit and selection semantics
TextView presentation tests  caret and selection geometry
pixel tests                  caret, selection, preedit actually visible
RuntimeHarness               a real WindowEvent reaches a TextView
manual IME smoke             the platform candidate window behaves
```

The last is manual because winit itself documents IME support and candidate
placement as varying by platform. It is a checklist item like F4, not a claim
CI can make.

### Out of scope for B

Guest revision synchronization, in full. Package C gets acknowledgements,
pending batches, session epoch, conflict handling, recovery, and the 500 ms
stalled guest — and it gets them against a text stack whose presentation and
input are already known to work, so a bad typing experience there is a
synchronization defect rather than an ambiguous one.
