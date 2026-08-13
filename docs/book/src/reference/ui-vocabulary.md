# UI vocabulary

The current protocol supports a small semantic tree with orthogonal layout and
style fields. The Rust source in `instar-ui-protocol` is normative.

## Nodes

| Node | Purpose | Children |
|---|---|---:|
| Root | One outer application surface | one or more |
| Column | Vertical flow | any |
| Row | Horizontal flow | any |
| Stack | Overlapping children in one cell | any |
| Scroll | Host-owned viewport and scroll state | exactly one |
| Text | Static semantic text | none |
| Button | Activatable control; may be disabled | none |
| TextView | Attachment to a host-owned text view resource | none |

`TextView` is part of protocol revision 9 and is under active integration. Do
not treat its present API as stable.

## Sizing and flow

- Preferred width and height: `Content` or `Fixed(u16)`.
- Minimum and maximum bounds.
- Flex basis, grow, and shrink.
- Main-axis justification: start, center, end, and the three space modes.
- Cross-axis alignment: start, center, end, stretch.
- Padding and gap.

There is no universal `Fill` size. Stretch and main-axis distribution are
different questions and have separate fields.

## Presence and clipping

| State | Layout | Paint | Interaction/accessibility |
|---|---|---|---|
| normal | yes | yes | yes |
| hidden | yes | no | no |
| display none | no | no | no |
| clipped | yes | inside clip only | inside clip only |

## Style

The wire carries host-interpreted intent for background and foreground color,
font role, cursor, text alignment, and related presentation properties. The
guest does not control a renderer or native platform widget.

## Identity

Each node has a numeric ID and generation. Keep the key stable while the node
means the same thing. Retire it when that meaning disappears. Host-owned focus,
scroll, pressed state, accessibility identity, and stale-event rejection all
depend on the full key.
