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

use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::window::WindowId;

use crate::{PhysicalSize, PointerButton, PointerState, WindowOutput, WindowState};

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

        // Winit's documented way to track runtime DPI changes. The new scale
        // is stored before this returns, so the next pointer event converts
        // with it -- see `WindowState::on_scale_factor_changed`.
        WindowEvent::ScaleFactorChanged {
            scale_factor,
            inner_size_writer: _,
        } => {
            // Winit hands the new *scale* here but communicates the new size
            // through a writer rather than a value. Keeping the current
            // physical size means the metrics stay self-consistent; the
            // `Resized` that follows carries the authoritative size.
            let physical = state.physical_size();
            Some(WindowOutput::MetricsChanged(
                state.on_scale_factor_changed(*scale_factor, physical),
            ))
        }

        WindowEvent::CursorMoved { position, .. } => {
            state.on_cursor_moved(position.x, position.y);
            // A move is not itself a pointer *event* in Instar's model: only
            // presses and releases are. The position is recorded for whichever
            // button event comes next. Hover and drag arrive with the
            // interaction state that needs them.
            None
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

        _ => None,
    }
}
