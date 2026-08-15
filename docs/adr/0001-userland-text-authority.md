# ADR 0001: guest-owned text authority

Status: accepted for Phase 3 Userland Authority Pivot

## Decision

Instar does not maintain a host document replica. A guest owns its document,
selection, edits, undo/redo, composition projection, viewport policy, and
commands. The host provides mechanisms only: native input transport, retained
focus/geometry, bounded immutable TextLayout resources, independent Surface
scenes, rendering, and candidate-window placement.

Surface identity is semantic `NodeKey` identity, not a new resource. A scene
retains an internal immutable layout object, never a guest capability. There
is no compatibility protocol for the deleted TextView/text-buffer world.

## Consequences

* A stalled or terminated guest cannot leave ghost edits in a host replica.
* Userland applications may choose different editor policies, including
  multi-caret behavior, without host branches or protocol vocabulary.
* TextLayout answers geometry facts and never performs editor commands.
* Presentation requests are bounded and replace scenes atomically.
* Accessibility, clipboard, files, dialogs, and product-level Scratchpad work
  remain follow-ons rather than hidden host authority.

## Proof obligations

Every new widget must demonstrate a guest-owned policy the host cannot model as
an editor special case. The Phase 3 closure proof is a two-caret insertion:
the guest transforms `abc` with carets at 1 and 3 into `aXbcX`, while the host
only receives ordinary input, layout, and scene operations.
