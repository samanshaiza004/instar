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

#### Measured, and left provisional

```text
MAX_SHAPED_PARAGRAPH_BYTES = 64 KiB

correctness   bounded, and does not scale with the line   PASS
performance   6.7 ms to shape the cap                     NOT ACCEPTED
decision      deferred until horizontal scrolling shows
              which indexing model is needed
```

**Bounded and affordable are not the same claim.** The cap does exactly what it
was written to do — the number stops growing with the line — and 6.7 ms is
still a visible hitch.

The number is deliberately not tuned. Changing 64 KiB to 8 KiB would trade one
arbitrary constant for another, because the real answer for a giant unwrapped
line is not a smaller brute-force window:

```text
5 MiB logical line
   -> horizontal presentation index
   -> a bounded chunk around the visible x-range,
      plus bounded shaping context either side
   -> TextLayout
```

Reaching a deep x-position in variable-width text without measuring everything
before it needs retained width information. That is the same shape of problem
as soft wrap, and it deserves the same treatment: do not optimize the constant
before the model is known.

Three related problems are now visible, and they are deliberately **not**
unified yet:

```text
vertical navigation, unwrapped     line index from the rope     cheap already
horizontal navigation, giant line  needs shaped widths          future index
soft wrapping                      rows depend on prior breaks  future index
```

The last two may eventually share one incremental presentation index. Merging
them now would be designing that index from two guesses instead of one
requirement.

B1 closes when work tracks the visible region:

```text
                        1 MiB, 2 KiB visible   10 MiB, 2 KiB visible
bytes shaped
glyphs lowered
presentation memory
layout time
```

and, separately, when the 5 MiB single line stays bounded by
`MAX_SHAPED_PARAGRAPH_BYTES` and does not scale with line or document length.

Not "the same bytes-shaped as the others" — an earlier draft of this section
said that, and the measurement contradicts it by design. One capped paragraph
is ~64 KiB against ~1.5 KiB for a screen of ordinary lines. Requiring them to
match byte for byte would serve nothing: the architectural property is that
the number stops growing, not that it lands on the same value as a different
kind of document.

### B2 — pointer selection and caret movement

Pointer positions to text positions through Parley's layout-space mapping,
mutating view state through the existing `TextSystem`. Click moves a caret;
drag changes a selection continuously; caret and selection visuals are
host-local and require no guest event.

Phase 2's doctrine carries forward unchanged: transient editor interaction
naming text positions is host-owned and stays responsive independent of guest
progress.

The path is already built on both sides; B2 joins them:

```text
pointer position                    absolute position
   -> PresentedSegment                 -> buffer_to_local
   -> TextLayout::cursor_from_point    -> cursor_from_byte_index
   -> segment-local byte + affinity    -> cursor geometry
   -> local_to_buffer                  -> paint caret
   -> absolute position
   -> TextSystem
```

**The rule that keeps it honest:**

> Pointer hit-testing and caret geometry must use the *same* `TextLayout`
> instance that produced the presented glyphs.

Calling `shape_keyless` again to answer a click would give a view two layouts
that can disagree about where text is. Phase 2 has already paid once for two
answers to a geometric question.

The tests worth writing attack the translation seam, not Parley:

```text
click row 0, and click row 90,000
inject buffer_range.start = 0, and prove only the deep case fails
click through multibyte UTF-8
a bidi fixture, where affinity decides the visual position at an
    ambiguous boundary
drag a selection across two presented rows
caret and selection pixel tests
```

The last because Phase 2 proved scene commands can exist while being
invisible.

**The blind spot is one row wide, not "the top of the file".** An earlier
draft of this section said a fixture that never scrolls cannot detect a lost
origin, and the fault injection disproved it: row 12 of an *unscrolled* view
still begins several hundred bytes in, so dropping the origin breaks it too.
Only row 0 starts at byte 0. The control fixture has to be that row
specifically, and B2a has a test occupying exactly it.

And instrument, without necessarily optimizing yet:

```text
pointer -> buffer position
buffer position -> caret geometry
caret-only repaint
visible-text reshapes triggered
```

#### What Parley normalizes, and what Instar may therefore claim

`Cursor::from_byte_index` snaps to a cluster's start, forces `Downstream` at
byte 0 because there is no upstream cluster there, and resolves anything past
the end to the layout end with `Upstream`. So this is **not** an invariant:

```text
(byte, affinity) in  ->  the same (byte, affinity) out
```

What Instar can claim, and what its tests assert:

> Instar preserves affinity across its own coordinate seam. Parley remains
> authoritative for which cursor states are valid inside a shaped layout.

#### The caret is host chrome

Like the focus ring and the scrollbar thumb. Not a semantic node, not wire
vocabulary: a guest describes an editor, not how wide an insertion point is on
this machine. `CARET_WIDTH` is 1.0 *logical* pixel, physicalized once at
lowering, which keeps the Phase 2 DPI boundary intact — `instar-ui` still never
sees display scale.

Glyphs and caret share one clip and one coordinate system. There is no separate
caret coordinate path.

No blinking yet. Blink is timer, focus and lifecycle semantics, and it proves
nothing about coordinate correctness; it belongs after B2c gives the view a
real interaction lifecycle.

#### Selection lives in document coordinates

> A `TextView`'s selection is an absolute anchor and focus in buffer bytes. A
> Parley `Selection` is a temporary projection of it onto one presented
> segment, built to ask for geometry and thrown away.

Parley's `Selection` is a range *within one layout*, and this editor shapes one
layout per row — so storing the view's selection as one breaks the moment a
drag crosses a row, which is the ordinary case rather than an edge case.

Anchor and focus, not a range: dragging backwards is a different gesture from
dragging forwards even when the selected bytes are identical, and `min..max`
loses which end is moving.

Paint order, which is the focus-ring lesson a third time:

```text
background -> selection -> glyphs -> caret
```

#### Capture is logical

> A text drag owns the view it began in until release or cancellation, even
> when the pointer crosses another view.

Not `Window::set_cursor_grab`: winit's grab modes are materially
platform-dependent — `Confined` unsupported on macOS, `Locked` on X11 — so
enforcing capture through the OS would make identical code behave differently
per platform. Host transient state is deterministic everywhere and is what the
scroll subsystem already does.

Cancellation reuses Phase 2's lifecycle rules with no new special case: focus
loss, cursor left, view retired. No outside-window autoscroll — that is an
editor behaviour to add when an application asks.

An edit during a drag **retires** it. The presented segments the drag reads
positions from describe text that has changed, and transforming a live capture
across an edit is a synchronization problem that does not need solving before
keyboard input exists.

#### One selection, and the evidence that decided where it lives

Package A declined to add an affinity type, in this project's usual order: "a
type with one meaningful value is speculative generality — the enum arrives if
a case needs both." B2a is that case, and it arrived with a measurement rather
than an argument. So the deferral ended and `instar-text` gained
[`TextAffinity`] and `TextPosition`:

```text
instar-text::Selection      absolute buffer positions with affinity
                            persistent TextView state, the only authority
        ->  instar-host maps positions into a segment
        ->  instar-ui / Parley Selection, segment-local and temporary
        ->  geometry
```

Two *representations* during rendering, one authority. That is healthy;
Parley's `Selection` is defined as a range within a single layout, so it should
stay ephemeral when a document is many independently shaped segments.

`instar-ui` did **not** grow a dependency on `instar-text` to share the enum.
It keeps its own Parley-facing `Affinity` and `instar-host` converts — six
lines, against a crate edge the layering tests exist to hold absent.

#### Two "which side?" questions, renamed apart

The old name for the edit rule was "the affinity policy", which became
dangerously overloaded the moment real affinity arrived. They answer different
questions and nothing may make one decide the other:

```text
edit stickiness   an insertion happens exactly at a position. Does the
                  position stay before the new text or move after it?
                  Decided by whether this view did the typing, and stored
                  nowhere — it is a property of an edit, not of a position.

visual affinity   a byte offset is visually ambiguous. Which side does the
                  caret draw on? Decided by where the user put it, and
                  carried with the position ever after.
```

Held apart by a test, not only by two names: an edit moves *where* a position
is and must leave its affinity alone. That test was written because the fault
injection for it initially passed — nothing had been checking it.

#### An empty document gets a synthetic insertion row

`crop` reports zero lines for an empty rope and does not count a trailing
newline as another line. Both are the upstream contract and both are now pinned
by tests; neither is corrected in storage to make the editor easier.

Presentation supplies what the editor needs instead:

> An empty document has zero storage lines but one synthetic insertion row.

A line box, not text: nothing invents a newline, the row's range is `0..0`, and
a caret in it sits at byte 0 of a zero-byte document — which is exactly true.
Three transitions are locked by tests: empty presents one row with a visible
caret; typing the first character replaces it with an ordinary row; deleting
the last character brings it back.

The target an arrow key should eventually reach is `reshapes: 0, extractions:
0, layout rebuilds: 0, caret paint only`. B2 does not have to get there; it has
to be able to say whether it did, because that measurement is what shows where
the next cache boundary belongs.

No cross-frame row cache before then. At ~200 µs for a full visible window,
caching would mean inventing invalidation semantics with no evidence about
what invalidates.

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
adapter integration          a real WindowEvent reaches a TextView through
                             the host text-input seam
manual IME smoke             the platform candidate window behaves
```

The fourth line originally said "RuntimeHarness — a real `WindowEvent` reaches
a `TextView`", and implementation disproved its premise. `RuntimeHarness` drives
events through the *bridge* to a guest tree, and no wire vocabulary declares an
editor surface, so nothing a guest commits can produce a text view to reach.

Rather than invent `NODE_TEXT_VIEW` inside a pointer fixture, B2d proves the
strongest claim that exists:

```text
B2   real WindowEvent -> winit adapter -> host text-input seam -> TextView

B2e  real guest tree  -> TextView attachment -> the same host seam
```

That is not a weaker test. It is evidence that corresponds to architecture
that exists, and the seam it exercises is production code B2e will call rather
than test-only editor logic that would be written twice.

The last is manual because winit itself documents IME support and candidate
placement as varying by platform. It is a checklist item like F4, not a claim
CI can make.

### B2e — the attachment contract, frozen

The first genuinely new guest-facing resource design since Phase 2, and the
reason B2d stopped where it did. Frozen here before any of it is built.

#### The decision

> `TextBuffer` and `TextView` are WIT resources. Neither a WIT handle nor a
> `TextViewId` is ever serialized into the UI snapshot. A commit carries a
> side table of **borrowed** `text-view` handles, and a wire node references an
> attachment *slot*.

Three identities, each meaning something different, none collapsed into
another:

```text
NodeKey        semantic and presentation identity; belongs to the retained tree
text-view      a guest capability, valid under Component Model resource rules
TextViewId     host resource identity, generational; belongs to instar-text
```

#### The shape

```wit
interface text {
    resource text-buffer;
    resource text-view;

    create-empty-buffer: func() -> result<text-buffer, text-error>;
    create-view: func(buffer: borrow<text-buffer>)
        -> result<text-view, text-error>;
}

interface ui {
    use text.{text-view};

    commit: func(
        snapshot: list<u8>,
        text-views: list<borrow<text-view>>,
    ) -> result<u64, commit-error>;
}
```

#### Synchronous in WIT, async in Rust

> `ui.commit` **stays a synchronous WIT function.** Its Wasmtime host
> implementation is Rust-async, so the runtime thread can await main-thread
> application without blocking native interaction.

An earlier draft of this section wrote it as `async func`, which would have
been a regression: `commit` is already synchronous WIT today, and declaring a
WIT function `async` opts into Component Model async semantics — overlapping
subtasks, explicit borrow-lifetime machinery, a different ABI. That is close to
the opposite of what a commit wants. Instar's admission is deliberately
single-flight:

```text
guest calls commit
  -> the guest cannot proceed through that call
  -> the Rust host future awaits the main-thread apply
  -> accepted or refused
  -> the guest resumes
```

**The host may execute asynchronously without the guest-visible operation
being concurrently outstanding.** `bindgen!`'s `imports: { default: async |
trappable }` is exactly that distinction, and it is what the text resources
use.

#### `kernel-ui.commit` is `async func` today, and changing it is a migration

Recorded because two earlier statements in this document were wrong about it,
both from reading `wit/world.wit` — the Gate 0 spike, whose `commit` is
synchronous — and attributing it to `wit/kernel.wit`, whose `commit` is not:

```wit
// wit/kernel.wit, today
commit: async func(batch: list<u8>) -> result<commit-result, commit-error>;
```

`runtime.rs` implements it through `HostWithStore` and an `Accessor`, which is
the Component Model async import form and exists *because* the WIT says
`async`. So "keep `commit` synchronous" is a **change to working code that Gate
0 validated**, not a preservation of the status quo, and it is not something
B2e-1 does on the way past.

The rule above therefore binds the *new* surface: every function in
`instar:text` is synchronous WIT, and its host implementation is Rust-async.
Whether `kernel-ui.commit` should migrate to match is a separate decision with
its own evidence — the async import machinery is load-bearing and tested — and
it is deliberately left open rather than folded into a package about text
resources.

and in the packed tree:

```rust
NodeKind::TextView { attachment: u16 }     // not { view_id: TextViewId }
```

**Verified against the pinned toolchain before freezing**, because the whole
design rests on it. `list<borrow<text-view>>` parses under wasm-tools 1.255,
generates host bindings under `wasmtime 47.0.3`, and generates guest bindings
under `wit-bindgen 0.60` as
`ui::commit(snapshot: &[u8], text_views: &[&TextView]) -> u64`.

The full B2e-1 shape was then written and compiled — synchronous WIT resources,
`imports: { default: async | trappable }`, `with:` mapping each resource to a
host lease type, and `async fn` implementations of the generated traits:

```rust
impl Host for State {
    async fn create_empty_buffer(&mut self)
        -> wasmtime::Result<Result<Resource<GuestTextBuffer>, TextError>>
    async fn create_view(&mut self, buffer: Resource<GuestTextBuffer>)
        -> wasmtime::Result<Result<Resource<GuestTextView>, TextError>>
}
```

Two details the spike settled that the documentation did not: the `with` key
separates the resource with a **dot** (`"instar:probe/text.text-buffer"`), not
a slash; and `async` alone omits the `wasmtime::Result` wrapper, so
`trappable` is required for `ResourceTable` operations that can fail.

A contract frozen on a feature the toolchain could not express would have been
a plan, not a decision.

#### Admission

```text
generation screen
  -> resolve borrowed WIT handles to TextViewIds
  -> decode snapshot
  -> validate attachment indices are in range
  -> validate TextViewId liveness and eligibility
  -> validate at most one live attachment per TextView
  -> atomic retained-tree apply
```

The retained host *window state* stores the resolved `TextViewId`, in its
attachment map. The `instar-ui` tree retains only `NodeKind::TextView`, which
carries no resource identity at all — B2e-2 settled that by giving the variant
no fields, so the slot cannot be retained even by accident. Nothing anywhere
stores a `Resource<T>` or a handle-table index: Component Model handles are opaque
table-backed capabilities, not stable application identifiers, and the guest's
table dies with its Store.

What goes in the Wasmtime `ResourceTable` is deliberately tiny — `{ id:
TextBufferId }` and `{ id: TextViewId }`. The rope, journal, selections and
views stay in the main-thread `TextSystem`. The Store does not become their
owner, which is also what makes restart tractable later: dropping a Store
destroys the guest's ability to *name* a document, not the document.

Borrowed rather than owned handles work because `commit` already suspends the
guest until the host has finished applying: every borrow can be resolved
before the request enters retained state, so nothing in the main-thread tree
ever holds a Component Model borrow.

#### Frozen semantics

```text
a TextView is attached to at most one live UI node at a time
a TextBuffer may have many TextViews
removing a TextView node detaches the view; it destroys neither view nor buffer
NodeKey remains the focus identity; the host follows the attachment to reach
    the TextViewId. There is still exactly one FocusState
removing, hiding or disabling the node retires pointer capture and focus by
    the existing NodeKey rules, and does not touch document lifetime
a stale, unknown, duplicated or out-of-range attachment rejects the entire
    commit before any mutation
a guest dropping its handle loses the capability for future commits; an
    already accepted attachment keeps the view alive until that attachment
    goes, so a drop cannot tear presentation out of an accepted snapshot
runtime-generation restart and recovery remain Package C
```

Two views of one buffer means two `TextView`s. One view attached twice would
be one caret, selection, scroll offset and IME state in two places at once.

#### What B2e deliberately does not do

Only `create-empty-buffer`. No `create-buffer(contents)`, because that
immediately raises canonical snapshot transfer, copy limits, bootstrap
revisions, acknowledgement and resynchronization — Package C's problem, pulled
forward. The synthetic empty insertion row already exists, so B3 can type into
an empty document, and C decides later how canonical contents initialize the
replica.

#### The alternative that was rejected

```text
commit(snapshot)
attach_text_view(node_key, view)      // rejected
```

That is a mutation stream running beside the authoritative snapshot, and it
permits a state where the tree says a text node exists while a separate
attachment call says otherwise. With the side table, **a snapshot's structure
and its resource references are accepted atomically as one description** —
which is the property the whole commit model is built on.

#### Hostile cases, because this is a capability boundary

```text
attachment index out of range        -> commit refused
one TextView attached to two nodes   -> commit refused
stale TextView generation            -> commit refused
a handle from another generation     -> commit refused
node removed                         -> view survives, detached
text node replaced by an ordinary one-> attachment released
commit refused mid-validation        -> old tree and old attachments intact
```

#### The proof B2 could not honestly run

```text
real guest: create-empty-buffer, create-view, commit a TextView node
            with a borrowed attachment
  -> RuntimeHarness
  -> real WindowEvent -> winit adapter
  -> FocusState and hit test
  -> resolved TextViewId
  -> the exact handle_pointer seam B2d proved
  -> caret and selection pixels
```

**No B2 logic may be rewritten to make this pass.** That is an acceptance
criterion for B2e itself: attachment supplies a `TextViewId` to the seam that
already exists; it does not create a second editor interaction route.

#### The slot is commit-local indirection, not retained identity

> A `TextView` node's `attachment` is an index into *this commit's* borrowed
> handle table. It is resolved during admission and never retained. What the
> retained tree holds is the resolved `TextViewId`.

This follows from what a `borrow<T>` is: a loan for the duration of one call,
and an opaque capability rather than a stable application identifier. Retaining
the slot number would make both of these wrong:

```text
commit A   slot 0 -> TextViewId(7, 2)
commit B   slot 0 -> TextViewId(19, 0)    the slot held still, the resource did not

commit A   slot 0 -> TextViewId(7, 2)
commit B   slot 4 -> TextViewId(7, 2)     the slot moved, the attachment did not
```

A diff comparing slots would report a change in the second case and miss one in
the first. B2e-3 therefore consumes the slot *before* retained composition and
compares resolved identity.

Which reinforces where things live:

```text
HostWindow
├── instar-ui retained tree      NodeKey -> NodeKind::TextView
└── text attachments             NodeKey -> TextViewId
```

`instar-ui` needs to know a node *is* a text surface — for layout, hit-testing,
focusability, clipping. It does not need to store a `TextViewId`, and still
does not know `instar-text` exists.

#### Changing which view a node shows is not a kind change

```text
NodeKey(50) TextView(V7)  ->  NodeKey(50) TextView(V12)
```

The same semantic surface, showing a different document. Classifying that as
`KindChanged` would delete and recreate the UI node, and take host-local focus
identity with it — which is exactly wrong for switching documents in an editor
pane.

Frozen now: **it is not `KindChanged`.** What it *does* invalidate is left open
until B2e-4 can say what presentation actually depends on the attachment.

#### A `TextView` is a leaf

Children are refused. The inside of the surface is host presentation of an
attached resource, and there must be one answer to who owns it:

```text
TextView
├── a guest Text node?
├── an overlay button?
└── host-owned document glyphs?      <- only this
```

Decoration goes *around* an editor, in a `Stack` or `Column`, not inside it.

#### B2e-2 proves serialization vocabulary and nothing else

```text
protocol 8 refused, 9 accepted
the attachment slot round-trips at its intended width
truncation inside the slot is a ProtocolError, never a panic
a TextView with children is refused semantically
a TextView counts toward MAX_NODES like any other node
same NodeKey: Text -> TextView, Button -> TextView, TextView -> Button
    are all KindChanged
unknown node kinds stay refused
```

Deliberately **not** in B2e-2, because there is no side table yet:
`attachment < table.len()`, `MAX_TEXT_ATTACHMENTS`, `TextViewId` lookup,
duplicate-view validation, attachment ownership, any `TextSystem` access. A
slot of `65535` is structurally valid bytes and semantically unresolved until
B2e-3 supplies the handles.

B2e-3 introduces attachment-reference collection and its consumer together:

```rust
struct TextAttachmentRef { node: NodeKey, slot: u16 }
```

`NodeKey` and a slot. Not a `TextViewId`, not a `Resource<GuestTextView>`.

Deliberately *not* added in B2e-2. Nothing consumed it there, and a vocabulary
term with a producer and no consumer is the shape this project has an
enforcement test against. It also has to be produced by the same pass that
validates the tree, rather than by a later scan that could disagree about which
nodes are semantically live — which is only expressible once the consumer
exists.

#### B2e-3, frozen

> **The question:** can one authoritative commit resolve temporary Component
> Model borrows into stable Instar identities, validate tree and attachment
> state together, and promote both atomically or neither?

Four identities meet here, which is why it is its own package:

```text
borrow<text-view>   ->  Resource<GuestTextView>  ->  OpaqueResourceKey
                    ->  TextViewId               ->  NodeKey -> TextViewId
      transient                                              retained
```

**Nothing that crosses the bridge is a Wasmtime handle.** The runtime resolves
each borrow far enough to copy out a lease and no further, so a commit request
carries exactly `GenerationId`, the batch bytes, and `Vec<OpaqueResourceKey>`.
A `borrow<T>` is non-owning and scoped to the call; letting one reach retained
state would be retaining something defined not to outlive the call.

##### Order, because it decides which refusal a hostile input gets

```text
generation screen           wrong generation      -> refused before decode
opaque key -> TextViewId    stale or unowned lease -> refused before decode
decode                      malformed batch       -> ProtocolError
semantic validation         + collect TextAttachmentRefs
slot resolution             slot >= table.len()   -> semantic refusal
duplicate validation        two nodes -> one view -> semantic refusal
tree diff
attachment diff
```

Keys resolve *before* decoding, so a bad capability never buys parser work —
and fault attribution stays clean, because each class of bad input has exactly
one refusal it can produce.

##### Mutation is impossible until acceptance

```rust
struct StagedUiCommit {
    tree: Tree,
    tree_changes: ChangeSet,
    attachments: BTreeMap<NodeKey, TextViewId>,
    attachment_changes: AttachmentChangeSet,
}
```

Everything is built into this, and only a complete one is promoted. Not this,
even with rollback:

```rust
window.tree = new_tree;
// ... validate attachments ...
window.text_attachments = attachments;
```

There must be no half-promoted state *to* roll back. The fault injection is
promoting the tree before validating attachments, and the refusal test has to
catch the half-applied interface.

##### Duplicates: the side table is scratch, the map is law

```text
text_views = [V7, V7]                       allowed
text_views with unreferenced entries        allowed
node 10 -> slot 0, both entries are V7      allowed
node 10 -> slot 0, node 20 -> slot 1        REFUSED: both resolve to V7
```

The illegal condition is two live `NodeKey`s attaching one `TextViewId`, not a
temporary list containing a capability twice. Policing the list would turn a
WIT argument representation into semantics for no correctness gain. Resolve
first; validate the staged map.

##### Three slot tests, not two

```text
A: slot 0 -> V7    B: slot 9 -> V7          attachment diff is a no-op
A: slot 0 -> V7    B: slot 0 -> V12         attachment changed

A: node 10 -> slot 0 -> V7, node 20 -> slot 1 -> V8
B: node 10 -> slot 1 -> V7, node 20 -> slot 0 -> V8
   tree no-op, attachment no-op
```

The third is the one that proves the *whole table* is positional scratch space,
rather than that one slot number happens to be allowed to move.

##### The bound, stated precisely

```text
MAX_TEXT_ATTACHMENTS <= MAX_NODES = 4096
```

A leaf text view consumes at most one slot, so there can never be more useful
attachments than nodes. What the claim is:

> Instar refuses `text_views.len() > MAX_TEXT_ATTACHMENTS` on entry and bounds
> all work after lifting.

What it is **not**: a pre-lift allocation bound. Generated `bindgen!` lifts a
`list<T>` into an owned Rust collection before the host implementation sees it,
so the lifting itself is not an Instar-controlled boundary. That belongs in the
code comment, not only here, because it is exactly the kind of claim that
drifts into an overclaim.

##### The joined-seam test is not optional

Twice in this phase a unit-level proof made me assume an integration-level one,
and twice the fault injection found it. So, as a phase rule:

> Every new cross-thread or cross-crate authority check needs one test at the
> final joined seam, even when both halves have exhaustive unit tests.

B2e-3's is a real guest committing with a borrowed capability, through the
async commit, the runtime's key extraction, the bridge, lease resolution, slot
resolution, and atomic promotion — with a live cross-layer authority fault
injected there. Making `resolve_view_lease` trust a live id without checking
ownership must refuse the whole guest commit.

##### Implementation order

```text
first  the staged commit type, and the attachment change set
then   WIT, borrow resolution, bridge payload, validation refs,
       slot resolution, host diff
last   atomic promotion, fault injection, the vertical guest test
```

##### B2e-3 ends inert

The retained attachment map is correct and does nothing. It does not keep views
alive, does not participate in focus or hit-testing, and does not feed the B2d
seam. Those are B2e-4.

```text
B2e-3   authority, and atomic description
B2e-4   lifetime, and behaviour
```

Keeping them apart is what makes each provable.

#### B2e-3, as built: where the freeze was wrong

Recorded because the freeze was written before the code and four of its
statements did not survive contact with it.

**The order above starts too late.** It began at the generation screen and said
nothing about the single-flight gate or about where borrow extraction sits
relative to either. Extracting up to `MAX_TEXT_ATTACHMENTS` handles before
those gates would let a superseded or overlapping guest buy that work — the
containment invariant, broken by the feature meant to sit behind it. The real
order is:

```text
canonical ABI lift                       unavoidable
generation preflight                     -> stale-generation
single-flight acquisition                -> commit-in-progress
attachment-count bound                   -> too-many-attachments
handle -> opaque key                     O(n), gated by everything above
generation recheck                       the race after preflight
cross-thread submit
main-thread generation screen            checked again, deliberately
capability resolution                    -> unavailable-text-view
decode + semantic validation             -> invalid-batch
```

`CommitPermit` exists to hold that: `commit_batch` cannot be called without
one, so a later edit cannot reintroduce the old shape by writing a fresh call
that skips the gates.

**`ledger.validate` belongs inside staging.** The freeze's order omitted it,
which made `StagedUiCommit` a weaker statement than its name: it was the second
fallible pre-mutation check, sitting beside the tree diff, and a staged commit
that could still fail it would not be staged. `ledger.apply` stays
mutation-side.

**The no-op gate had to widen.** It returned early on an empty *tree* diff, so
the frozen `V7 -> V12` case — same node, same bytes, different document — was
accepted and never promoted. The host reporting success for a change it
discarded is worse than refusing it. The gate now consults both diffs.

**The bound is a policy, not a theorem.** `MAX_TEXT_ATTACHMENTS = MAX_NODES`
was justified as the structural maximum implied by a valid snapshot. It is not:
unreferenced side-table entries are legal, so a one-node tree supplying 4097
entries is meaningful under Instar's own semantics and is refused by this rule
alone. The value stands; it is the maximum *useful* cardinality, and the
reasoning is now written that way.

**One joined-seam fault was unreachable.** B2e-3's proof was to be a real guest
committing a capability it did not own. No real guest can express that: a new
generation gets a fresh `Store` and `ResourceTable` and cannot hold its
predecessor's handle, and the old generation's own commit dies at
`stale-generation` long before the resolver. Manufacturing it would have meant
test-only machinery for smuggling a foreign key into a new Store, which proves
the machinery. The authority check is tested one seam lower, where an
`OpaqueResourceKey` can be constructed legitimately; the real guest proves
transport, permutation and promotion.

#### The rule B2e-3 earned: the mutant comes first

> **Every new lifetime or authority invariant gets its mutant written first.
> The test is accepted only after the mutant has been observed failing.**

Not adopted ceremonially. B2e-3 shipped four tests that could not fail, and
every one of them was green and confident:

```text
the permutation regression   rebuilt the slot -> view map inside its own
                             helper, so the one test written to catch "code
                             comparing slot numbers" never called that code
a diff-ignores-the-table     had no table in it: two hand-built maps, an
  test                       assertion true of any implementation
the unowned-key test         named a resource that never existed, so identity
                             refused it and authority was never consulted
the joined-seam test         waited on a condition that was already true, and
                             so read the retained map before the commit under
                             test had been applied
```

The last two were written while explicitly trying not to make this mistake.
That is the argument for the rule: green is not evidence, and intent is not a
substitute for the mutant.

The stronger form, which is what the rule is actually for:

> A passing test is evidence only after it has been shown to turn red under
> the specific wrong implementation it claims to exclude.

##### B2e-4's mutants, which its frozen law already names

The lifetime law is two `OR`s, stated in
`instar-host/src/text_host.rs::collect_unowned_resources`:

```text
a TextView   lives while a guest lease names it OR a retained attachment does
a TextBuffer lives while a guest lease names it OR a live TextView does
```

Both halves of both `OR`s are a mutant, and each has one obligation:

```text
view lifetime -> guest lease only      dropping the guest handle kills an
                                       attached view
view lifetime -> attachment only       detaching kills a view the guest still
                                       holds
detach forgets to decrement            the live-view counter never returns to
                                       baseline
replacement releases before acquiring  an observable intermediate destruction
                                       between V7 leaving and V12 arriving
teardown kills an attached view        a generation's death destroys a document
  because its lease went               -- the thing B2e exists to disprove
buffer rule broken                     the last guest buffer lease destroys a
                                       buffer a live view still references
final release leaks                    the view goes, the buffer does not
```

The counters already exist for this: `TextResourceCounts` asserts a return to
baseline rather than that some `drop` ran, which is what makes the third and
seventh expressible at all.

Then the behavioural mutants, which are about the B2d seam rather than lifetime:

```text
NodeKey -> TextViewId lookup skipped
attachment replacement treated as a kind change
detachment leaves text capture or the focus route alive
a pointer event routes through a stale previous TextViewId
```

The second is already half-frozen: `NodeKey` remains the focus identity and the
attachment is only how the host reaches the view, so `V7 -> V12` must not
recreate the semantic node. B2e-3 locked the control that it is not a kind
change; B2e-4 owes the mutant that proves nothing downstream treats it as one.

##### `cargo-mutants` as a supplement, and only that

Worth adding, scoped to the changed B2e modules rather than the workspace —
mutation runs get expensive fast, and ~660 tests is already a slow full pass.
(Not installed here yet; it is a `cargo install cargo-mutants` away.)

What it is good for is the boring holes: an unmutated branch, a comparison that
could be inverted with nobody noticing. What it will not invent is
"resolve authority *after* decode", or "retain slot identity instead of
resource identity", or "wait on a condition that was already true" — none of
which are local code mutations. Those are the architectural invariants, they
are where this package's real defects were, and they stay hand-written.

The tool finds the holes around the deliberate faults. It does not replace
them, and a green `cargo-mutants` run is not the rule above being satisfied.

#### Order

```text
B2e-0  freeze this contract                            <- done
B2e-1  the first WIT resources, and their leases.
       No change to ui.commit, no protocol bump, no attachment path,
       no change to the B2d seam
B2e-2  protocol v9: TextView node and attachment slot
B2e-3  the commit side table, resolved before decode and apply
B2e-4  attachment lifetime and retirement rules
B2e-5  the RuntimeHarness vertical proof above
```

Each step gets one responsibility. Combining resources, a protocol bump, a
borrowed side table and a new concurrency mode into one jump is how a package
stops being reviewable.

#### The sink, frozen

`instar-kernel` **does** know that the world imports `text-buffer` and
`text-view` — generated bindings force that, and pretending otherwise would be
a fiction. What it must not know is what a buffer contains, how a view
references one, what a selection is, or when a document should die.

The precedent is `CommitSink`, which exists so the kernel can suspend a guest
call while the thread that owns the retained tree does the actual work:

```rust
/// A host resource, named without saying what kind of thing it is.
pub struct OpaqueResourceKey {
    slot: u32,
    incarnation: u32,
}

/// Distinct Rust types, so a buffer cannot be accepted where a view belongs.
pub struct GuestTextBuffer { key: OpaqueResourceKey }
pub struct GuestTextView   { key: OpaqueResourceKey }

pub trait TextSink: Send + Sync + 'static {
    fn submit(&self, request: TextRequest) -> Result<(), TextRequest>;
}
```

**`incarnation`, not `generation`.** There are already two unrelated
generations in this system and naming both the same in bridge code is a defect
waiting to happen:

```text
GenerationId          one Wasmtime Store, component instance and guest task
TextViewId.generation ABA protection for one host resource slot
```

`instar-host` is the only layer that translates `OpaqueResourceKey` to and from
`TextBufferId`/`TextViewId`. There is no `instar-kernel -> instar-text` edge.

```text
guest create-empty-buffer()
  -> generated host binding
  -> TextRequest { GenerationId }
  -> the existing bounded runtime/main bridge
  -> generation screen
  -> TextSystem allocates a TextBufferId
  -> encoded as OpaqueResourceKey
  -> reply
  -> ResourceTable.push(GuestTextBuffer { key })
  -> guest receives own<text-buffer>
```

#### Three things that are load-bearing rather than incidental

**No Store-backed borrow survives the main-thread await.** Not a borrowed
`ResourceTable` entry, not a `Resource<T>`-derived reference, not anything else
the Store owns. For `create-view`: take the borrowed lease, copy its two
`u32`s, drop the borrow, *then* marshal.

Stated that way rather than as "no Store access across the await", which was
shorthand borrowed from the true-async `commit` path and is not what this shape
does. Synchronous WIT with `imports: { default: async | trappable }` generates
async Rust methods while the call stays blocking from WebAssembly's point of
view; `store` is a separate flag, and true WIT `async` functions are what get
`Accessor` semantics. B2e-1 does not need `HostWithStore` or `Accessor` at all
— the ordinary async imported-resource shape with a `ResourceTable` is enough,
and it is what the spike compiled.

**A failed table push must compensate.** The host resource already exists by
the time `ResourceTable::push` runs:

```text
main thread creates the buffer -> reply -> push fails
  -> release the lease that was just created
  -> then return the error
```

Otherwise the failure meant to refuse a resource is what leaks one.

**Store destruction is not the cleanup mechanism.** Wasmtime can run a
destructor when a guest drops an owned resource, but host resource lifetime
stays the embedder's problem — and Instar destroys whole Stores on trap and
restart, where no guest destructor runs at all. So the lease registry is
generation-aware, and teardown hangs off the terminal path that already
exists:

```text
HostEffect::GuestGone { generation, error }   error: None means a clean exit
  -> Host::on_guest_gone
  -> release every remaining lease for that generation
```

> Dropping a Store removes Component Model handles. Instar's generation
> teardown removes the corresponding host capability leases.

Cross-thread cleanup does **not** go in `Drop for GuestTextView`. A destructor
doing hidden thread-affine work is the lifecycle coupling this project has
spent two phases removing.

#### No fallback sink

`CommitSink` keeps batches when nobody is installed, because a headless commit
log is a meaningful thing. A fake text system is not:

```text
no TextSink installed  ->  create-empty-buffer returns host-unavailable
```

Installed once, before any generation runs, exactly as `install_commit_sink` is.

#### `kernel-ui.commit` stays true WIT async

```text
kernel-ui.commit async-WIT migration
status: OPEN / no evidence requiring change
```

It is `async func` today and implemented through `HostWithStore` and
`Accessor`, which is how the guest suspends while the thread-affine retained-
tree owner applies the commit. That mechanism is Gate 0-validated, already has
an explicit single-flight gate bounding its concurrency, and has caused no
measured problem. Migrating it during B2e would combine a runtime ABI
experiment with the first resource implementation for no demonstrated gain.

#### B2e-1's question, and its lease lifetimes

> Can a guest acquire and release typed capabilities naming main-thread
> `TextSystem` resources, without Wasmtime owning those resources?

```text
Wasmtime Store / runtime thread     main thread
  ResourceTable                       TextSystem
    GuestTextBuffer { id }    --->      TextBuffer   rope, revision, journal
    GuestTextView   { id }    --->      TextView     caret, selection, scroll
```

Drop semantics, frozen for the unattached case B2e-1 covers:

```text
drop a TextView handle              -> lease removed; the internal view may go
drop a TextBuffer handle while a
  TextView still refers to it       -> the buffer stays
```

B2e-4 adds the second owner, and the accounting it implies:

```text
TextView lives while    a guest lease OR a retained UI attachment exists
TextBuffer lives while  a guest lease OR a live TextView exists
```

Not generalized into a resource framework. Two resources, and whatever falls
out of them.

What B2e-1 must prove, before any tree is involved:

```text
create-empty-buffer            -> a real bounded TextBuffer slot
create-view(buffer)            -> a view referring to that exact buffer
explicit drop of a view        -> the slot returns; reuse advances the
                                  internal incarnation
drop a buffer lease while a
  view still references it     -> the buffer survives
a stale opaque key             -> refused by TextSystem, never misapplied
MAX_TEXT_BUFFERS exceeded      -> an explicit error, not a panic
MAX_TEXT_VIEWS exceeded        -> an explicit error
ResourceTable push fails       -> the host lease is released, not leaked
generation trap or exit        -> every lease returns to baseline
no sink, or a disconnected one -> an error, never a parked guest
```

The last matters as much as the rest: a capability boundary that can hang is
worse than one that refuses.

And the race, which is worth more than ordinary teardown because it is what
proves the registry actually closes the cross-thread window:

```text
create-view for generation 17 is in flight
GuestGone(17) arrives

allowed    GuestGone wins   -> the request is refused, nothing is allocated
allowed    creation wins    -> the view exists, its lease is registered, and
                              GuestGone then releases it
forbidden  an orphaned TextView with no owner at all
```

#### Typed refusals are not traps

`trappable` grants the *ability* to trap; it does not make every refusal one.

```text
text-error   resource limits, a stale lease, no text service available
trap         a corrupted ResourceTable, an impossible internal invariant
```

The first set are ordinary Instar refusals a guest should be able to handle.
The second are bugs.

#### `ResourceTable` holds handles, `TextSystem` holds relationships

No parent/child ownership tricks in the table for buffer-to-view. That relation
lives in `TextSystem`, or Component Model handle lifetime quietly becomes
Instar document lifetime — which is the whole thing this package exists to keep
apart.

The generational allocator matters here: the Component Model gives handle
safety at the ABI, but once a handle resolves to a `TextViewId`, it is Instar's
own generation that stops a stale id reaching the resource that replaced it.

Four counters, so teardown tests can assert a return to baseline rather than
that a `drop` ran:

```text
live guest buffer leases    live TextBuffers
live guest view leases      live TextViews
```

#### Recorded, not started

AccessKit has `TextInput` and `MultilineTextInput` roles, so a `TextView`'s
accessibility projection will hang off the semantic `NodeKey` and its
attachment. That the role exists is not evidence that text accessibility is
solved: selection, value and text-range semantics need B3 and B4 to have
established what editing actually does first.

There is a pattern here worth noticing and **not** generalizing yet: a semantic
snapshot may reference a specialized host resource through a typed capability
attachment without that resource becoming part of the tree's lifetime. Images,
2D scene surfaces and GPU resources may eventually want the same shape.
`TextView` is the first proof, and one proof is not a framework.

### Out of scope for B

Guest revision synchronization, in full. Package C gets acknowledgements,
pending batches, session epoch, conflict handling, recovery, and the 500 ms
stalled guest — and it gets them against a text stack whose presentation and
input are already known to work, so a bad typing experience there is a
synchronization defect rather than an ambiguous one.

### Package B completion (B2e-4 through B4)

Package B is host-local and deliberately does not add a guest edit protocol or
an application/package format. TextView presentation is strictly unwrapped:
one hard document line is one row, with the shared Parley-derived
`FontSizeRelative(1.4)` row height used for shaping, viewport arithmetic,
scrolling, and candidate geometry. Crop owns grapheme and CRLF storage facts;
Parley remains behind `instar-ui::TextLayout` for visual cursor semantics.

Retained attachments are counted owners. Admission enforces global
`TextViewId` uniqueness across windows, replacement retains the new view before
releasing the old one, and generation teardown removes only guest leases.
Pointer, focus, wheel chaining, and command editing are host-local and never
send guest events. IME state separates transient preedit from its saved target;
the empty-preedit-before-commit sequence is preserved, invalid cursor metadata
only hides the composition cursor, and commit without a preedit replaces the
current selection. Native configuration is deduplicated and disabled across
the metrics barrier.

Completion evidence: `cargo test -p instar-text`, `cargo test -p instar-ui`,
`cargo test -p instar-window`, `cargo test -p instar-host --lib`, and
`cargo check -p instar-shell` pass in the workspace. The production harness
observes retained attachments, resource counts, selection/revision, and scene
state without adding a routing or promotion test path.

---

## Package C — revision and edit synchronization, frozen

> **The question:** can host-local edits stay immediate while guest and host
> converge deterministically under delay, conflict, desynchronization, and
> generation teardown?

Not *restart*. C cannot prove same-document restart recovery, and the reason is
recorded below rather than glossed.

Frozen before implementation, as B was. Nothing from D–H starts alongside it:
this is where the two-authority text model proves itself or breaks.

### `next-edit` is `async func`. The other four are not.

The WIT already encodes the rule, and C must not break it:

```text
async func    next-event, commit, await-op    suspend on an external event
func          start, cancel, create-*         bounded; the guest blocks inside
```

Marking a plain import async in `bindgen!` makes only the *host implementation*
async — to WebAssembly the call is still blocking, so the guest task is parked
and can do nothing else meanwhile. A guest must be able to await an edit and a
UI event concurrently, which makes this the first `instar:text` function that
has to be `async func` in the WIT itself.

```wit
next-edit: async func(buffer: borrow<text-buffer>)
    -> result<edit-notification, text-error>;

create-buffer:  func(contents: string)            -> result<text-buffer, text-error>;
apply-edits:    func(buffer: borrow<text-buffer>,
                     expected-revision: u64,
                     edits: list<text-edit>)      -> result<apply-edits-result, text-error>;
read-range:     func(buffer: borrow<text-buffer>,
                     start: u64, end: u64)        -> result<range-contents, text-error>;
resynchronize:  func(buffer: borrow<text-buffer>) -> result<snapshot, text-error>;

variant edit-notification {
    edits(list<applied-edit>),
    desynchronized(u64),        // latest revision
}

variant apply-edits-result {
    applied(u64),               // new revision
    conflict(u64),              // current revision
}
```

`conflict` is an outcome, not a `text-error`. A guest that loses a race is
told, and being told is ordinary.

### `invalid-edit` is one refusal, not three

`instar-text`'s own `TextError` distinguishes `InvertedRange`,
`RangeOutOfBounds`, `NotACharBoundary`, `NotAGraphemeBoundary`, and
`LineOutOfBounds` — the storage layer's own taxonomy for a byte range it
cannot use. None of that crosses into `text-error`. Adding a WIT variant case
is itself a breaking interface change, so the public shape stops at what a
guest can act on — "this batch was malformed" — not at which storage-layer
check caught it:

```wit
variant text-error {
    too-many-buffers(u32),
    too-many-views(u32),
    no-such-resource,
    unavailable,
    already-waiting,
    edit-batch-too-large,
    invalid-edit,
    buffer-too-large,
}
```

`apply-edits` enforces these, and everything else `apply-edits` can refuse, in
one fixed order — frozen so fault attribution stays unambiguous the same way
every other "one refusal per class of bad input" rule in this document does:

```text
1  generation screen           stale-generation
2  capability/lease resolve    no-such-resource
3  inbound count/byte bounds   edit-batch-too-large, before anything clones
4  expected-revision           conflict(current), before any edit is attempted
5  sequential clone validation invalid-edit
6  resulting document ceiling  buffer-too-large
7  swap the clone in
8  publish to every relationship but the source
```

Step 4 before step 5 is load-bearing, not incidental: judging a byte range
against the buffer's current state when the batch was authored against a
revision that no longer exists is meaningless, so a stale revision paired with
a malformed range is `conflict`, never `invalid-edit`.

### The sync state machine

Per `(GenerationId, TextBufferId)` — the keying `GenerationLeases` already
uses, because a generation's death must take its synchronization state and
nothing else.

```text
Synchronized   { queue: VecDeque<AppliedEdit>, queued_bytes }
Desynchronized { latest_revision }
```

```text
host-local edit, synchronized     -> admit, or collapse
host-local edit, desynchronized   -> enqueue nothing; move latest_revision only
next-edit, queue non-empty        -> drain a bounded batch
next-edit, queue empty, synced    -> suspend
next-edit, desynchronized         -> Desynchronized { latest_revision }
```

**Collapse immediately; never coalesce.** A spanning edit covering `min..max`
of a backlog is frequently most of the document anyway, and it destroys the
exact granularity Tree-sitter and every other incremental consumer exists to
use. One incremental path plus one snapshot recovery path is fewer algorithms
than an incremental path, a synthetic-edit path, and a recovery path.

While desynchronized the host holds one revision number, whatever the guest is
doing. That is what makes an arbitrarily stalled guest survivable.

#### The bound is checked before the allocation

Normative, because "push, then check" allocates precisely the memory the bound
exists to prevent:

```text
prospective_count = queue.len() + 1
prospective_bytes = queued_bytes.checked_add(edit.replacement.len())

exceeds either  ->  clear the queue, become Desynchronized, and never clone
                    or push the replacement
otherwise       ->  push
```

#### Both bounds, because one is not a bound

```text
MAX_PENDING_EDITS        entries
MAX_PENDING_EDIT_BYTES   summed replacement length
```

A count-only ceiling is defeated by one paste or one IME commit; a byte-only
ceiling is defeated by ten thousand keystrokes. Neither is the bound; the pair
is. The same pair, inbound, bounds an `apply-edits` batch — a million empty
replacements defeats a byte-only limit from the other direction.

#### Consuming the marker does not clear it

The rule that closes the race, and the one most likely to be got wrong:

```text
guest receives Desynchronized { 42 }   -> still desynchronized
guest resynchronizes                   -> host atomically snapshots at R
                                          and re-arms the queue at R
edit R -> R+1                          -> queued normally
```

If draining the marker re-armed the queue, every edit between the drain and the
guest's read would be lost in silence, and the guest would believe itself
synchronized at a revision it had never read. **Recovery is a property of the
read, not of the delivery.**

#### `read-range` is strictly observational

It never re-arms, and it is not the recovery mechanism. The hazard is invisible
until it corrupts a document:

```text
guest canonical @ 5
host edits 5 -> 6 -> 7          both queued
guest apply-edits(expected=5)   -> conflict(7)
guest read-range(...)           -> bytes @ 7
queue still holds 5->6, 6->7
```

A guest that adopted revision 7 from that read and then drained the queue would
apply both edits a second time. So after `conflict` a guest either drains
`next-edit` until it has caught up, **or** calls `resynchronize`.

`resynchronize` is therefore legal while synchronized too, and does the same
three things in both states:

```text
snapshot the rope at R
discard pending notifications through R
establish R as the new sync base
```

One snapshot mechanism serves queue overflow and "I would rather not replay
this backlog" alike. No acknowledgement protocol is needed, and the recovery
path is exercised routinely rather than only after a fault.

##### C4b close-out: `&self` makes the hazard uncompilable, not merely untested

`TextHost::read_range` takes `&self`, not `&mut self` -- there is no `self.sync`
mutation available to write inside it at all. Confirmed empirically, the same
discipline as every other mutant in this document: a fault that added
`self.sync.get_mut(...).poll(...)` to the method body without widening its
signature was attempted and rejected by the compiler
(`error[E0596]: cannot borrow self.sync as mutable`), not caught by a test that
could later regress. The runtime test
(`text_host::tests::read_range_never_touches_synchronization_state`) stays
alongside it because it also proves the *read* is correct, not only that
nothing else moved -- but the double-application hazard above cannot be
introduced by editing this function's body, only by someone deliberately
widening its signature, which is a change anyone reading the diff would see
coming.

#### Bootstrap establishes a baseline, not an edit

> `create-buffer(contents)` establishes a baseline revision. It generates no
> edit notification and no journal entry.

The guest supplied the bytes; reporting them back is the host telling the guest
its own text as news, and a journal entry would make the document's first state
undoable into an empty buffer it never had. Born `Synchronized` with an empty
queue; `create-empty-buffer` is the same rule at `Revision(0)`.

#### Guest edits are not echoed to their source generation

Echoing would oblige every guest to filter its own edits back out to avoid
double-applying — a correctness burden on every guest, forever, to save one bit
of origin in the host. A buffer is reachable by exactly one generation, so
origin is binary and cheap.

The batch is atomic: `conflict` applies nothing, exactly as a refused UI commit
promotes nothing.

#### One outstanding `next-edit` per (generation, buffer)

A second is a deterministic refusal, not two futures racing one queue. The
mechanism exists already: `begin_commit`'s semaphore and `CommitPermit` are the
same shape, and `commit-in-progress` is the same refusal.

Teardown drops the waiter without leaking sync state, and B2e-3a's rule binds
unchanged — **resolve the borrow to a stable `TextBufferId` before
suspending.** Nothing table-derived may be retained across the await.

### Transfer, and the ceiling it forces

A bounded single-shot resync implies a bounded document, or C has a state it
cannot leave:

```text
create a 10 MiB document, edit it up to 20 MiB
queue overflows      -> Desynchronized
resynchronize()      -> cannot return 20 MiB.  Stuck, permanently.
```

So, frozen together:

```text
MAX_TEXT_BUFFER_BYTES <= MAX_TEXT_TRANSFER_BYTES
```

`MAX_TEXT_BUFFER_BYTES` is enforced on **every** mutation — `apply-edits` and
host-local edits alike — not only at bootstrap. 16 or 32 MiB is ample against
the 1–10 MiB target `textbench` already exercises. Later evidence can replace
the ceiling with chunked transfer; until then the ceiling is what makes
recovery total.

#### The snapshot is cloned on the text thread and materialized off it

Not "one copy": a rope clone, then a materialization, then a Canonical ABI
transfer.

```text
text-owner thread   clone the Rope (O(1), structurally shared)
                    capture revision R, re-arm sync state at R
                    reply with the snapshot
runtime thread      materialize UTF-8, lower into the guest
```

Holding the presentation thread while building a 10 MiB `String` would stall
the window in order to serve a recovery — the exact failure the architecture
claims not to have. `crop::Rope` and `TextStorage` are both `Send`, verified by
compile-time assertion before this was frozen, so the split is available.

Deferred chunking stays cheap for the same reason: a future chunked resync can
pin a snapshot at R and serve ranges while the live rope moves on.

##### C4c close-out: one synchronous call, not yet a two-thread split

`TextHost::resynchronize` materializes UTF-8 synchronously, in the same call
that captures the revision and re-arms the relationship -- it does not yet
split the clone from the materialization across two threads. This is a
deliberate simplification, not a rejection of the design above: `TextHost`
today is served entirely on the same thread as UI commit processing (via
`MainThreadSink`), so there is no second "runtime thread" yet for a
materialize step to be deferred *to*. Splitting the two remains available
without a protocol change whenever a genuinely separate text-owner thread
exists to make it worth the latency win -- the revision-then-materialize
ordering this method already uses is exactly what that split would keep.

What C4c does guarantee now, independent of threading: the revision reported
in `snapshot` and the revision the relationship is re-armed at are the same
value, because they are read once and used twice within one `&mut self`
call. The priority mutant for this package made a real, buildable fault out
of that claim -- re-arming one revision ahead of what is reported --
confirmed red in `text_host::tests::resynchronize_re_arms_at_exactly_the_revision_it_reports`,
then reverted. The literal "unlock; edit; re-arm at the new current
revision" race the mutant is named for is not otherwise constructible in the
present single-threaded implementation: nothing can run between the read and
the re-arm without a second `&mut TextHost` borrow, which the type system
already refuses. The mutant proves the code that *would* have been wrong is
recognizably wrong, ahead of whatever architecture eventually makes the race
representable.

#### Hostcall fuel is containment, not the API limit

B2e-3's close-out recorded that `MAX_TEXT_ATTACHMENTS` bounds Instar's work
*after* Canonical ABI lifting and cannot bound the lift itself, and that
`runtime.rs` installs the limiter and epoch deadline and nothing else. C is
where that audit is owed, because `create-buffer(contents)` and `apply-edits`
are the first calls handing the host megabytes.

`Store::set_hostcall_fuel` bounds host allocation while lifting guest→host, is
reset per host call, corresponds roughly to transferred bytes, and defaults to
128 MiB. It does **not** constrain host→guest — so it protects bootstrap and
`apply-edits`, while `MAX_TEXT_BUFFER_BYTES` is what protects `resynchronize`.
The two interlock; neither replaces the other.

```text
canonical ABI fuel   containment against absurd pre-lift input
Instar limits        precise API policy, and a typed refusal
```

#### C4a close-out: the chosen fuel value, and what a real guest showed

`TEXT_TRANSFER_HOSTCALL_FUEL = 40 MiB`, in `instar-kernel/src/resource.rs`,
installed once per generation alongside the epoch deadline. Comfortably above
`MAX_TEXT_BUFFER_BYTES` (32 MiB, with headroom asserted in
`crates/instar-host/tests/text_transfer_bound.rs`) and well under Wasmtime's
own 128 MiB default, so the window between the two ceilings is wide enough to
exercise "liftable but too large for Instar" as its own observable outcome.

Confirmed against a real guest, not assumed: exceeding the fuel budget traps
the generation with Wasmtime's own message, `"fuel allocated for hostcalls
has been exhausted"`, distinct from a memory-policy trap ("forcing trap when
growing memory") and from an ordinary guest panic (a bare Wasmtime
backtrace). The trap surfaces through the same `GuestTrapped` channel a
guest panic does — there is no `text-error` for it, because the host
implementation of `create-buffer` is never entered.

One test-design finding worth keeping: Wasm linear memory only grows. A
single guest generation that allocates a legal-sized payload and then an
oversized one back to back can trip its *own* 64 MiB memory policy on the
second allocation before ever reaching the call under test — the memory
never shrinks between allocations, so the two peaks add. Each of C4a's
three size-tier tests therefore runs its own fresh generation.

#### C4d close-out: the convergence claim, proven rather than assumed

Every earlier joined-seam test in Package C proved one seam in isolation.
None of them puts a real guest through the property the whole package exists
for: that host and guest *converge*. `a_real_guest_converges_with_the_host_after_a_forced_desync`
is that test —  a real guest bootstraps a non-empty document, an ordinary
real host-local edit lands, a second real host-local edit deliberately
exceeds `MAX_PENDING_EDIT_BYTES` in one push to force `Desynchronized`
cheaply and deterministically (no need for thousands of real round trips to
exhaust the count bound instead), the guest calls `resynchronize`, and its
reported length and revision are checked against the host's own buffer
state — not trusted.

Two findings from actually running it, kept here so they don't have to be
rediscovered:

- **Clicking a button steals focus from the text view.** The same lesson
  C2c's own test already learned for its "Await edit" button applies to
  "Resynchronize": IME has to be refocused onto `TEXT_NODE_A` before the
  edit that verifies recovery held has anywhere host-local to route, or it
  silently no-ops and the assertion fails for the wrong reason.
- **Document length and revision converging is not the same claim as the
  relationship's own baseline converging.** The first version of this test
  checked the guest's reported `(len, revision)` against
  `TextBuffer::len_bytes`/`revision` — both untouched by the C4c priority
  mutant, since that mutant only corrupts `SyncState::latest_revision`, a
  separate value. The test passed under the mutant. Adding an explicit
  `sync_state(...).latest_revision()` assertion — independent of what the
  guest was told — closed the gap; re-running the same mutant against the
  fixed test now fails it, confirming the closure test exercises the
  priority mutant's exact claim, not just a claim that happens to look
  similar.

### Generation teardown, and what C does not answer

A new generation cannot inherit handles: leases die with the generation, and
there is no API to acquire a buffer that already exists. A restarted guest does
not resume a session — it creates a buffer and bootstraps it.

```text
sync state is per (generation, buffer), and dies with the generation
any buffer a new generation can name starts Synchronized at its baseline
a dead generation's apply-edits is refused by the existing generation screen
```

What C does **not** add is re-acquisition of a buffer that outlived its
generation. B2e-4 made retained attachments counted owners, so an attached view
can keep a document alive with no guest able to name it. That is
resource-acquisition rather than synchronization, and inventing an API for it
inside the package about convergence would repeat the mistake B2e-0 avoided
when it refused to design synchronization inside a resource constructor.

Named here so it is a known hole rather than a discovered one — and it is why
the question above says *teardown* and not *restart*.

### Order, and the mutants each package owes

`a0866d8`'s rule binds: every lifetime or authority invariant gets its mutant
written first, and the test is accepted only after the mutant has been observed
failing.

```text
C1  the sync state machine       host-only Rust, no WIT
C2  next-edit                    async func, kernel bridge, delivery
C3  apply-edits and conflict
C4  transfer: create-buffer(contents), read-range, resynchronize
C5  convergence, workload stalls, teardown
```

C1 is deliberately pure Rust with no guest in it, so the state machine is
exhaustively testable before a suspension point exists — the split B2e-1 used
for its kernel and host halves.

```text
C1  bound checked after the push             the allocation the bound prevents
    only the count bounded                   one 1 MiB paste stays queued
    only the bytes bounded                   ten thousand keystrokes stay queued
    desync still enqueues                    host cost grows with the stall
    desync forgets latest_revision           resync re-arms at the wrong point
    collapse keeps the queue head            partial history claims to be whole

C2  next-edit declared plain func            no guest can await UI and edits at once
    suspends while desynchronized            a stalled guest never hears it is behind
    marker cleared on delivery               the re-arm race, silently
    batch drains past its bound              one call returns the whole backlog
    a second waiter admitted                 two futures race one queue
    borrow retained across the await         a Store-derived handle outlives its call

C3  conflict applies a prefix                a refused batch mutated the document
    expected_revision unchecked              applied to text that moved underneath
    revision checked after validation        a stale+malformed batch reports invalid-edit,
                                              not conflict — coordinates judged against a
                                              revision the guest never authored against
    guest edits echoed to their origin       every guest double-applies its own
    origin filter reaches everyone           the source relearns what it already applied
    origin filter reaches no one             another relationship never hears about it
    inbound batch unbounded                  a million empty edits, or one huge one

C4  resynchronize re-arms non-atomically     edits between read and re-arm are lost
    read-range re-arms                       a conflict re-read discards the queue
    read-range used as recovery              the double-application hazard, shipped
    ceiling checked only at bootstrap        edits grow past what resync can return
    snapshot materialized on the text thread a recovery stalls the window
    bootstrap emits an edit                  the guest hears its own text as news
    bootstrap records a journal entry        undo reaches a document that never was
```

#### C5 is workload-based, not time-based

"A 500 ms stall stays synchronized" is not the invariant, and the earlier note
in *Out of scope for B* should be read as shorthand for this. A 500 ms stall
containing a 20 MiB paste must desynchronize; a twenty-minute stall with no
edits must cost nothing.

```text
a typing workload under both bounds        -> incremental, no desync
that workload plus one entry               -> desync
that workload plus one byte past the cap   -> desync
an arbitrarily long stall with zero edits  -> zero queue growth
```

A build where only some of those pass is a build where the bounds mean nothing.

`cargo-mutants`, scoped to the C modules, as the close-out frames it: it finds
the unmutated branches *around* these, not these.

#### The closure gate

Driven through a real guest and the real bridge, as B2e-3c's seam test is.
`guests/hostile` is the natural home — it already owns deliberate misbehaviour
and already holds text capabilities.

```text
non-empty bootstrap
  -> host-local typing and guest apply-edits concurrently
  -> a conflict, refused, then resolved by draining
  -> a second conflict, resolved by resynchronize instead
  -> a workload past the bounds, desynchronized
  -> resynchronize, re-armed
  -> convergence: host replica and guest document byte-identical at a
     revision both agree on
```

Gate 3 of the four stops being theoretical here:

```text
1  input-to-pixel latency stays host-local and low        B
2  a stalled guest does not stall caret, selection, IME   B
3  host and guest converge after edits and conflicts      <- C
4  1-10 MB documents do not create pathological costs     A / D
```
