# Host-owned UI

Instar applications own meaning and state. The host owns presentation mechanics.

| Guest owns | Host owns |
|---|---|
| application state | native window and event loop |
| semantic node structure | layout and all rectangles |
| stable node identity | hit-testing and pointer capture |
| labels and enabled state | focus, hover, press, scroll |
| response to semantic events | accessibility projection |
| text resource intent | shaping, rendering, and pixels |

## Why the guest cannot send rectangles

Geometry is absent from the protocol. A guest can request content sizing,
fixed dimensions, growth, shrinkage, alignment, padding, and gaps. The host
chooses the actual layout for its viewport, scale factor, fonts, and platform.

That keeps DPI conversion and native accessibility in one authority. It also
prevents an untrusted component from aiming an invisible control at a screen
coordinate the host did not validate.

## Full snapshots, retained host state

The guest commits a complete semantic snapshot. The host diffs it against the
retained tree and preserves host-local state for nodes whose full `NodeKey`
continues to represent the same thing.

This is why node identity matters. Focus and pointer state must follow semantic
identity, not “the third child.” Removing a node retires its host-owned state.

## Continuous interaction stays local

Scrolling, hover, press feedback, focus traversal, and pointer capture do not
round-trip through Wasm. The guest receives completed semantic events, such as
an activation of a particular node. This keeps interactions responsive even
when the guest is busy.

## Accessibility is a projection

The host projects the retained semantic tree into AccessKit. When assistive
technology attaches, Instar sends a complete tree; while attached, it sends
deltas. When detached, it does not drain updates into nowhere.

The formal native screen-reader smoke pass remains an explicitly tracked
project-status item. The architecture and automated projections exist; that
manual platform acceptance claim is not presented as complete.
