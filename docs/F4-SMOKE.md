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

## Observe before asserting

The most important rule in this document, and the one it originally got wrong.

AccessKit defines a broad, cross-platform action vocabulary — `Click`, `Focus`,
`Blur`, `ScrollIntoView` and more. Which of those a given screen reader actually
generates for a given gesture is the **native adapter's** business, and it
varies by platform. A smoke test that demands a particular sequence is testing a
guess.

So the first run of any platform pass is an *observation*. Run with `--debug`
and read what arrives:

```text
instar: a11y attached, sending the whole tree
instar: a11y Focus node 12 -> id 12 gen 0 -> entered Focus
instar: a11y ScrollIntoView node 15 -> id 15 gen 0 -> entered Reveal
instar: a11y detached
```

Each line reports the action the platform sent, the raw `NodeId`, the decoded
`NodeKey` (id and generation separately, because a stale generation is the one
failure that looks like a working request), and which canonical operation it
entered. `entered nothing` means the action crossed the seam and was refused —
either unsupported, or naming a node that is no longer eligible.

Record what a platform does. Turn it into an assertion only afterwards, and
only in this document.

### VoiceOver's cursor is not keyboard focus

The specific mistake this section exists to prevent.

macOS treats the VoiceOver cursor and keyboard focus as **two separate things**,
and provides commands to synchronize them in either direction:

```text
VO-Shift-Fn-F4    move the VoiceOver cursor to keyboard focus
VO-Fn-Command-F4  move keyboard focus to the VoiceOver cursor
```

So this expectation is wrong:

```text
VO-Right to a button  ->  Instar FocusState changes  ->  focus ring moves
```

It would be wrong for a perfectly accessible native application too. Navigating
with `VO-Left`/`VO-Right` moves the VoiceOver cursor; it does not necessarily
ask the application to move keyboard focus, and Instar should not be judged for
declining to. What the pass checks instead is that when keyboard focus is
*explicitly* moved to the VoiceOver cursor, Instar's own `FocusState`, reveal
path and focus ring all behave exactly as they would for Tab.

The user-visible requirements are the real gate, and they are weaker than any
particular action sequence:

```text
VoiceOver can discover the controls
VoiceOver can activate them
disabled controls stay disabled
offscreen controls can become reachable
explicit keyboard focus uses the normal FocusState and reveal
tree changes are reflected
```

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

### What is already proven without a screen reader

`the_gallery_exercises_what_the_manual_accessibility_pass_will_ask_of_it`, in
`instar-host`, asserts the tree's shape: the disabled control is present and
marked, the offscreen control starts outside the viewport, focusing it scrolls
it in, and activating it through the accessibility seam reaches the guest.

`an_accessibility_action_produces_the_same_effects_as_the_other_sources`, in
the Interaction Lab, drives real AccessKit actions into the real Gallery guest
and asserts the guest's own counter changed.

So if those are green and a pass below fails, the failure is at the native
boundary — which is the only thing this document is for.

---

## Pass 1 — the keyboard, with VoiceOver off

Run this first, and separately. `KeyboardInput` was literally unreachable until
recently: nothing translated it, so Tab did not move focus in the running
application at all. The Interaction Lab gets close, but `winit::event::KeyEvent`
cannot be constructed outside winit, so a real keypress through a real event
loop is the last mile no automated test here can cover.

```text
1  Tab moves focus to the first control, and a ring appears
2  Tab again skips the disabled control
3  Shift+Tab goes back
4  Enter activates the focused control; the readout changes
5  Space activates it too, and looks pressed while held
6  Tab onwards reaches the offscreen control, scrolling it into view
7  Enter there activates it; the readout changes
```

Step 5's "while held" is worth watching rather than glancing at: pressed
presentation is host-owned, and a release that does not arrive would leave a
button looking stuck.

Step 6 is where the nested viewport matters — traversal passes through the
inner scroll on its way down.

## Pass 2 — VoiceOver

Toggle with **Command-F5**. VoiceOver takes over audio and keyboard while
running, so do this deliberately rather than alongside other work. The caption
panel shows announcement text on screen, which makes results far easier to
record than transcribing speech.

```text
 1  the window and its tree are discovered at all
 2  VO-Left / VO-Right move through the controls
    (VO-Shift-Down may be needed to interact with a group)
 3  names and roles are announced sensibly
 4  the disabled control is announced as unavailable, not skipped
 5  VO-Space on the ordinary button performs its default action
 6  that reaches the F3 Activate seam -- the readout changes
 7  navigation reaches the offscreen control
 8  it becomes exposed correctly (whatever the platform does to get there)
 9  the nested viewport's controls are reachable and reveal correctly
10  VO-Fn-Command-F4 moves keyboard focus to the VoiceOver cursor,
    and *then* Instar's focus ring and reveal behave as they do for Tab
11  tree changes are reflected after an activation
```

Step 6 is the vertical proof: the readout only changes if the action reached the
guest, and the guest only hears it through the same seam a click uses.

Step 9 is the one no automated test can stand in for. AccessKit defines
`ScrollIntoView` semantically — make the target visible by scrolling the
containing scrollable regions — which lines up almost exactly with Instar's
`reveal`, and Instar reveals innermost-outwards, recomputing between steps.
Whether the platform agrees is the open question.

Step 10 is the corrected expectation. Do **not** expect steps 2 or 7 to move
Instar's focus.

### Deactivation and reattachment

Run third, separately. This is where the drain rule is load-bearing and there
is no automated equivalent at the platform level.

```text
12  turn VoiceOver off, interact with the window, turn it back on
13  the reattached adapter receives a complete, coherent tree
```

The proxy delivers `InitialTreeRequested`, `ActionRequested` and
`AccessibilityDeactivated` as distinct events, and the `--debug` log names the
first and last of those directly. Instar answers activation by resetting the
projection and sending everything. Step 13 fails if it ever tries to resume an
incremental history whose consumer did not exist — see the data-lifetime
invariant in `ARCHITECTURE.md`.

## Windows

Narrator ships with Windows. Same two passes. Narrator's navigation model
differs from VoiceOver's, so observe before asserting there too — the
cursor-versus-focus distinction is not necessarily drawn the same way.

## Linux

Orca over AT-SPI. Same two passes. The Linux adapter's async runtime is a
feature choice on `accesskit_winit`; Instar takes the default (`async-io`).

## Recording a result

Replace the `PENDING` line with the date, the OS and screen reader versions, and
the highest step reached in each pass. A partial result is a useful result:
"pass 1 clean, pass 2 reached step 6, activation did not fire" is a bug report.
"PENDING" is not.

Paste the `--debug` a11y lines alongside it. What a platform actually asks for
is the most reusable thing this exercise produces, and it is worth having on
record before the next platform is attempted.
