# F4: native accessibility smoke

Everything else in Instar's accessibility support is checked by the automated
suite. This is the part that cannot be: what a real screen reader, talking to a
real platform adapter, actually does with the tree Instar projects.

Three platforms, three separate native adapters underneath AccessKit. Passing on
one says nothing about the others, so each is its own gate.

```text
macOS     VoiceOver   PENDING
Windows   Narrator    PENDING
Linux     Orca        PENDING
```

Phase 2 does not close until all three are recorded here. None of them blocks
package G — the architectural uncertainty they resolve is small, and withholding
dogfooding to wait for three machines would cost more than it buys.

## The fixture

`guests/gallery` — the same application package G builds, not a separate one.
An accessibility fixture nothing else uses is a fixture that drifts from what
it is supposed to represent; the Gallery is exercised by eleven automated
integration tests on every run, so it cannot.

```text
Window
├── Text readout            -- outside every viewport, so it stays announceable
├── Button "Stall guest 500ms"
└── Scroll (grows)
    ├── Button "Pointer target"
    ├── Button "Disabled control"   -- state, not omission
    ├── Scroll (120pt)              -- a viewport inside a viewport
    │   ├── Button "Inner top"
    │   ├── spacer (300pt)
    │   └── Button "Inner bottom"
    ├── spacer (400pt)
    └── Button "Offscreen target"   -- reveal, then activation
```

The nested viewport deserves a moment with a screen reader on its own: it is
the case where "scroll into view" has more than one possible answer.

The offscreen control is the point: reaching and pressing it requires the F1
projection to describe a node that is not painted, focus to move to it, the E3
reveal path to scroll it into view, the F3 activation seam to fire from an
accessibility source rather than a pointer, and the F2 incremental update to
describe the result.

```bash
cd guests/gallery && cargo build --target wasm32-wasip2
```

```bash
./target/debug/instar run guests/gallery/target/wasm32-wasip2/debug/gallery.wasm --debug
```

### The fixture is checked separately

`the_gallery_exercises_what_the_manual_accessibility_pass_will_ask_of_it`, in
`instar-host`, asserts the tree's shape without a screen reader: the disabled
control is present and marked, the offscreen control starts outside the
viewport, focusing it scrolls it in, and activating it through the
accessibility seam reaches the guest.

`an_accessibility_action_produces_the_same_effects_as_the_other_sources`, in
the Interaction Lab, goes further: it drives real AccessKit actions into the
real Gallery guest and asserts the guest's own counter changed. Steps 5 to 8
below are therefore already proven up to the platform boundary — what the
manual pass adds is the platform itself.

That test is why a failure below is informative. If it is green and the manual
pass still fails, the failure is at the native boundary — which is exactly what
this document is for.

## The pass

Nine steps, in order. Each one depends on the last, so stop at the first
failure and record where.

```text
 1  the window and its tree are discovered at all
 2  control names are announced correctly
 3  the disabled control is announced as unavailable, not skipped
 4  navigation reaches the offscreen control
 5  focusing it scrolls it into view      (E3 reveal, from the a11y source)
 6  it can be activated from the screen reader
 7  the activation runs the same F3 seam a click does
 8  the guest's readout changes
 9  the screen reader observes the change (F2 incremental update)
10  the nested viewport's controls are reachable and reveal correctly
```

Steps 7 and 8 are one thing seen from two sides: the readout only changes if the
guest received the event, and the guest only receives it through the seam.

Step 10 is the one the automated tests cannot stand in for. Instar reveals from
the innermost viewport outwards, recomputing between steps; whether a screen
reader's own idea of "scroll into view" agrees is a platform question.

### Deactivation and reattachment

Run this second, separately. It is the one path with no automated equivalent at
the platform level, because it is where the drain rule is load-bearing.

```text
11  turn the screen reader off, interact, turn it back on
12  the reattached adapter receives a complete, coherent tree
```

The proxy delivers `InitialTreeRequested`, `ActionRequested` and
`AccessibilityDeactivated` as distinct events, and Instar answers the first by
resetting the projection and sending everything. Step 12 fails if it ever tries
to resume an incremental history whose consumer did not exist — see the
data-lifetime invariant in `ARCHITECTURE.md`.

## macOS

VoiceOver ships with the system and toggles with **Command-F5**. It takes over
audio and keyboard while running, so run it deliberately rather than in the
background of other work.

The VoiceOver caption panel makes the pass much easier to record: it shows the
announcement text on screen, so results can be captured without transcribing
speech.

## Windows

Narrator ships with Windows. Same ten steps, then deactivation.

## Linux

Orca over AT-SPI. Same ten steps, then deactivation. Note that the Linux
adapter's async runtime is a feature choice on `accesskit_winit`; Instar takes
the default (`async-io`).

## Recording a result

Replace the `PENDING` line for that platform with the date, the OS and screen
reader versions, and the highest step reached. A partial pass is a useful
result and should be recorded as one — "reached step 6, activation did not fire"
is a bug report; "PENDING" is not.
