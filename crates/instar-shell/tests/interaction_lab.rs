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

use std::time::Duration;

use instar_host::HostEffect;
use instar_shell::test_harness::{RuntimeHarness, launch_component};
use instar_ui::NodeKey;
use instar_window::{LogicalSize, WindowId, WindowMetricsChanged};
use winit::event::ElementState;
use winit::keyboard::NamedKey;

const WINDOW: WindowId = WindowId::from_raw(1);

const WIDTH: f64 = 480.0;
const HEIGHT: f64 = 400.0;

// The Gallery's node keys, as a host learns them: off the wire.
const STALL: NodeKey = NodeKey::first(2);
const STATUS: NodeKey = NodeKey::first(1);
const OUTER: NodeKey = NodeKey::first(10);
const POINTER_TARGET: NodeKey = NodeKey::first(12);
const DISABLED: NodeKey = NodeKey::first(13);
const OFFSCREEN: NodeKey = NodeKey::first(15);
const INNER: NodeKey = NodeKey::first(20);

trait GalleryState {
    fn status(&self) -> String;
}

impl GalleryState for RuntimeHarness {
    fn status(&self) -> String {
        self.read_text(STATUS)
    }
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

fn open() -> RuntimeHarness {
    launch_component(component(), metrics())
}

fn scrollbar(lab: &RuntimeHarness, viewport: NodeKey) -> instar_ui::Scrollbar {
    lab.scrollbar(viewport)
}

// --- Pointer -----------------------------------------------------------

/// A click, all the way from a winit button event into the guest's own state.
///
/// The readout is the assertion, not the hit test: it changes only if the
/// event reached the guest, the guest committed, and the host applied it.
#[test]
fn a_pointer_click_reaches_the_guest_and_comes_back_as_a_visible_change() {
    let mut lab = open();
    assert!(lab.status().starts_with("pointer 0"), "{}", lab.status());
    lab.expect_guest_message_count(0);

    lab.click_node(POINTER_TARGET);
    lab.expect_guest_message_count(1);
    lab.await_guest_commit()
        .expect("the click reaches the guest");

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
    let mut lab = open();
    let (x, y) = lab.screen_point_of(POINTER_TARGET);

    lab.move_to(x, y);
    lab.button(ElementState::Pressed);
    lab.set_window_focus(false);
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
    let mut lab = open();
    lab.press_key(NamedKey::Tab);
    lab.press_key(NamedKey::Tab);
    assert_eq!(lab.focused(), Some(POINTER_TARGET));

    lab.key(NamedKey::Space, true);
    lab.set_window_focus(false);
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
    let mut lab = open();
    lab.press_key(NamedKey::Tab);
    assert_eq!(lab.focused(), Some(STALL));

    lab.set_shift(true);
    lab.set_window_focus(false);
    lab.set_window_focus(true);
    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(POINTER_TARGET),
        "after focus loss and regain, Tab traverses forward -- the held \
         shift must not have been resurrected"
    );

    lab.set_shift(true);
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
    let mut lab = open();
    let bar = scrollbar(&lab, OUTER);
    let thumb_x = f64::from(bar.thumb.x + bar.thumb.width / 2);
    let thumb_y = f64::from(bar.thumb.y + 2);

    lab.move_to(thumb_x, thumb_y);
    lab.button(ElementState::Pressed);
    lab.move_to(thumb_x, thumb_y + 40.0);
    let dragged = lab.scroll_offset(OUTER);
    assert!(dragged > 0, "the drag is live before the pointer leaves");

    lab.leave_window();
    lab.move_to(thumb_x, thumb_y + 80.0);
    assert_eq!(
        lab.scroll_offset(OUTER),
        dragged,
        "the drag cannot continue after CursorLeft"
    );

    lab.button(ElementState::Released);
}

/// Hover is presentation, so CursorLeft must remove it the moment the pointer
/// is no longer over the window.
#[test]
fn cursor_left_removes_scrollbar_hover() {
    let mut lab = open();
    let bar = scrollbar(&lab, OUTER);

    lab.move_to(f64::from(bar.thumb.x + 2), f64::from(bar.thumb.y + 2));
    assert!(
        lab.hovered().is_some(),
        "hover is present before the pointer leaves"
    );

    lab.leave_window();
    assert_eq!(
        lab.hovered(),
        None,
        "hover presentation cannot survive the pointer leaving the window"
    );
}

/// A disabled control refuses the same click, at the same coordinates.
#[test]
fn a_disabled_control_refuses_a_click_that_would_otherwise_land() {
    let mut lab = open();
    let before = lab.status();
    lab.expect_guest_message_count(0);

    let effects = lab.click_node(DISABLED);

    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, HostEffect::SendToGuest(_))),
        "a disabled control must not reach the guest at all"
    );
    lab.expect_guest_message_count(0);
    assert_eq!(lab.status(), before);
}

// --- Scroll ------------------------------------------------------------

/// A wheel notch, through the real adapter, visibly moves the viewport.
#[test]
fn a_wheel_moves_the_viewport_it_is_over() {
    let mut lab = open();
    assert_eq!(lab.scroll_offset(OUTER), 0);

    // Below the inner viewport, so this is unambiguously the outer one.
    lab.wheel(WIDTH / 2.0, HEIGHT - 20.0, -3.0);

    assert!(
        lab.scroll_offset(OUTER) > 0,
        "three notches down should have scrolled the outer viewport"
    );
}

/// The wheel's residual bubbles outward when the inner viewport is spent.
///
/// A single scroll subsystem that stops at the innermost viewport looks
/// correct until a nested one exists, which is why the Gallery has one.
#[test]
fn a_wheel_over_a_nested_viewport_bubbles_its_residual_outward() {
    let mut lab = open();
    let (x, y) = lab.screen_point_of(INNER);

    // Far more than the inner viewport can absorb.
    lab.wheel(x, y, -50.0);
    let inner = lab.scroll_offset(INNER);
    assert!(inner > 0, "the inner viewport should have scrolled");

    lab.wheel(x, y, -50.0);
    assert_eq!(
        lab.scroll_offset(INNER),
        inner,
        "the inner viewport is at its limit"
    );
    assert!(
        lab.scroll_offset(OUTER) > 0,
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
    let mut lab = open();
    let bar = scrollbar(&lab, OUTER);
    let thumb_x = f64::from(bar.thumb.x + bar.thumb.width / 2);
    let thumb_y = f64::from(bar.thumb.y + 2);

    lab.move_to(thumb_x, thumb_y);
    lab.button(ElementState::Pressed);
    assert_eq!(
        lab.scroll_offset(OUTER),
        0,
        "the press alone scrolls nothing"
    );

    lab.move_to(thumb_x, thumb_y + 40.0);
    let dragged = lab.scroll_offset(OUTER);
    assert!(
        dragged > 0,
        "dragging the thumb down must move the content: offset is still \
         {dragged}"
    );

    lab.move_to(thumb_x, thumb_y + 80.0);
    assert!(
        lab.scroll_offset(OUTER) > dragged,
        "and it must keep tracking, rather than moving once and sticking"
    );

    lab.button(ElementState::Released);
    let settled = lab.scroll_offset(OUTER);
    lab.move_to(thumb_x, thumb_y + 200.0);
    assert_eq!(
        lab.scroll_offset(OUTER),
        settled,
        "and stop tracking once the button is released"
    );
}

// --- Keyboard and focus ------------------------------------------------

/// Tab moves focus and shows the ring.
#[test]
fn tab_moves_focus_forward_and_shows_the_ring() {
    let mut lab = open();
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
    let mut lab = open();
    lab.press_key(NamedKey::Tab);
    lab.press_key(NamedKey::Tab);
    assert_eq!(lab.focused(), Some(POINTER_TARGET));

    lab.set_shift(true);
    lab.press_key(NamedKey::Tab);
    assert_eq!(
        lab.focused(),
        Some(STALL),
        "shift must still be held when the key arrives"
    );

    lab.set_shift(false);
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
        let mut lab = open();
        lab.press_key(NamedKey::Tab);
        lab.press_key(NamedKey::Tab);
        assert_eq!(lab.focused(), Some(POINTER_TARGET));

        lab.press_key(key);
        lab.await_guest_commit()
            .unwrap_or_else(|error| panic!("{key:?} should have reached the guest: {error}"));
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
    let mut lab = open();
    assert_eq!(lab.scroll_offset(OUTER), 0);

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
        lab.scroll_offset(OUTER) > 0,
        "and reaching it must bring it into view"
    );

    // And it is genuinely reachable now, not merely focused.
    lab.press_key(NamedKey::Enter);
    lab.await_guest_commit()
        .expect("the guest hears the activation");
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
    let mut lab = open();
    let target = accesskit::NodeId(OFFSCREEN.to_accesskit_id());

    lab.on_accessibility_action(accesskit::Action::ScrollIntoView, target);
    assert!(
        lab.scroll_offset(OUTER) > 0,
        "ScrollIntoView must run the same reveal Tab does"
    );

    lab.on_accessibility_action(accesskit::Action::Focus, target);
    assert_eq!(lab.focused(), Some(OFFSCREEN));

    lab.on_accessibility_action(accesskit::Action::Click, target);
    lab.await_guest_commit()
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
    let mut lab = open();
    let before = lab.status();

    let started = lab.stall_guest(STALL);
    // No `await_commit`: the guest is busy, which is the point.

    // Wheel.
    lab.wheel(WIDTH / 2.0, HEIGHT - 20.0, -3.0);
    let scrolled = lab.scroll_offset(OUTER);
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
        lab.scroll_offset(OUTER) != scrolled,
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
    lab.await_guest_commit()
        .expect("the stalled guest eventually commits");
    assert!(
        lab.status().contains("stalls 1"),
        "application consequences queue rather than being dropped: {}",
        lab.status()
    );
}
