# ADR 0003: UI primitives and userland controls

Status: proposed

## Context

Instar already has the right split for custom applications: guests author
complete declarative snapshots, the host admits and retains them atomically,
and `Surface` provides a bounded custom-rendering and raw-input escape hatch.
The next controls must not turn the host into a catalogue of policy-bearing
widgets, and ordinary applications must not have to build every control as a
canvas scene.

The primary ownership test is:

> If state can change without changing what the application means, the host is
> a strong candidate to own it. If changing it changes application truth or
> custom interaction semantics, the guest is the strong candidate.

This is an ownership decision, not a wire-format decision. Exact structs,
opcode numbers, semantic roles, and style encoding require implementation
spikes and tests later.

## Decision

Keep the existing snapshot boundary and evolve the host vocabulary through a
small set of generic mechanisms rather than host widgets:

```text
guest-authored declarative snapshot
              ↓
       atomic host admission
              ↓
       host-retained realization
              ↓
     layout / interaction / a11y
              ↓
             pixels
```

The host owns semantic geometry, layout, hit testing, focus, scrolling, native
input routing, rendering machinery, and native accessibility adaptation. The
guest owns application state and application-specific interaction policy.
There is no imperative remote `create_node`, `insert_child`, or `mutate_node`
API, and no compatibility protocol is introduced for this direction.

### Ownership boundary

Host-local transient mechanism state includes hover, pressed, focus,
focus-visible, pointer-capture lifecycle, scroll offset and momentum, layout
geometry, hit testing, standard activation mechanics, and the native
accessibility adapter. Guest-owned state includes checkbox and slider values,
document contents, editor selections, undo, the response to activation, and
custom widget semantics or interaction policy.

The host does not wake the guest merely because a host-local transient state
changed, and it never mutates a guest-owned value as a side effect of
presentation.

### `Action` is the first primitive

`Action` is provisionally a composable activatable region, not a visual Button
widget. It may contain an icon, text, or arbitrary ordinary children.

The eventual host mechanism may own:

* hit testing and disabled gating;
* pointer press/release and keyboard activation mechanics;
* focus, hover, pressed, and focus-visible state; and
* one semantic `Activate(NodeKey)` event for a completed activation.

The guest owns the children, enabled/value state, appearance description, and
the application response to `Activate`. `Action` is deliberately not assigned
the accessibility role `Button`: activation mechanics and semantic role are
related but distinct. A menu item, list row, and button may share the former
without sharing the latter.

### Host-local appearance variants

The smallest useful future mechanism is a bounded generic state-style facility,
initially exercised by `Action`. A guest may eventually admit a finite set of
presentation variants, and the host may select among them without waking the
guest using only:

```text
normal · hovered · pressed · focus-visible · disabled
```

This is not a CSS engine: there are no selectors, cascade, specificity,
arbitrary guest-defined state machines, or host-side application state. The
variant mechanism is future direction and is not current protocol.

### Userland controls

The planned first-party `instar-controls` crate is ordinary guest-side
userland, not privileged host functionality. It may eventually offer Button,
Checkbox, Switch, RadioGroup, Slider, Tabs, FormField, Menu, and standard
TextField/TextArea controls by composing public Instar primitives and keeping
their application policy in guest code. Third-party controls may replace it.

The Novel-Widget Test remains mandatory: a guest must be able to implement a
control the host has never heard of without changing host code, unless the
control genuinely requires new native machinery.

### Surface remains first-class

`Surface` remains a semantic leaf with an independently replaceable bounded
scene and neutral raw input. It is the correct mechanism for editors,
terminals, DAW timelines, waveforms, dense grids, node graphs, CAD/image
tools, games, and other genuinely custom views.

Surface must not become a second hidden UI framework with generic host hover,
drag, or selection rules, and ordinary applications must not be forced to
build ordinary controls inside it. Surface visual scenes remain independent
of the semantic snapshot.

### Accessibility direction

Current ordinary nodes continue to project from the retained host tree into
AccessKit where they do today. Future portable semantic metadata belongs on the
ordinary declarative snapshot; it must not expose AccessKit types on the guest
wire or require one `NodeKind` per accessibility role. Candidate concepts
include role, label/description, checked/selected/expanded state, value/range,
relationships, and actions, but this ADR freezes no vocabulary.

A custom Surface will eventually need a guest-authored semantic projection
retained alongside its visual scene. The host may map that projection to
AccessKit/native accessibility, but it must not infer semantics from paint
commands, rectangles, or text runs. Visual and semantic Surface revisions must
have an explicit coherency rule so a platform cannot expose semantics from an
unrelated visual revision.

## Implementation roadmap

This ADR records direction only. The implementation sequence is:

1. **Action contract spike** — define admission, activation, disabled gating,
   focus, and event semantics without choosing final wire numbers.
2. **Host-local transient state** — prove hover, pressed, and focus-visible
   changes repaint without guest wakeups and retire on node identity changes.
3. **State-dependent presentation variants** — add the bounded five-state
   mechanism only after the host-local state proof is green.
4. **Replace and prove an existing Button** — preserve the current behavior
   while demonstrating that Action mechanics are not Button semantics.
5. **Minimal `instar-controls`** — build first-party guest controls from public
   primitives; keep the crate replaceable and policy-bearing.
6. **Portable semantics vocabulary** — design and test metadata independently
   of AccessKit implementation types.
7. **Surface accessibility projection** — add guest-authored semantic state with
   visual/semantic revision coherence and native mapping.

## Architectural mutants and proofs

Each roadmap package must carry a proof that would fail at the ownership
boundary it protects:

| Package | Boundary mutant | Required proof |
|---|---|---|
| A — Action | Force Action to accessibility role `Button`, or make it a visual host widget | A menu-row fixture shares activation mechanics but retains distinct semantics and arbitrary children without a new widget kind |
| B — transient state | Hover or pressed state wakes the guest | Pointer movement/press changes retained presentation with zero guest event and no application-state mutation |
| C — variants | Host invents a new state or mutates a guest value | Only the five admitted variants are selectable; a Checkbox value remains unchanged while host state changes |
| D — replacement proof | A first-party Checkbox requires a new host `NodeKind` | The guest control composes existing public primitives and passes the same host layering test as a third-party replacement |
| E — semantics | AccessKit types leak into the guest protocol | Wire-level metadata remains Instar-owned and portable; no guest dependency or field names an AccessKit type |
| F — Surface a11y | Host infers semantics from paint commands | A Surface scene with misleading rectangles/text yields no semantic update without an explicit guest projection |
| G — coherency | Semantic revision drifts from the displayed visual revision | Replacing either projection independently is refused or held until the explicit revision relationship is satisfied |

## Non-goals

This decision does not add imperative render-object handles, mutable remote
DOM APIs, a host node kind for every widget, document/editor policy in the
host, accessibility inferred from paint, CSS/cascade machinery, literal native
platform controls, or a requirement that all UI use Surface. Native view
composition is a separate future architecture question.

## Consequences

The host remains small and mechanically testable, while userland controls can
evolve policy and appearance without expanding the host protocol for every
product idea. The cost is that Action and semantic metadata need careful,
explicit contracts before implementation, and Surface accessibility requires
its own projection rather than a heuristic over pixels.
