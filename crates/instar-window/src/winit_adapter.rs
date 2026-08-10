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

use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};

use crate::{
    PhysicalSize, PointerButton, PointerState, RawPointerMoved, ScrollDelta, WindowId,
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
            None
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

        _ => None,
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
    use winit::event::{DeviceId, MouseScrollDelta, TouchPhase};

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
        match translate(
            &mut state,
            WINDOW,
            &WindowEvent::MouseInput {
                device_id: DeviceId::dummy(),
                state: winit::event::ElementState::Pressed,
                button: winit::event::MouseButton::Left,
            },
        ) {
            Some(WindowOutput::Pointer(event)) => {
                assert_eq!(event.logical_pos, LogicalPoint { x: 40.0, y: 20.0 })
            }
            other => panic!("expected a press, got {other:?}"),
        }
    }

    /// The wrong-window guard applies here too: this window's scale factor
    /// would otherwise convert another window's coordinates.
    #[test]
    fn a_move_for_another_window_is_not_translated() {
        let mut state = state(2.0);
        assert!(translate(&mut state, WindowId::from_raw(2), &moved(100.0, 60.0)).is_none());
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
