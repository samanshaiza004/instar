//! Window creation and the event loop.
//!
//! Like [`crate::winit_adapter`], this is plumbing: it owns no policy beyond
//! "wait for events rather than spinning". It is not exercised by the headless
//! test suite, because an event loop needs a display server.

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::{PhysicalSize, WindowOutput, WindowState, winit_adapter};

/// Drives one window, handing translated events to a callback.
pub struct WindowDriver<F> {
    attributes: WindowAttributes,
    window: Option<Window>,
    state: Option<WindowState>,
    on_event: F,
}

impl<F: FnMut(WindowOutput)> WindowDriver<F> {
    pub fn new(attributes: WindowAttributes, on_event: F) -> Self {
        Self {
            attributes,
            window: None,
            state: None,
            on_event,
        }
    }

    /// Runs until the event loop exits.
    ///
    /// Sets [`ControlFlow::Wait`], which is the entire reason this function
    /// exists rather than being left to callers. Instar's premise is that an
    /// idle application costs nothing — Gate 0 proved the guest side of that,
    /// and `Poll` here would give it all back by spinning the main thread
    /// through the event loop as fast as the OS allows, whether or not
    /// anything happened.
    pub fn run(mut self) -> Result<(), winit::error::EventLoopError> {
        let event_loop = EventLoop::new()?;
        event_loop.set_control_flow(ControlFlow::Wait);
        event_loop.run_app(&mut self)
    }

    pub fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }
}

impl<F: FnMut(WindowOutput)> ApplicationHandler for WindowDriver<F> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = match event_loop.create_window(self.attributes.clone()) {
            Ok(window) => window,
            Err(error) => {
                event_loop.exit();
                // Creating the only window is the one failure with nothing to
                // fall back to, so exit rather than run a loop that can never
                // show anything.
                eprintln!("instar-window: could not create a window: {error}");
                return;
            }
        };

        let size: PhysicalSize = window.inner_size().into();
        let state = WindowState::new(window.id(), window.scale_factor(), size);

        // The host needs geometry before it can lay anything out, and no
        // resize is guaranteed to arrive first.
        (self.on_event)(WindowOutput::MetricsChanged(state.metrics()));

        self.state = Some(state);
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        let close_requested = matches!(event, WindowEvent::CloseRequested);
        if let Some(output) = winit_adapter::translate(state, id, &event) {
            (self.on_event)(output);
        }

        // Closing is the host's decision in principle, but with a single
        // window and no host yet there is nothing left to run. WP7 moves this
        // decision up where it belongs.
        if close_requested {
            event_loop.exit();
        }
    }
}
