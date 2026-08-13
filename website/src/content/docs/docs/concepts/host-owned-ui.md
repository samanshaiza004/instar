---
title: Host-owned UI
description: The ownership split — the guest owns meaning and state, the host owns geometry, interaction, accessibility, and pixels.
sidebar:
  order: 2
---

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

There is no encoding for a rectangle in the wire format, so this is not a rule
enforced by review or by a linter — a guest could not state a rectangle if it
wanted to.

## Full snapshots, retained host state

The guest commits a complete semantic snapshot. The host diffs it against the
retained tree and preserves host-local state for nodes whose full `NodeKey`
continues to represent the same thing.

This is why node identity matters. Focus and pointer state must follow semantic
identity, not "the third child." Removing a node retires its host-owned state.

## Continuous interaction stays local

Scrolling, hover, press feedback, focus traversal, and pointer capture do not
round-trip through Wasm. The guest receives completed semantic events, such as
an activation of a particular node. This keeps interactions responsive even
when the guest is busy.

The practical consequence: a guest that takes a long time to answer an event
does not make the window feel broken. The scroll still scrolls, the hover still
highlights, focus still moves — those never needed the guest's opinion.

## Accessibility is a projection

The host projects the retained semantic tree into AccessKit. When assistive
technology attaches, Instar sends a complete tree; while attached, it sends
deltas. When detached, it does not drain updates into nowhere.

This is the same tree the layout and hit-testing use, not a parallel structure
built for accessibility alone. A control that exists for the mouse exists for
the screen reader by construction.

The formal native screen-reader smoke pass remains an explicitly tracked
project-status item. The architecture and automated projections exist; that
manual platform acceptance claim is not presented as complete.
