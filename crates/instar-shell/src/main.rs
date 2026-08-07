//! `instar` — the desktop shell (WP7B2).
//!
//! ```text
//! MAIN THREAD                                   RUNTIME THREAD
//! winit EventLoop                               instar-kernel
//!   │                                             │
//!   ├─ WindowEvent ── translate ── HostBridge ────┼──> guest wakes
//!   │                                             │
//!   ├─ user_event <─── EventLoopProxy ────────────┴─── guest commits
//!   │       └─ pump: screen, decode, validate, apply, lay out,
//!   │                lower to a scene, reply, request_redraw
//!   │
//!   └─ RedrawRequested ── Vello CPU ── pack ── softbuffer present
//! ```
//!
//! Everything the loop does with a message is `instar-host`'s; everything it
//! does with a pixel is this crate's. The file is mostly wiring, and the two
//! places it is not are marked.

use std::sync::Arc;

use instar_host::HostEffect;
use instar_host::bridge::{HostBridge, Wake};
use instar_shell::{Presenter, default_font};
use instar_window::{WindowOutput, WindowState, winit_adapter};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes};

/// The component the shell runs, built from source by `build.rs`.
const COUNTER: &[u8] = include_bytes!(env!("COUNTER_WASM"));

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `with_user_event` is what makes the proxy able to wake a loop parked in
    // `Wait`. Without it the runtime thread could queue a commit and the
    // window would sit there until the user happened to move the mouse.
    let event_loop = EventLoop::<()>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut shell = Shell::new(event_loop.create_proxy());
    event_loop.run_app(&mut shell)?;

    // Joins the runtime thread. Dropping would do it too — `RuntimeThread`'s
    // `Drop` shuts the guest down — but doing it explicitly means a guest that
    // refuses to leave is a visible hang here rather than a mystery at exit.
    shell.shutdown();
    Ok(())
}

struct Shell {
    proxy: EventLoopProxy<()>,
    /// `Arc` because softbuffer's surface holds the window too, and winit's
    /// `Window` is not `Clone`.
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    state: Option<WindowState>,
    bridge: Option<HostBridge>,
    presenter: Option<Presenter>,
}

impl Shell {
    fn new(proxy: EventLoopProxy<()>) -> Self {
        Self {
            proxy,
            window: None,
            surface: None,
            state: None,
            bridge: None,
            presenter: None,
        }
    }

    fn shutdown(&mut self) {
        if let Some(bridge) = self.bridge.as_mut() {
            bridge.shutdown();
        }
    }

    /// Applies what the host decided. The only interesting case is `Render`;
    /// the rest is bookkeeping.
    fn apply(&mut self, effects: Vec<HostEffect>, event_loop: &ActiveEventLoop) {
        for effect in effects {
            match effect {
                HostEffect::Render { .. } => {
                    // Ask winit for a frame rather than drawing here. Drawing
                    // straight from an event handler would present several
                    // times for one logical change — a click alone produces a
                    // press frame, a release frame, and a commit frame — and
                    // the compositor only ever shows the last.
                    if let Some(window) = self.window.as_ref() {
                        window.request_redraw();
                    }
                }
                HostEffect::GuestGone { generation, error } => match error {
                    Some(error) => eprintln!("instar: {generation} trapped: {error}"),
                    // A guest that returned from `run` is done, and so is the
                    // application: there is nothing left to drive the window.
                    None => {
                        eprintln!("instar: {generation} exited");
                        event_loop.exit();
                    }
                },
                HostEffect::Exit => event_loop.exit(),
                // Consumed by the bridge on its way to the runtime thread.
                HostEffect::SendToGuest(_) => {}
            }
        }
    }

    /// Draws the host's current scene into the window.
    ///
    /// Nothing here decides *what* to draw. If the host has no presentable
    /// scene the frame is skipped entirely — that is the metrics barrier
    /// arriving at the last layer that could ignore it, and the window keeps
    /// whatever it was showing rather than being cleared to something
    /// arbitrary.
    fn redraw(&mut self) {
        let (Some(bridge), Some(surface), Some(presenter)) = (
            self.bridge.as_ref(),
            self.surface.as_mut(),
            self.presenter.as_mut(),
        ) else {
            return;
        };
        let window = bridge.window();
        let Some(scene) = bridge.host().window(window).and_then(|w| w.scene()) else {
            return;
        };

        let (Some(width), Some(height)) = (
            std::num::NonZeroU32::new(scene.size.width),
            std::num::NonZeroU32::new(scene.size.height),
        ) else {
            // A zero-area window is normal while minimizing on some platforms.
            return;
        };
        if let Err(error) = surface.resize(width, height) {
            eprintln!("instar: could not size the window surface: {error}");
            return;
        }

        let mut buffer = match surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(error) => {
                eprintln!("instar: could not acquire the window buffer: {error}");
                return;
            }
        };

        // The one rule this function exists to keep: on any error the buffer
        // may hold a torn mix of two frames, so it is dropped unpresented.
        // A stale frame is a far smaller problem than a half-drawn one.
        match presenter.render_into_window(scene, &mut buffer) {
            Ok(()) => {
                if let Err(error) = buffer.present() {
                    eprintln!("instar: could not present: {error}");
                }
            }
            Err(error) => eprintln!("instar: frame dropped: {error}"),
        }
    }

    /// Drains the runtime thread's queue and acts on what it says.
    fn pump(&mut self, event_loop: &ActiveEventLoop) {
        let Some(bridge) = self.bridge.as_mut() else {
            return;
        };
        let effects = bridge.pump();
        self.apply(effects, event_loop);
    }
}

impl ApplicationHandler<()> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attributes = WindowAttributes::default()
            .with_title("Instar")
            .with_inner_size(winit::dpi::LogicalSize::new(480.0, 320.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("instar: could not create a window: {error}");
                event_loop.exit();
                return;
            }
        };

        let physical: instar_window::PhysicalSize = window.inner_size().into();
        let state = WindowState::new(window.id().into(), window.scale_factor(), physical);

        // The proxy is the whole reason two threads are workable: it is
        // `Send + Sync` where `EventLoop` deliberately is not, so the runtime
        // thread can say "look at your queue" without touching anything winit
        // owns. The payload travels on the bridge's own channel; this only has
        // to make the loop wake up.
        let proxy = self.proxy.clone();
        let wake: Wake = Arc::new(move || {
            let _ = proxy.send_event(());
        });

        let started = match default_font() {
            Ok(font) => HostBridge::spawn_with_glyphs(
                COUNTER.to_vec(),
                state.window_id(),
                wake,
                Arc::new(font),
            ),
            Err(error) => {
                // Survivable, and worth surviving: every rectangle still
                // renders, so a broken font gives a usable window with no
                // labels rather than no window at all.
                eprintln!("instar: no text this session ({error})");
                HostBridge::spawn(COUNTER.to_vec(), state.window_id(), wake)
            }
        };
        let mut bridge = match started {
            Ok(bridge) => bridge,
            Err(error) => {
                eprintln!("instar: the guest did not start: {error}");
                event_loop.exit();
                return;
            }
        };

        let window = Arc::new(window);
        match softbuffer::Context::new(Arc::clone(&window))
            .and_then(|context| softbuffer::Surface::new(&context, Arc::clone(&window)))
        {
            Ok(surface) => self.surface = Some(surface),
            Err(error) => {
                eprintln!("instar: could not attach a surface to the window: {error}");
                event_loop.exit();
                return;
            }
        }

        self.presenter = match Presenter::new(instar_paint::PhysicalSize {
            width: physical.width,
            height: physical.height,
        }) {
            Ok(presenter) => Some(presenter),
            Err(error) => {
                eprintln!("instar: could not start the renderer: {error}");
                event_loop.exit();
                return;
            }
        };

        // The host needs geometry before it can lay anything out, and no
        // resize is guaranteed to arrive first.
        let effects = bridge.on_window_event(WindowOutput::MetricsChanged(state.metrics()));

        self.bridge = Some(bridge);
        self.state = Some(state);
        self.window = Some(window);
        self.apply(effects, event_loop);
    }

    /// The runtime thread queued something. The event carries no payload —
    /// the bridge's channel does — so this just means "look".
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: ()) {
        self.pump(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        if matches!(event, WindowEvent::RedrawRequested) {
            self.redraw();
            return;
        }

        let (Some(state), Some(bridge)) = (self.state.as_mut(), self.bridge.as_mut()) else {
            return;
        };
        let Some(output) = winit_adapter::translate(state, id.into(), &event) else {
            return;
        };
        let effects = bridge.on_window_event(output);
        self.apply(effects, event_loop);
    }

    /// End of the event cycle: flush metrics left pending by a scale change.
    ///
    /// By now winit has applied the OS-suggested size for the new scale, so
    /// `inner_size()` is coherent with the scale factor recorded during the
    /// change. This is what closes the metrics barrier on platforms that send
    /// no separate `Resized`.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(state), Some(window), Some(bridge)) = (
            self.state.as_mut(),
            self.window.as_ref(),
            self.bridge.as_mut(),
        ) else {
            return;
        };
        let Some(metrics) = state.take_pending_metrics(window.inner_size().into()) else {
            return;
        };
        let effects = bridge.on_window_event(WindowOutput::MetricsChanged(metrics));
        self.apply(effects, event_loop);
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.shutdown();
    }
}
