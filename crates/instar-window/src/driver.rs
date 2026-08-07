//! Window creation and the event loop.
//!
//! Like [`crate::winit_adapter`], this is plumbing: it owns no policy beyond
//! "wait for events rather than spinning". It is not exercised by the headless
//! test suite, because an event loop needs a display server.

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes};

use crate::{PhysicalSize, WindowOutput, WindowState, winit_adapter};

/// What the host wants done after seeing an event.
///
/// Close policy lives above this crate: winit leaves it to the application,
/// and a host may reasonably want to ask its guest first, ignore the request,
/// or close one window of several. `instar-window` reports that a close was
/// requested and does nothing about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDecision {
    /// Keep running.
    Continue,
    /// Exit the event loop.
    Exit,
}

/// Drives one window, handing translated events to a callback.
pub struct WindowDriver<F> {
    attributes: WindowAttributes,
    window: Option<Window>,
    state: Option<WindowState>,
    on_event: F,
}

impl<F: FnMut(WindowOutput) -> HostDecision> WindowDriver<F> {
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

impl<F: FnMut(WindowOutput) -> HostDecision> ApplicationHandler for WindowDriver<F> {
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
        let state = WindowState::new(window.id().into(), window.scale_factor(), size);

        // The host needs geometry before it can lay anything out, and no
        // resize is guaranteed to arrive first.
        if (self.on_event)(WindowOutput::MetricsChanged(state.metrics())) == HostDecision::Exit {
            event_loop.exit();
        }

        self.state = Some(state);
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        // Note what does *not* happen here: `CloseRequested` is translated and
        // reported like any other event, and nothing exits on its own. Only
        // the host's `HostDecision` closes this loop.
        if let Some(output) = winit_adapter::translate(state, id.into(), &event)
            && (self.on_event)(output) == HostDecision::Exit
        {
            event_loop.exit();
        }
    }

    /// End of the event cycle: flush metrics left pending by a scale change.
    ///
    /// By now winit has applied the OS-suggested size for the new scale, so
    /// `inner_size()` is coherent with the scale factor stored during the
    /// change. This is what makes the pair safe to publish even on a platform
    /// that sends no separate `Resized`.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(state), Some(window)) = (self.state.as_mut(), self.window.as_ref()) else {
            return;
        };

        if let Some(metrics) = state.take_pending_metrics(window.inner_size().into())
            && (self.on_event)(WindowOutput::MetricsChanged(metrics)) == HostDecision::Exit
        {
            event_loop.exit();
        }
    }
}
