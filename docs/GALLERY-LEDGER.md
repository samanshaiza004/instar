# The limitation ledger

What building applications revealed Instar cannot express, recorded rather than
implemented. Opened by the Gallery; the Calculator now votes.

The rule this file exists to enforce:

> **The Gallery discovers missing primitives; it does not automatically justify
> implementing them.**

An application under construction always wants one more thing, and a toolkit
that grants every such wish becomes a pile of features held together by the
order they were requested in. So an entry here is evidence, not a decision. If
the Calculator independently reaches for the same thing, the case becomes
strong — two unrelated applications needing a primitive is the signal Phase 2's
freeze criterion was written around.

Each entry answers the same six questions.

```text
Missing capability:
Observed while building:
Can current primitives express it:
Workaround quality:
Calculator likely needs it:
Decision:
```

---

## 1. A viewport cannot be sized as a fraction of its parent — STILL ONE VOTE

**Missing capability.** A `Scroll` whose height is "half the window" or "the
rest of this row", without a literal.

**Observed while building.** The Gallery's nested viewport is
`WireSize::Fixed(120)`. The outer one uses `grow: 1.0` and is fine, because it
is the only flexible thing in its column. The inner one cannot: `grow` would
make it fight the spacer beside it for the outer viewport's overflow, and the
nested-scroll demonstration needs both viewports to have somewhere to go.

**Can current primitives express it.** Only as a literal, which does not follow
a resized window. `min_height`/`max_height` are also literals.

**Workaround quality.** Adequate here and misleading in general. A fixed 120pt
viewport looks correct at one window size and wrong at every other, and nothing
in the vocabulary says so.

**Calculator likely needs it.** Predicted *no* — "a calculator's layout is a
fixed grid of keys". **The prediction was wrong.**

**Calculator says:** *no* — and the first reading of its evidence was wrong.

A keypad wants "each key takes an equal quarter of the row", which looks like
fractional sizing and is not. The actual missing primitive is **flex basis**:
`grow` distributes free space computed from a starting size, and without a way
to state that starting size each key begins at its own content width. The fix
is `basis: 0, grow: 1`, not a percentage of anything. Taffy models the two
separately for exactly this reason — `Dimension::percent` resolves against the
containing block, `flex_basis` sets an item's initial main-axis size.

So the second vote evaporates on inspection. That is the ledger working: the
Calculator did not merely confirm a vague missing feature, it split one into
two precise semantics and only needed one of them.

**Decision.** Still one application, still not implemented. Percentage sizing
would be a genuinely new sizing mode on the strength of a single Gallery
request. Flex basis shipped instead (H3), because it closes a hole in a
vocabulary Instar already deliberately exposes — `grow` and `shrink` without a
basis is an incomplete set — and is backed by the application that needed it.

---

## 2. No way to say "this control is the default"

**Missing capability.** Marking one control as the one Enter activates when
nothing has focus.

**Observed while building.** Enter and Space activate the *focused* control,
which is correct. But a real application usually has an obvious default, and
the Gallery has no way to express "pressing Enter here means the Pointer
target" without first tabbing to it.

**Can current primitives express it.** No. A guest could focus a node itself if
the protocol allowed guest-initiated focus, which it deliberately does not:
focus is host-owned, and a guest that could steal it would break the retirement
rule that everything else depends on.

**Workaround quality.** None. The behaviour is simply absent.

**Calculator likely needs it.** Predicted plausible.

**Calculator says:** wanted, not needed. Every key is reachable by Tab and
activates with Enter or Space, so the calculator is fully usable without it.
What is missing is only that pressing Enter on a freshly-opened window does
nothing until something has been focused — an ergonomic gap, not a functional
one.

**Decision.** Still record only, and this is the useful negative result: the
Calculator is the second application and it did *not* make the case. A feature
two applications merely find pleasant is exactly what the freeze criterion
exists to keep out.

---

## 3. Text has no alignment

**Missing capability.** Centring a label, or right-aligning a number.

**Observed while building.** The Gallery's status readout is left-aligned
because that is the only thing text can be. `WireAlign` positions a *node*
within its parent; it says nothing about the glyphs inside a text node, and
`finalize` always aligns `Start`.

**Can current primitives express it.** Node alignment can centre the text
node's box, which looks like centring for a single line and stops looking like
it the moment the text wraps.

**Workaround quality.** Poor and quietly wrong. It coincides with the right
answer often enough to be mistaken for it.

**Calculator likely needs it.** Predicted almost certainly.

**Calculator says:** yes, and it is the single worst thing about writing
against Instar today. A calculator display is right-aligned in every
implementation anyone has seen, and the workarounds are both bad: stretch the
node and the digits sit left, which reads as broken; do not stretch it and the
panel hugs the digits and jumps sideways on every keypress.

**Decision.** Record, and expect to implement. The strongest candidate in the
ledger.

Alignment is implemented in the shaping layer, but it is not therefore a
*Phase 3 feature*: it is not an editing capability, it is an ordinary UI
requirement that happens to be satisfied by shaping. If the Calculator
independently needs a right-aligned display — which is close to universal for
calculators — that is precisely the second-application evidence Phase 2's
freeze criterion asks for, and it should be promoted to an ordinary Phase 2
addition rather than deferred by where its implementation lands.

---

## 4. Nested viewports put their scrollbars in the same place — CLOSED

**Missing capability.** Any way to tell two overlapping scroll regions apart.

**Observed while building.** The Gallery's inner `Scroll` uses
`align_self: Stretch`, so it spans the outer column's full width — and the
scrollbar is an *overlay*, drawn inside the viewport's right edge without
reserving layout space. Both bars therefore land at the same x, one on top of
the other for the band where they overlap. On screen they read as a single
broken bar. The nested viewport has no background, no border and no separator,
so there is nothing to say a scroll region begins there at all.

**Can current primitives express it.** Partly. A guest can give the inner
`Scroll` a background or a border, or narrow it with `max_width`, and it
becomes legible. Nothing forces it to.

**Workaround quality.** Adequate for an application author who knows to do it;
useless as a default. The Gallery hit this immediately and it was the first
thing a viewer found confusing.

**Calculator likely needs it.** No — a calculator has one scroll region at
most.

**Decision.** **Resolved by the catalog, and closed as a host policy.** The
two specimens answered both questions.

*Can existing styling make a nested region visually distinct?* **Yes.** A
background, a border and a radius were enough — the delineated specimen reads
as its own viewport immediately. So `Scroll` gets no default chrome, and an
application expresses a boundary when it wants one. Plenty of legitimate scroll
regions should disappear into the surface around them.

*Once distinct, are coincident overlay bars still confusing?* **Yes.** With the
boundary unmistakable, both bars still land in the same right-edge band and
neither can be attributed to its viewport. Viewport legibility was never the
root problem, which is exactly what the pair of specimens was built to
separate.

So `instar_ui::ScrollbarStyle { Overlay, Inset }` exists, chosen by the host:

```text
Overlay   viewport rect unchanged; the bar paints over the content edge
Inset     the bar gets its own strip; the content rectangle is narrower
          by SCROLLBAR_THICKNESS; the bar stays on the viewport edge
```

Treating this as policy has strong precedent. AppKit supports overlay and
legacy scrollers and selects between them from a *user preference*; GTK has an
explicit overlay-scrolling setting; Qt's classic scroll area reserves viewport
space when a bar appears. Three toolkits, one axis, and none of them makes it
intrinsic to the widget.

What was deliberately **not** done, per the same reasoning:

- no default background, border or separator on `Scroll`
- no moving a nested bar inward because it is nested
- no nesting depth anywhere in scrollbar geometry
- no change to `Scroll` semantics: offsets, extents, bubbling, retirement and
  interaction are identical under both policies
- **not on the wire.** A guest says *that* something scrolls, never how its
  chrome is presented. It goes on the wire only if an application shows it
  genuinely needs to override the host, and neither the Gallery nor the
  Calculator has yet.

`Inset` reserves the strip whether or not the content currently overflows.
Reserving only when a bar appears — Qt's classic behaviour — reflows content at
the moment it crosses the threshold, and oscillates for a viewport whose
content sits near its own height. CSS calls the stable version
`scrollbar-gutter: stable`; it is the same trade, and stability is worth more
than the twelve pixels.

Compare them with the Gallery's own specimens:

```bash
./target/release/instar run guests/gallery/target/wasm32-wasip2/debug/gallery.wasm
./target/release/instar run guests/gallery/target/wasm32-wasip2/debug/gallery.wasm --inset-scrollbars
```

Two runs rather than a side-by-side, because the policy is one choice for the
application. That is the price of keeping it off the wire, and it is the right
price until something demonstrates otherwise.

---

## Not in this ledger

Things the Gallery could not do that turned out to be **defects**, not missing
primitives, are fixed rather than recorded. Eight so far, all found by running
an application that was not the counter, and the last three by pointing a real
screen reader at it:

- labels wrapped according to their own fractional widths
- no wheel event ever reached the scroll subsystem
- a viewport could not be bounded by its parent
- the scrollbar thumb took a press and never moved
- Tab did not move focus
- accessibility bounds were logical where AccessKit wants physical
- a scrolled viewport reported its contents at their resting positions
- the focus ring was painted and then covered by the button's own face

The distinction matters. A missing primitive is a design question deferred; a
defect is a promise the toolkit already made and did not keep.
