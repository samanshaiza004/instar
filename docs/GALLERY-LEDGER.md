# The Gallery limitation ledger

What building the Gallery revealed Instar cannot express, recorded rather than
implemented.

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

## 1. A viewport cannot be sized as a fraction of its parent

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

**Calculator likely needs it.** Probably not. A calculator's layout is a fixed
grid of keys; percentage sizing is a document-layout want.

**Decision.** Record only. Percentage lengths are a real wire change —
`WireSize` currently means "content or a bounded integer", and a percentage is
neither. Wait for a second application to ask.

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

**Calculator likely needs it.** Plausibly — `=` is a natural default. Worth
watching specifically.

**Decision.** Record only. This is a semantic property of a node, so it would
be a wire flag rather than an interaction rule, and it interacts with focus
ownership in ways that deserve their own thinking. If the Calculator wants it,
that is the second application and the case is made.

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

**Calculator likely needs it.** Almost certainly. A right-aligned display is
close to universal for calculators.

**Decision.** Record, and expect to implement. This is the strongest candidate
in the ledger, but it belongs to the text architecture rather than to G — Phase
3 owns shaping, and alignment is a shaping property. Hold until the Calculator
confirms it, then decide whether it is a Phase 2 addition or waits for Phase 3.

---

## 4. Nested viewports put their scrollbars in the same place

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

**Decision.** Record only, but note that this is not purely a guest-side
question. Two candidate host changes exist and neither should be made on one
data point:

- reserve layout space for a scrollbar instead of overlaying it, so nested
  viewports inset each other naturally (this changes every existing layout)
- give a `Scroll` a default chrome — a hairline on the viewport edge — which
  is the host inventing appearance, and the rule so far has been that it does
  not

The Gallery should first try expressing it with what exists, and the visual
catalog is the place to do that.

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
