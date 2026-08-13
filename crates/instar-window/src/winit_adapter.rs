//! The winit plumbing, kept deliberately thin.
//!
//! Everything interesting — the DPI arithmetic, the ordering rules, the
//! cursor bookkeeping — lives in [`WindowState`], which knows nothing about
//! winit and is therefore testable on a machine with no display. This module
//! is the shell that turns winit's event enum into calls on it, and it is
//! meant to stay boring enough that reading it is sufficient review.
//!
//! Nothing here is exercised by the headless test suite, because running an
//! event loop needs a display server. That is the reason for the split: the
//! part that cannot be tested in CI is the part with no logic in it.

use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key as WinitKey, NamedKey};

use crate::{
    Key, PhysicalSize, PointerButton, PointerState, RawPointerMoved, ScrollDelta, WindowId,
    WindowOutput, WindowState,
};

impl From<winit::window::WindowId> for WindowId {
    fn from(id: winit::window::WindowId) -> Self {
        WindowId::from_raw(u64::from(id))
    }
}

impl From<MouseButton> for PointerButton {
    fn from(button: MouseButton) -> Self {
        match button {
            MouseButton::Left => PointerButton::Primary,
            MouseButton::Right => PointerButton::Secondary,
            MouseButton::Middle => PointerButton::Middle,
            MouseButton::Back => PointerButton::Other(3),
            MouseButton::Forward => PointerButton::Other(4),
            MouseButton::Other(code) => PointerButton::Other(code),
        }
    }
}

impl From<ElementState> for PointerState {
    fn from(state: ElementState) -> Self {
        match state {
            ElementState::Pressed => PointerState::Pressed,
            ElementState::Released => PointerState::Released,
        }
    }
}

impl From<winit::dpi::PhysicalSize<u32>> for PhysicalSize {
    fn from(size: winit::dpi::PhysicalSize<u32>) -> Self {
        Self {
            width: size.width,
            height: size.height,
        }
    }
}

/// Translates one winit event, updating `state` as required.
///
/// Returns `None` for events Instar does not act on — which is most of them.
/// Unhandled events are ignored rather than matched exhaustively so that a
/// winit upgrade adding a variant does not break the build; the cost is that
/// adding support for one is a deliberate act.
pub fn translate(
    state: &mut WindowState,
    window_id: WindowId,
    event: &WindowEvent,
) -> Option<WindowOutput> {
    // A stray event for another window must never be translated with this
    // window's scale factor.
    if window_id != state.window_id() {
        return None;
    }

    match event {
        WindowEvent::CloseRequested => Some(WindowOutput::CloseRequested { window_id }),

        WindowEvent::RedrawRequested => Some(WindowOutput::RedrawRequested { window_id }),

        WindowEvent::Resized(physical) => Some(WindowOutput::MetricsChanged(
            state.on_resized((*physical).into()),
        )),

        // Winit's documented way to track runtime DPI changes. The new scale is
        // stored immediately, so the next pointer event converts with it, but
        // no *metrics* are emitted: the matching physical size is not available
        // here. What is emitted is the barrier -- everything the host derived
        // from the old geometry is now stale, and it needs to know that before
        // it processes any further event, not after.
        //
        // Ordering is the whole reason this is a distinct signal: winit runs
        // `about_to_wait` after queued window events and redraw callbacks, so a
        // `RedrawRequested` can arrive between the scale change and the flush.
        WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
            state.on_scale_factor_changed(*scale_factor);
            Some(WindowOutput::MetricsInvalidated {
                window_id: state.window_id(),
            })
        }

        // A move carries no button, so it is not a `Pointer` event -- but it
        // is still an event. Hover and a live thumb drag are both continuous,
        // and both are pure presentation the host owns alone. This arm
        // returned `None` for the whole of packages B through F, which left
        // `Host::on_pointer_moved` implemented, tested, and unreachable: the
        // scrollbar thumb took a press and then never moved.
        WindowEvent::CursorMoved { position, .. } => {
            let logical_pos = state.on_cursor_moved(position.x, position.y);
            Some(WindowOutput::PointerMoved(RawPointerMoved {
                window_id,
                logical_pos,
            }))
        }

        WindowEvent::CursorLeft { .. } => {
            state.on_cursor_left();
            Some(WindowOutput::PointerLeft { window_id })
        }

        WindowEvent::MouseInput {
            button,
            state: element_state,
            ..
        } => state
            .on_mouse_input((*button).into(), (*element_state).into())
            .map(WindowOutput::Pointer),

        // winit's positive Y means "the content should move down", which
        // reveals what is *above* -- so it is Instar's negative offset delta.
        // That is the whole of `natural: false`: the sign is inverted exactly
        // once, in `WindowState::on_wheel`, and this is the caller that says
        // so. The platform has already applied the user's natural-scrolling
        // preference before winit sees it, so there is nothing else to ask.
        WindowEvent::MouseWheel { delta, .. } => state
            .on_wheel(
                match delta {
                    // A count of notches, not a distance: no scale factor.
                    MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines {
                        x: f64::from(*x),
                        y: f64::from(*y),
                    },
                    // Physical pixels, which `on_wheel` converts.
                    MouseScrollDelta::PixelDelta(position) => ScrollDelta::Logical {
                        x: position.x,
                        y: position.y,
                    },
                },
                false,
            )
            .map(WindowOutput::Scroll),

        // Winit reports modifiers as their own event rather than on the key
        // events they apply to, so the state is held until a key arrives.
        WindowEvent::ModifiersChanged(modifiers) => {
            state.on_modifiers_changed(modifiers.state().shift_key());
            None
        }

        // Keyboard focus is a lifecycle cancellation surface: losing focus
        // means winit will not deliver the releases of keys held while the
        // window was focused, so the translator drops what it was holding.
        // Gaining focus emits the event but restores nothing.
        WindowEvent::Focused(focused) => {
            state.on_focus_changed(*focused);
            Some(WindowOutput::WindowFocusChanged {
                window_id,
                focused: *focused,
            })
        }

        // Package E built focus traversal, Enter/Space activation and the
        // focus ring, and `Host::on_key` routed all of it -- with nothing
        // translating a key. This arm is what makes any of it reachable.
        //
        // The *logical* key, not the physical one: Instar asks which key this
        // is in the user's layout, and a physical-position mapping is for
        // games that want WASD to stay where it is regardless of what the
        // keycaps say.
        WindowEvent::KeyboardInput { event, .. } => Some(WindowOutput::Key(state.on_key(
            instar_key(&event.logical_key),
            event.state.is_pressed(),
            event.repeat,
        ))),

        // IME input crosses the seam as text, not as keys: winit delivers the
        // full preedit string, the byte-wise caret range inside it, and the
        // whole committed string. The host owns the payload, so it is cloned
        // out of the event before this event is dropped.
        WindowEvent::Ime(ime) => match ime {
            Ime::Enabled => Some(WindowOutput::ImeEnabled { window_id }),
            Ime::Preedit(text, cursor_range) => Some(WindowOutput::ImePreedit {
                window_id,
                text: text.clone(),
                cursor_range: *cursor_range,
            }),
            Ime::Commit(text) => Some(WindowOutput::ImeCommit {
                window_id,
                text: text.clone(),
            }),
            Ime::Disabled => Some(WindowOutput::ImeDisabled { window_id }),
        },

        _ => None,
    }
}

/// Winit's logical key, in Instar's deliberately small vocabulary.
///
/// The *logical* key, not the physical one: Instar asks which key this is in
/// the user's layout, and a physical-position mapping is for games that want
/// WASD to stay put regardless of what the keycaps say.
///
/// Split out from the match arm, and public, because `winit::event::KeyEvent`
/// cannot be constructed outside winit -- its `platform_specific` field is a
/// private platform type. An integration test therefore cannot start from a
/// real `KeyboardInput` event, and the closest it can get is the real mapping
/// applied to a real `winit::keyboard::Key`. Exporting this is what lets those
/// tests use the mapping rather than reimplement it, which would leave them
/// agreeing with themselves.
pub fn instar_key(logical: &WinitKey) -> Key {
    match logical {
        WinitKey::Named(NamedKey::Tab) => Key::Tab,
        WinitKey::Named(NamedKey::Enter) => Key::Enter,
        WinitKey::Named(NamedKey::Space) => Key::Space,
        WinitKey::Named(NamedKey::Escape) => Key::Escape,
        WinitKey::Named(NamedKey::ArrowLeft) => Key::ArrowLeft,
        WinitKey::Named(NamedKey::ArrowRight) => Key::ArrowRight,
        WinitKey::Named(NamedKey::Home) => Key::Home,
        WinitKey::Named(NamedKey::End) => Key::End,
        WinitKey::Named(NamedKey::Backspace) => Key::Backspace,
        WinitKey::Named(NamedKey::Delete) => Key::Delete,
        // Carried rather than dropped: the host is entitled to know a key
        // happened without this crate growing an opinion about which one.
        _ => Key::Other,
    }
}

#[cfg(test)]
mod tests {
    //! The wheel's trip from winit into Instar's coordinate and sign
    //! conventions.
    //!
    //! The scroll subsystem itself -- bubbling, clamping, scrollbar geometry --
    //! is tested in `instar-ui` and `instar-host`. What is checked here is only
    //! that a wheel event *arrives*, in the right units and pointing the right
    //! way. That link was missing entirely: everything downstream of it existed
    //! and was tested, and no wheel ever reached any of it.

    use super::*;
    use crate::{LogicalPoint, RawScrollEvent};
    use winit::dpi::PhysicalPosition;
    use winit::event::{DeviceId, ElementState, MouseScrollDelta, TouchPhase};

    const WINDOW: WindowId = WindowId::from_raw(1);

    fn state(scale: f64) -> WindowState {
        let mut state = WindowState::new(
            WINDOW,
            scale,
            PhysicalSize {
                width: (400.0 * scale) as u32,
                height: (300.0 * scale) as u32,
            },
        );
        // A wheel needs somewhere to be: `on_wheel` yields nothing until the
        // cursor position is known.
        state.on_cursor_moved(20.0 * scale, 40.0 * scale);
        state
    }

    fn wheel(delta: MouseScrollDelta) -> WindowEvent {
        WindowEvent::MouseWheel {
            device_id: DeviceId::dummy(),
            delta,
            phase: TouchPhase::Moved,
        }
    }

    fn scrolled(state: &mut WindowState, delta: MouseScrollDelta) -> RawScrollEvent {
        match translate(state, WINDOW, &wheel(delta)) {
            Some(WindowOutput::Scroll(event)) => event,
            other => panic!("a wheel must translate to a scroll, got {other:?}"),
        }
    }

    fn moved(x: f64, y: f64) -> WindowEvent {
        WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(x, y),
        }
    }

    fn mouse(state: ElementState) -> WindowEvent {
        WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state,
            button: winit::event::MouseButton::Left,
        }
    }

    fn left() -> WindowEvent {
        WindowEvent::CursorLeft {
            device_id: DeviceId::dummy(),
        }
    }

    fn focused(focused: bool) -> WindowEvent {
        WindowEvent::Focused(focused)
    }

    fn ime(ime: Ime) -> WindowEvent {
        WindowEvent::Ime(ime)
    }

    /// A move is an event, and it carries where it happened.
    ///
    /// This arm returned `None` for the whole of packages B through F, which
    /// left `Host::on_pointer_moved` implemented, tested, and unreachable --
    /// the scrollbar thumb accepted a press and then never moved.
    #[test]
    fn a_cursor_move_is_delivered_in_logical_coordinates() {
        let mut state = state(2.0);
        match translate(&mut state, WINDOW, &moved(100.0, 60.0)) {
            Some(WindowOutput::PointerMoved(event)) => {
                assert_eq!(event.window_id, WINDOW);
                assert_eq!(
                    event.logical_pos,
                    LogicalPoint { x: 50.0, y: 30.0 },
                    "physical to logical, like every other distance crossing \
                     this boundary"
                );
            }
            other => panic!("a cursor move must be delivered, got {other:?}"),
        }
    }

    /// A move still updates the position a later button event uses, which is
    /// what it did back when it was the only thing it did.
    #[test]
    fn a_move_still_positions_the_button_event_that_follows_it() {
        let mut state = state(2.0);
        translate(&mut state, WINDOW, &moved(80.0, 40.0));
        match translate(&mut state, WINDOW, &mouse(ElementState::Pressed)) {
            Some(WindowOutput::Pointer(event)) => {
                assert_eq!(event.logical_pos, LogicalPoint { x: 40.0, y: 20.0 })
            }
            other => panic!("expected a press, got {other:?}"),
        }
    }

    /// The pointer leaving is an event with a lifecycle, not a quiet internal
    /// detail: the cursor position is cleared and the host is told, so it can
    /// cancel hover, a press, and a thumb drag that can no longer continue.
    #[test]
    fn cursor_left_clears_the_position_and_emits_pointer_left() {
        let mut state = state(1.0);
        translate(&mut state, WINDOW, &moved(40.0, 30.0));
        assert_eq!(state.last_cursor(), Some(LogicalPoint::new(40.0, 30.0)));

        match translate(&mut state, WINDOW, &left()) {
            Some(WindowOutput::PointerLeft { window_id }) => assert_eq!(window_id, WINDOW),
            other => panic!("CursorLeft must emit PointerLeft, got {other:?}"),
        }
        assert_eq!(
            state.last_cursor(),
            None,
            "CursorLeft clears the position the next button event would use"
        );
        assert!(
            translate(&mut state, WINDOW, &mouse(ElementState::Released)).is_none(),
            "a button event after the pointer left has nowhere to be"
        );
    }

    /// Focus loss is the same class of cancellation for the keyboard: the
    /// window may never hear the releases, so holding shift past a loss would
    /// make the first Tab after regain traverse backwards forever.
    #[test]
    fn focus_loss_clears_cursor_and_shift_and_emits_window_focus_changed() {
        let mut state = state(1.0);
        translate(&mut state, WINDOW, &moved(40.0, 30.0));
        translate(
            &mut state,
            WINDOW,
            &WindowEvent::ModifiersChanged(winit::keyboard::ModifiersState::SHIFT.into()),
        );

        match translate(&mut state, WINDOW, &focused(false)) {
            Some(WindowOutput::WindowFocusChanged { window_id, focused }) => {
                assert_eq!(window_id, WINDOW);
                assert!(!focused);
            }
            other => panic!("Focused(false) must emit WindowFocusChanged, got {other:?}"),
        }
        assert_eq!(
            state.last_cursor(),
            None,
            "focus loss leaves no cursor position behind"
        );
        assert!(
            !state.on_key(Key::Tab, true, false).shift,
            "focus loss forgets shift rather than waiting for a release that \
             may never arrive"
        );
    }

    /// A regain is reported so the host has the full lifecycle, but it is not
    /// permission to resurrect input that ended with the loss.
    #[test]
    fn focus_gain_emits_the_event_without_resurrecting_cursor_or_modifiers() {
        let mut state = state(1.0);
        translate(&mut state, WINDOW, &moved(40.0, 30.0));
        translate(
            &mut state,
            WINDOW,
            &WindowEvent::ModifiersChanged(winit::keyboard::ModifiersState::SHIFT.into()),
        );
        translate(&mut state, WINDOW, &focused(false));

        match translate(&mut state, WINDOW, &focused(true)) {
            Some(WindowOutput::WindowFocusChanged { window_id, focused }) => {
                assert_eq!(window_id, WINDOW);
                assert!(focused);
            }
            other => panic!("Focused(true) must emit WindowFocusChanged, got {other:?}"),
        }
        assert_eq!(
            state.last_cursor(),
            None,
            "a regain must not fake a cursor position that was never reported"
        );
        assert!(
            !state.on_key(Key::Tab, true, false).shift,
            "a regain must not resurrect a shift state that ended with the loss"
        );
    }

    /// The window-layer half of the HARDEN-1 press regression: a press before
    /// focus loss, followed by a release, must not even be attributed to a
    /// position once the loss cleared the cursor.
    #[test]
    fn a_release_after_focus_loss_has_nowhere_to_land() {
        let mut state = state(1.0);
        translate(&mut state, WINDOW, &moved(40.0, 30.0));
        match translate(&mut state, WINDOW, &mouse(ElementState::Pressed)) {
            Some(WindowOutput::Pointer(event)) => assert_eq!(event.state, PointerState::Pressed),
            other => panic!("expected a press, got {other:?}"),
        }

        translate(&mut state, WINDOW, &focused(false));

        assert!(
            translate(&mut state, WINDOW, &mouse(ElementState::Released)).is_none(),
            "the release must not be attributed to a position that left with \
             focus"
        );
    }

    /// The wrong-window guard applies here too: this window's scale factor
    /// would otherwise convert another window's coordinates.
    #[test]
    fn a_move_for_another_window_is_not_translated() {
        let mut state = state(2.0);
        assert!(translate(&mut state, WindowId::from_raw(2), &moved(100.0, 60.0)).is_none());
    }

    /// Every key Instar's vocabulary names maps to itself, and everything
    /// else maps to `Other` rather than to nothing.
    ///
    /// Package E built focus traversal, Enter/Space activation and the focus
    /// ring, and `Host::on_key` routed all of it -- with nothing translating a
    /// key. None of it was reachable from the running application.
    #[test]
    fn the_keys_the_vocabulary_names_map_to_themselves() {
        for (named, want) in [
            (NamedKey::Tab, Key::Tab),
            (NamedKey::Enter, Key::Enter),
            (NamedKey::Space, Key::Space),
            (NamedKey::Escape, Key::Escape),
            (NamedKey::ArrowLeft, Key::ArrowLeft),
            (NamedKey::ArrowRight, Key::ArrowRight),
            (NamedKey::Home, Key::Home),
            (NamedKey::End, Key::End),
            (NamedKey::Backspace, Key::Backspace),
            (NamedKey::Delete, Key::Delete),
        ] {
            assert_eq!(
                instar_key(&WinitKey::Named(named)),
                want,
                "{named:?} lost its identity in transit"
            );
        }

        assert_eq!(
            instar_key(&WinitKey::Named(NamedKey::F1)),
            Key::Other,
            "an unnamed key is still an event -- the host is entitled to know \
             a key happened"
        );
        assert_eq!(
            instar_key(&WinitKey::Character("a".into())),
            Key::Other,
            "and character input is Phase 3's, not a fifth named key"
        );
    }

    /// IME text is a payload, not a key name: the whole string and the
    /// byte-wise caret range cross the seam, and the preedit and commit arms
    /// own the strings rather than borrowing the winit event.
    #[test]
    fn ime_preedit_carries_the_full_string_and_cursor_range() {
        let mut state = state(1.0);
        match translate(
            &mut state,
            WINDOW,
            &ime(Ime::Preedit("あb".into(), Some((1, 1)))),
        ) {
            Some(WindowOutput::ImePreedit {
                window_id,
                text,
                cursor_range,
            }) => {
                assert_eq!(window_id, WINDOW);
                assert_eq!(text, "あb");
                assert_eq!(cursor_range, Some((1, 1)));
            }
            other => panic!("an IME preedit must be delivered, got {other:?}"),
        }
    }

    /// Committed text arrives whole; the host decides where and how it edits.
    #[test]
    fn ime_commit_carries_the_full_string() {
        let mut state = state(1.0);
        match translate(&mut state, WINDOW, &ime(Ime::Commit("あ不".into()))) {
            Some(WindowOutput::ImeCommit { window_id, text }) => {
                assert_eq!(window_id, WINDOW);
                assert_eq!(text, "あ不");
            }
            other => panic!("an IME commit must be delivered, got {other:?}"),
        }
    }

    /// The session lifecycle is reported so the host can flip composition
    /// state on and off instead of guessing from empty preedits.
    #[test]
    fn ime_session_lifecycle_is_delivered() {
        let mut state = state(1.0);
        match translate(&mut state, WINDOW, &ime(Ime::Enabled)) {
            Some(WindowOutput::ImeEnabled { window_id }) => assert_eq!(window_id, WINDOW),
            other => panic!("Ime::Enabled must emit ImeEnabled, got {other:?}"),
        }
        match translate(&mut state, WINDOW, &ime(Ime::Disabled)) {
            Some(WindowOutput::ImeDisabled { window_id }) => assert_eq!(window_id, WINDOW),
            other => panic!("Ime::Disabled must emit ImeDisabled, got {other:?}"),
        }
    }

    /// Shift arrives on its own event, and has to still be attached to the key
    /// it modifies. Reverse focus traversal is the only reason Instar asks.
    #[test]
    fn shift_is_remembered_from_its_own_event_until_a_key_arrives() {
        let mut state = state(1.0);
        assert!(
            !state.on_key(Key::Tab, true, false).shift,
            "nothing is held to begin with"
        );

        translate(
            &mut state,
            WINDOW,
            &WindowEvent::ModifiersChanged(winit::keyboard::ModifiersState::SHIFT.into()),
        );
        assert!(
            state.on_key(Key::Tab, true, false).shift,
            "shift-tab must reach the host as shifted, or focus only ever \
             traverses forwards"
        );

        translate(
            &mut state,
            WINDOW,
            &WindowEvent::ModifiersChanged(winit::keyboard::ModifiersState::empty().into()),
        );
        assert!(
            !state.on_key(Key::Tab, true, false).shift,
            "and releasing it must be noticed"
        );
    }

    /// Both directions are delivered: a button held with Space is
    /// pressed-looking until the release says otherwise.
    #[test]
    fn a_key_release_is_carried_as_a_release() {
        let state = state(1.0);
        assert!(state.on_key(Key::Space, true, false).pressed);
        assert!(!state.on_key(Key::Space, false, false).pressed);
        assert!(
            state.on_key(Key::Space, true, true).repeat,
            "autorepeat is carried, because what it means is the host's call"
        );
    }

    /// The direction, which is the half of this a test can be silently wrong
    /// about.
    ///
    /// winit's positive Y means the content should move down, revealing what is
    /// *above* it. Instar counts offset from the content's origin, so revealing
    /// what is above is a decrease. Wheeling away from you must arrive negative.
    #[test]
    fn wheeling_away_from_you_reveals_what_is_above() {
        let mut state = state(1.0);

        let away = scrolled(&mut state, MouseScrollDelta::LineDelta(0.0, 1.0));
        assert_eq!(
            away.delta,
            ScrollDelta::Lines { x: 0.0, y: -1.0 },
            "winit's positive Y reveals content above, which is a negative Instar \
             offset delta"
        );

        let toward = scrolled(&mut state, MouseScrollDelta::LineDelta(0.0, -1.0));
        assert_eq!(
            toward.delta,
            ScrollDelta::Lines { x: 0.0, y: 1.0 },
            "and the opposite direction is the opposite sign, not a clamp"
        );
    }

    /// Lines are a count and pixels are a distance, so only one of them is a
    /// scale-factor question.
    #[test]
    fn only_the_pixel_delta_is_converted_for_scale() {
        let mut state = state(2.0);

        let lines = scrolled(&mut state, MouseScrollDelta::LineDelta(0.0, 3.0));
        assert_eq!(
            lines.delta,
            ScrollDelta::Lines { x: 0.0, y: -3.0 },
            "three notches are three notches at any scale factor"
        );

        let pixels = scrolled(
            &mut state,
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 60.0)),
        );
        assert_eq!(
            pixels.delta,
            ScrollDelta::Logical { x: 0.0, y: -30.0 },
            "60 physical pixels at scale 2 is 30 logical"
        );

        assert_eq!(
            pixels.logical_pos,
            LogicalPoint { x: 20.0, y: 40.0 },
            "and the scroll happens where the cursor is, in logical space"
        );
    }

    /// Horizontal survives the trip too, and is not quietly folded into vertical.
    #[test]
    fn the_horizontal_axis_is_carried_independently() {
        let mut state = state(1.0);
        let diagonal = scrolled(&mut state, MouseScrollDelta::LineDelta(2.0, 5.0));
        assert_eq!(diagonal.delta, ScrollDelta::Lines { x: -2.0, y: -5.0 });
    }

    /// A wheel with nowhere to be is not an event.
    #[test]
    fn a_wheel_before_the_cursor_is_known_is_dropped() {
        let mut fresh = WindowState::new(
            WINDOW,
            1.0,
            PhysicalSize {
                width: 400,
                height: 300,
            },
        );
        assert!(
            translate(
                &mut fresh,
                WINDOW,
                &wheel(MouseScrollDelta::LineDelta(0.0, 1.0))
            )
            .is_none(),
            "inventing a position would scroll whatever sits at the origin"
        );
    }

    /// The wrong-window guard applies to wheels like everything else, because it
    /// is this window's scale factor that would otherwise be used.
    #[test]
    fn a_wheel_for_another_window_is_not_translated() {
        let mut state = state(2.0);
        assert!(
            translate(
                &mut state,
                WindowId::from_raw(2),
                &wheel(MouseScrollDelta::LineDelta(0.0, 1.0))
            )
            .is_none()
        );
    }
}
