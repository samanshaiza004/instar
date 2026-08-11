//! The Interaction Lab: every native input path, through the real shell path.
//!
//! This file exists because of a specific failure. Three complete, tested
//! subsystems — the wheel, the pointer move, the keyboard — were each
//! disconnected from the application by a single missing arm in
//! `winit_adapter::translate`. Every unit test passed. Every one of them
//! called `Host::on_wheel`, `Host::on_pointer_moved`, `Host::on_key` directly,
//! and so could not see that nothing ever called those functions.
//!
//! **So nothing here calls those.** Every test starts from a
//! `winit::event::WindowEvent`, goes through the real `translate`, the real
//! `HostBridge`, and the real Gallery guest on a real second thread, and ends
//! at something a person could see: a scroll offset, a focus ring, a rendered
//! frame, or a counter the guest itself incremented.
//!
//! ```text
//! winit::event::WindowEvent
//!    ↓  winit_adapter::translate
//! WindowOutput
//!    ↓  HostBridge::on_window_event
//! host state + guest round trip
//!    ↓
//! observable effect
//! ```
//!
//! # The one place this cannot reach the bottom
//!
//! `winit::event::KeyEvent` cannot be constructed outside winit: its
//! `platform_specific` field is a private platform type. Keyboard tests
//! therefore start one step in — at the real `instar_key` mapping applied to a
//! real `winit::keyboard::Key`, with shift supplied by a real translated
//! `ModifiersChanged` — and the existence of the `KeyboardInput` arm itself is
//! held by `every_window_output_term_is_produced_by_the_winit_adapter`. That
//! is the deepest the platform allows, and it is stated here rather than
//! quietly worked around.

use std::sync::Arc;
use std::time::{Duration, Instant};

use instar_host::HostEffect;
use instar_host::bridge::{HostBridge, Wake};
use instar_shell::default_font;
use instar_ui::NodeKey;
use instar_window::{
    LogicalSize, WindowId, WindowMetricsChanged, WindowOutput, WindowState, winit_adapter,
};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};

const WINDOW: WindowId = WindowId::from_raw(1);
const PATIENCE: Duration = Duration::from_secs(5);

const WIDTH: f64 = 480.0;
const HEIGHT: f64 = 400.0;

// The Gallery's node keys, as a host learns them: off the wire.
const STALL: NodeKey = NodeKey::first(2);
const OUTER: NodeKey = NodeKey::first(10);
const POINTER_TARGET: NodeKey = NodeKey::first(12);
const DISABLED: NodeKey = NodeKey::first(13);
const OFFSCREEN: NodeKey = NodeKey::first(15);
const INNER: NodeKey = NodeKey::first(20);

/// Everything a test drives: the window state the adapter converts against,
/// and the bridge running the guest.
struct Lab {
    state: WindowState,
    bridge: HostBridge,
}

fn component() -> Vec<u8> {
    std::fs::read(env!("GALLERY_WASM")).expect("the Gallery guest is built by build.rs")
}

fn metrics() -> WindowMetricsChanged {
    WindowMetricsChanged {
        window_id: WINDOW,
        logical_size: LogicalSize {
            width: WIDTH,
            height: HEIGHT,
        },
        physical_size: instar_window::PhysicalSize {
            width: WIDTH as u32,
            height: HEIGHT as u32,
        },
        scale_factor: 1.0,
    }
}

impl Lab {
    /// A running Gallery with its opening interface applied.
    fn open() -> Self {
        let wake: Wake = Arc::new(|| {});
        let mut bridge =
            HostBridge::spawn_with_monospace_face(component(), WINDOW, wake, default_font())
                .expect("the Gallery guest starts");
        bridge.on_window_event(WindowOutput::MetricsChanged(metrics()));

        let mut lab = Lab {
            state: WindowState::new(
                WINDOW,
                1.0,
                instar_window::PhysicalSize {
                    width: WIDTH as u32,
                    height: HEIGHT as u32,
                },
            ),
            bridge,
        };
        lab.await_commit()
            .expect("the Gallery commits its interface");
        lab
    }

    /// The real path: a winit event, translated, routed.
    ///
    /// A translation that yields nothing is not an error — for example,
    /// `ModifiersChanged` only updates translator state — so this reports
    /// whether anything was routed.
    fn send(&mut self, event: WindowEvent) -> Vec<HostEffect> {
        match winit_adapter::translate(&mut self.state, WINDOW, &event) {
            Some(output) => self.bridge.on_window_event(output),
            None => Vec::new(),
        }
    }

    /// Keyboard, as deep as winit permits. See the module docs.
    fn key(&mut self, named: NamedKey, pressed: bool) -> Vec<HostEffect> {
        let key = winit_adapter::instar_key(&WinitKey::Named(named));
        let event = self.state.on_key(key, pressed, false);
        self.bridge.on_window_event(WindowOutput::Key(event))
    }

    fn press_key(&mut self, named: NamedKey) -> Vec<HostEffect> {
        let mut effects = self.key(named, true);
        effects.extend(self.key(named, false));
        effects
    }

    fn move_to(&mut self, x: f64, y: f64) -> Vec<HostEffect> {
        self.send(WindowEvent::CursorMoved {
            device_id: winit::event::DeviceId::dummy(),
            position: PhysicalPosition::new(x, y),
        })
    }

    fn leave(&mut self) -> Vec<HostEffect> {
        self.send(WindowEvent::CursorLeft {
            device_id: winit::event::DeviceId::dummy(),
        })
    }

    fn button(&mut self, state: ElementState) -> Vec<HostEffect> {
        self.send(WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state,
            button: MouseButton::Left,
        })
    }

    fn click_at(&mut self, x: f64, y: f64) -> Vec<HostEffect> {
        self.move_to(x, y);
        let mut effects = self.button(ElementState::Pressed);
        effects.extend(self.button(ElementState::Released));
        effects
    }

    fn wheel_at(&mut self, x: f64, y: f64, lines: f32) -> Vec<HostEffect> {
        self.move_to(x, y);
        self.send(WindowEvent::MouseWheel {
            device_id: winit::event::DeviceId::dummy(),
            delta: MouseScrollDelta::LineDelta(0.0, lines),
            phase: winit::event::TouchPhase::Moved,
        })
    }

    fn shift(&mut self, held: bool) {
        let state = if held {
            ModifiersState::SHIFT
        } else {
            ModifiersState::empty()
        };
        self.send(WindowEvent::ModifiersChanged(state.into()));
    }

    fn focus(&mut self, focused: bool) -> Vec<HostEffect> {
        self.send(WindowEvent::Focused(focused))
    }

    fn offset_of(&self, viewport: NodeKey) -> i32 {
        self.bridge
            .host()
            .window(WINDOW)
            .expect("the window")
            .scroll()
            .get(viewport)
            .y
    }

    fn focused(&self) -> Option<NodeKey> {
        self.bridge
            .host()
            .window(WINDOW)
            .expect("the window")
            .focus()
            .focused()
    }

    fn focus_ring_shown(&self) -> bool {
        self.bridge
            .host()
            .window(WINDOW)
            .expect("the window")
            .focus()
            .focus_visible()
    }

    fn hovered(&self) -> Option<(NodeKey, instar_ui::ScrollbarPart)> {
        self.bridge
            .host()
            .window(WINDOW)
            .expect("the window")
            .scroll()
            .hovered()
    }

    /// The rect a control occupies, in content coordinates.
    fn rect_of(&self, key: NodeKey) -> instar_ui::Rect {
        self.bridge
            .host()
            .window(WINDOW)
            .expect("the window")
            .layout()
            .expect("layout")
            .get(key)
            .unwrap_or_else(|| panic!("{key:?} should be laid out"))
    }

    /// Where a control is on screen right now, accounting for scroll.
    fn screen_point_of(&self, key: NodeKey, viewport: NodeKey) -> (f64, f64) {
        let rect = self.rect_of(key);
        let offset = self.offset_of(viewport);
        (
            f64::from(rect.x + rect.width / 2),
            f64::from(rect.y + rect.height / 2 - offset),
        )
    }

    /// What the guest itself says has happened. The whole round trip.
    fn status(&self) -> String {
        let host = self.bridge.host();
        let window = host.window(WINDOW).expect("the window");
        let tree = window.tree().expect("a tree");
        tree.iter()
            .find(|node| node.key == NodeKey::first(1))
            .map(|node| match &node.kind {
                instar_ui::NodeKind::Text { text } => text.clone(),
                other => panic!("the status node should be text, got {other:?}"),
            })
            .expect("the status readout")
    }

    fn await_commit(&mut self) -> Option<()> {
        let target = self.bridge.commit_sequence() + 1;
        let started = Instant::now();
        while started.elapsed() < PATIENCE {
            self.bridge.wait(Duration::from_millis(25));
            if self.bridge.commit_sequence() >= target {
                return Some(());
            }
        }
        None
    }
}

/// The Gallery's scrollbar for a viewport, as the host computes it.
fn scrollbar(lab: &Lab, viewport: NodeKey) -> instar_ui::Scrollbar {
    let host = lab.bridge.host();
    let window = host.window(WINDOW).expect("the window");
    let layout = window.layout().expect("layout");
    let tree = window.tree().expect("a tree");
    let content = tree
        .iter()
        .find(|node| node.key == viewport)
        .and_then(|node| node.children.first())
        .expect("a viewport has one content child");
    let viewport_rect = layout.get(viewport).expect("the viewport");
    let content_rect = layout.get(content.key).expect("the content");
    instar_ui::Scrollbar::for_viewport(viewport_rect, content_rect.height, lab.offset_of(viewport))
        .expect("the Gallery's viewports all overflow")
}

// --- Pointer -----------------------------------------------------------

/// A click, all the way from a winit button event into the guest's own state.
///
/// The readout is the assertion, not the hit test: it changes only if the
/// event reached the guest, the guest committed, and the host applied it.
#[test]
fn a_pointer_click_reaches_the_guest_and_comes_back_as_a_visible_change() {
    let mut lab = Lab::open();
    assert!(lab.status().starts_with("pointer 0"), "{}", lab.status());

    let (x, y) = lab.screen_point_of(POINTER_TARGET, OUTER);
    lab.click_at(x, y);
    lab.await_commit().expect("the click reaches the guest");

    assert!(
        lab.status().starts_with("pointer 1"),
        "the guest should have counted the click: {}",
        lab.status()
    );
}

// --- Window lifecycle cancellation --------------------------------------

/// HARDEN-1's pointer regression, through real winit events: a press before
/// focus loss is cancelled, so a later release cannot activate anything.
#[test]
fn a_release_after_focus_loss_cannot_activate() {
    let mut lab = Lab::open();
    let (x, y) = lab.screen_point_of(POINTER_TARGET, OUTER);

    lab.move_to(x, y);
    lab.button(ElementState::Pressed);
    lab.focus(false);
    let effects = lab.button(ElementState::Released);

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, HostEffect::SendToGuest(_))),
        "the release must not reach the guest: {effects:?}"
    );
    assert!(
        lab.status().starts_with("pointer 0"),
        "the guest must not have counted a click: {}",
        lab.status()
    );
}

/// HARDEN-1's keyboard regression, at the deepest point winit permits: Space
/// goes down, focus is lost, and the Space up cannot complete a capture that
/// died with the window's focus.
#[test]
fn space_up_after_focus_loss_cannot_activate() {
    let mut lab = Lab::open();
    lab.press_key(NamedKey::Tab);
    lab.press_key(NamedKey::Tab);
    assert_eq!(lab.focused(), Some(POINTER_TARGET));

    lab.key(NamedKey::Space, true);
    lab.focus(false);
    let effects = lab.key(NamedKey::Space, false);

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, HostEffect::SendToGuest(_))),
        "the Space up must not reach the guest: {effects:?}"
    );
    assert!(
        lab.status().starts_with("pointer 0"),
        "the guest must not have counted an activation: {}",
        lab.status()
    );
}

/// Shift is translator state, so focus loss must forget it: winit will not
/// report the release while the window is unfocused, and a regain must not
/// resurrect it. Traversal stays forward until the platform reports shift
/// held again.
#[test]
fn focus_loss_forgets_shift_until_it_is_reported_again() {
    let mut lab = Lab::open();
    lab.press_key(NamedKey::Tab);
    assert_eq!(lab.focused(), Some(STALL));

    lab.shift(true);
    lab.focus(false);
    lab.focus(true);
    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(POINTER_TARGET),
        "after focus loss and regain, Tab traverses forward -- the held \
         shift must not have been resurrected"
    );

    lab.shift(true);
    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(STALL),
        "and once the platform reports shift held again, traversal reverses"
    );
}

/// A thumb drag is pointer-owned, so CursorLeft cancels it: a later move over
/// the scrollbar must not continue scrolling the viewport.
#[test]
fn cursor_left_cancels_a_thumb_drag() {
    let mut lab = Lab::open();
    let bar = scrollbar(&lab, OUTER);
    let thumb_x = f64::from(bar.thumb.x + bar.thumb.width / 2);
    let thumb_y = f64::from(bar.thumb.y + 2);

    lab.move_to(thumb_x, thumb_y);
    lab.button(ElementState::Pressed);
    lab.move_to(thumb_x, thumb_y + 40.0);
    let dragged = lab.offset_of(OUTER);
    assert!(dragged > 0, "the drag is live before the pointer leaves");

    lab.leave();
    lab.move_to(thumb_x, thumb_y + 80.0);
    assert_eq!(
        lab.offset_of(OUTER),
        dragged,
        "the drag cannot continue after CursorLeft"
    );

    lab.button(ElementState::Released);
}

/// Hover is presentation, so CursorLeft must remove it the moment the pointer
/// is no longer over the window.
#[test]
fn cursor_left_removes_scrollbar_hover() {
    let mut lab = Lab::open();
    let bar = scrollbar(&lab, OUTER);

    lab.move_to(f64::from(bar.thumb.x + 2), f64::from(bar.thumb.y + 2));
    assert!(
        lab.hovered().is_some(),
        "hover is present before the pointer leaves"
    );

    lab.leave();
    assert_eq!(
        lab.hovered(),
        None,
        "hover presentation cannot survive the pointer leaving the window"
    );
}

/// A disabled control refuses the same click, at the same coordinates.
#[test]
fn a_disabled_control_refuses_a_click_that_would_otherwise_land() {
    let mut lab = Lab::open();
    let before = lab.status();

    let (x, y) = lab.screen_point_of(DISABLED, OUTER);
    let effects = lab.click_at(x, y);

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, HostEffect::SendToGuest(_))),
        "a disabled control must not reach the guest at all"
    );
    assert_eq!(lab.status(), before);
}

// --- Scroll ------------------------------------------------------------

/// A wheel notch, through the real adapter, visibly moves the viewport.
#[test]
fn a_wheel_moves_the_viewport_it_is_over() {
    let mut lab = Lab::open();
    assert_eq!(lab.offset_of(OUTER), 0);

    // Below the inner viewport, so this is unambiguously the outer one.
    lab.wheel_at(WIDTH / 2.0, HEIGHT - 20.0, -3.0);

    assert!(
        lab.offset_of(OUTER) > 0,
        "three notches down should have scrolled the outer viewport"
    );
}

/// The wheel's residual bubbles outward when the inner viewport is spent.
///
/// A single scroll subsystem that stops at the innermost viewport looks
/// correct until a nested one exists, which is why the Gallery has one.
#[test]
fn a_wheel_over_a_nested_viewport_bubbles_its_residual_outward() {
    let mut lab = Lab::open();
    let (x, y) = lab.screen_point_of(INNER, OUTER);

    // Far more than the inner viewport can absorb.
    lab.wheel_at(x, y, -50.0);
    let inner = lab.offset_of(INNER);
    assert!(inner > 0, "the inner viewport should have scrolled");

    lab.wheel_at(x, y, -50.0);
    assert_eq!(
        lab.offset_of(INNER),
        inner,
        "the inner viewport is at its limit"
    );
    assert!(
        lab.offset_of(OUTER) > 0,
        "so the residual must reach the outer one rather than being swallowed"
    );
}

/// The gesture that was broken: press the thumb, move the mouse, content moves.
///
/// Driven entirely through winit events. `Host::on_pointer_moved` had six
/// tests and every one called it directly, which is exactly why none of them
/// noticed that no cursor move ever arrived.
#[test]
fn dragging_the_scrollbar_thumb_moves_the_content() {
    let mut lab = Lab::open();
    let bar = scrollbar(&lab, OUTER);
    let thumb_x = f64::from(bar.thumb.x + bar.thumb.width / 2);
    let thumb_y = f64::from(bar.thumb.y + 2);

    lab.move_to(thumb_x, thumb_y);
    lab.button(ElementState::Pressed);
    assert_eq!(lab.offset_of(OUTER), 0, "the press alone scrolls nothing");

    lab.move_to(thumb_x, thumb_y + 40.0);
    let dragged = lab.offset_of(OUTER);
    assert!(
        dragged > 0,
        "dragging the thumb down must move the content: offset is still \
         {dragged}"
    );

    lab.move_to(thumb_x, thumb_y + 80.0);
    assert!(
        lab.offset_of(OUTER) > dragged,
        "and it must keep tracking, rather than moving once and sticking"
    );

    lab.button(ElementState::Released);
    let settled = lab.offset_of(OUTER);
    lab.move_to(thumb_x, thumb_y + 200.0);
    assert_eq!(
        lab.offset_of(OUTER),
        settled,
        "and stop tracking once the button is released"
    );
}

// --- Keyboard and focus ------------------------------------------------

/// Tab moves focus and shows the ring.
#[test]
fn tab_moves_focus_forward_and_shows_the_ring() {
    let mut lab = Lab::open();
    assert_eq!(lab.focused(), None);
    assert!(!lab.focus_ring_shown(), "nothing is focused to begin with");

    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(STALL),
        "the first focusable control is the stall button"
    );
    assert!(
        lab.focus_ring_shown(),
        "keyboard traversal shows the ring -- that is what distinguishes it \
         from a click"
    );

    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(POINTER_TARGET),
        "and the disabled control is skipped, not focused"
    );
}

/// Shift+Tab traverses backwards.
///
/// Winit reports modifiers on their own event, so this is the case where an
/// integration test earns its keep: the shift has to survive from one event to
/// the key it modifies, or focus can only ever go forwards.
#[test]
fn shift_tab_traverses_backwards() {
    let mut lab = Lab::open();
    lab.press_key(NamedKey::Tab);
    lab.press_key(NamedKey::Tab);
    assert_eq!(lab.focused(), Some(POINTER_TARGET));

    lab.shift(true);
    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(STALL),
        "shift must still be held when the key arrives"
    );

    lab.shift(false);
    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(POINTER_TARGET),
        "and releasing it must be noticed"
    );
}

/// Enter and Space both activate the focused control, and the guest sees it.
#[test]
fn enter_and_space_activate_the_focused_control() {
    for (key, expected) in [
        (NamedKey::Enter, "pointer 1"),
        (NamedKey::Space, "pointer 1"),
    ] {
        let mut lab = Lab::open();
        lab.press_key(NamedKey::Tab);
        lab.press_key(NamedKey::Tab);
        assert_eq!(lab.focused(), Some(POINTER_TARGET));

        lab.press_key(key);
        lab.await_commit()
            .unwrap_or_else(|| panic!("{key:?} should have reached the guest"));
        assert!(
            lab.status().starts_with(expected),
            "{key:?} did not activate the focused control: {}",
            lab.status()
        );
    }
}

/// Traversal reaches a control that is not on screen, and reveals it.
#[test]
fn tabbing_to_an_offscreen_control_scrolls_it_into_view() {
    let mut lab = Lab::open();
    assert_eq!(lab.offset_of(OUTER), 0);

    // Forward until the last control has focus.
    for _ in 0..12 {
        lab.press_key(NamedKey::Tab);
        if lab.focused() == Some(OFFSCREEN) {
            break;
        }
    }
    assert_eq!(
        lab.focused(),
        Some(OFFSCREEN),
        "traversal should reach the offscreen control"
    );
    assert!(
        lab.offset_of(OUTER) > 0,
        "and reaching it must bring it into view"
    );

    // And it is genuinely reachable now, not merely focused.
    lab.press_key(NamedKey::Enter);
    lab.await_commit().expect("the guest hears the activation");
    assert!(
        lab.status().contains("offscreen 1"),
        "the revealed control should activate like any other: {}",
        lab.status()
    );
}

// --- Accessibility -----------------------------------------------------

/// An AccessKit action takes the same route a pointer or key does.
///
/// F3's claim, seen from the application: the accessibility source is not a
/// parallel implementation, it is a third adapter onto one system.
#[test]
fn an_accessibility_action_produces_the_same_effects_as_the_other_sources() {
    let mut lab = Lab::open();
    let target = accesskit::NodeId(OFFSCREEN.to_accesskit_id());

    lab.bridge
        .on_accessibility_action(accesskit::Action::ScrollIntoView, target);
    assert!(
        lab.offset_of(OUTER) > 0,
        "ScrollIntoView must run the same reveal Tab does"
    );

    lab.bridge
        .on_accessibility_action(accesskit::Action::Focus, target);
    assert_eq!(lab.focused(), Some(OFFSCREEN));

    lab.bridge
        .on_accessibility_action(accesskit::Action::Click, target);
    lab.await_commit()
        .expect("an accessibility activation reaches the guest");
    assert!(
        lab.status().contains("offscreen 1"),
        "and activation must reach the guest exactly as a click does: {}",
        lab.status()
    );
}

// --- Guest stall -------------------------------------------------------

/// The claim the whole architecture rests on, demonstrated rather than argued.
///
/// The guest is blocked for 500ms. Everything below is host-owned, and must
/// keep working throughout — while the guest's own state stays frozen, because
/// the guest genuinely is not running.
#[test]
fn native_interaction_survives_a_blocked_guest() {
    let mut lab = Lab::open();
    let before = lab.status();

    let (x, y) = lab.screen_point_of(STALL, OUTER);
    lab.click_at(x, y);
    // No `await_commit`: the guest is busy, which is the point.

    let started = Instant::now();

    // Wheel.
    lab.wheel_at(WIDTH / 2.0, HEIGHT - 20.0, -3.0);
    let scrolled = lab.offset_of(OUTER);
    assert!(
        scrolled > 0,
        "the wheel must work while the guest is blocked"
    );

    // Thumb drag.
    let bar = scrollbar(&lab, OUTER);
    let thumb_x = f64::from(bar.thumb.x + bar.thumb.width / 2);
    let thumb_y = f64::from(bar.thumb.y + 2);
    lab.move_to(thumb_x, thumb_y);
    lab.button(ElementState::Pressed);
    lab.move_to(thumb_x, thumb_y + 30.0);
    assert!(
        lab.offset_of(OUTER) != scrolled,
        "and so must the scrollbar thumb"
    );
    lab.button(ElementState::Released);

    // Focus and its ring. The click that started the stall also focused the
    // stall button -- without showing a ring, because a click is not keyboard
    // traversal -- so Tab moves on from there.
    assert_eq!(lab.focused(), Some(STALL));
    assert!(
        !lab.focus_ring_shown(),
        "a click focuses without showing the keyboard ring"
    );
    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(POINTER_TARGET),
        "traversal must keep working while the guest is blocked"
    );
    assert!(
        lab.focus_ring_shown(),
        "focus presentation is host-owned too"
    );

    assert_eq!(
        lab.status(),
        before,
        "meanwhile the guest has genuinely not run -- if this changed, the \
         stall did not happen and the test proves nothing"
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "all of that has to happen *during* the stall to mean anything; it \
         took {:?}",
        started.elapsed()
    );

    // And the queued consequence arrives once the guest wakes.
    lab.await_commit()
        .expect("the stalled guest eventually commits");
    assert!(
        lab.status().contains("stalls 1"),
        "application consequences queue rather than being dropped: {}",
        lab.status()
    );
}
