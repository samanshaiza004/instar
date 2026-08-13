---
title: Questions
description: The questions the design keeps provoking, answered from what Instar actually does.
sidebar:
  order: 2
---

#### Is this Electron for WebAssembly?

No, and the difference is the direction of authority. An Electron app ships a
browser and draws its own interface. An Instar guest cannot draw: it sends a
semantic tree and the native host decides every rectangle, every pixel, and
every accessibility node. There is no framebuffer and no DOM crossing the
boundary.

#### Can I use a framework I already know?

Not today. A guest links `instar-ui-protocol` and optionally `instar-sdk`, and
that is the whole guest-side surface. Anything else — a component library, a
reactive runtime, a styling system — would be built on top of it by someone,
and nobody has yet.

#### Why can't the guest send coordinates?

Because a coordinate the host did not compute is a coordinate the host cannot
validate, and an untrusted component that can place an invisible control at a
screen position is a component that can lie about what a user is clicking. It
also keeps DPI conversion and native accessibility in one authority. See
[Host-owned UI](/docs/concepts/host-owned-ui).

#### Isn't sending the whole tree every time wasteful?

The host diffs the snapshot against its retained tree, so the work downstream
is proportional to what changed, not to what was sent. What the full snapshot
buys is that the guest never has to be right about what the host currently
believes — there is no patch stream to fall out of sync.

#### What happens when my guest has a bug?

Depends which kind. A malformed or nonsensical snapshot is refused and the
previous interface stays on screen. A trap ends the guest's generation, and the
native shell keeps the window and shows a crash surface the guest cannot
overwrite. See [Failure and recovery](/docs/concepts/failure-and-recovery).

#### Why is there only one command?

Because every additional command publishes a contract. `instar new` freezes a
project layout; `instar package` freezes a bundle format; `instar doctor`
implies a supported environment matrix. Instar has not earned the right to
freeze any of those, and a CLI that implies them anyway would be making a
promise the project cannot keep.

#### Does an idle app really cost nothing?

An idle *guest* costs nothing: it is suspended at `next-event` and no timer
wakes it to ask whether it has work. The native process, window system
resources, and runtime remain resident — the claim is about guest execution,
not about the process disappearing. Gate 0 tests it by observing polls.

#### Can I ship something with this?

Not yet, honestly. There is no tagged binary release, no compatibility policy,
and no deployment story, and the WIT contract is changing while Phase 3
proceeds. It is a good time to experiment and to read the architecture; it is
not a good time to bet a product on it.

#### Which platforms work?

macOS, Linux, and Windows, on 64-bit hosts, with a graphical session. CI runs
the suite on all three, because claims about a runtime's scheduling and timing
behaviour are not portable by assumption. See
[Requirements](/docs/getting-started/requirements).

#### Where do I look when the docs and the code disagree?

The code. `crates/instar-ui-protocol/src/lib.rs` and
`crates/instar-kernel/wit/` are normative; this guide is the readable version.
The [repository map](/docs/development/repository-map) lists which document
answers which question.
