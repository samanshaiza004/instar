//! `instar` — the desktop shell.
//!
//! ```text
//! instar run path/to/app.component.wasm [--debug]
//! ```
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
//! does with a pixel is this crate's.
//!
//! # Why there is exactly one command
//!
//! `run` has one useful job: prove the host can load an *arbitrary* component
//! implementing the current experimental world. Before this, the counter was
//! compiled into the binary, and "Instar runs a guest" was true only of the
//! guest Instar was built with.
//!
//! Everything else a CLI usually has — `new`, `build`, `dev`, `package`,
//! `inspect`, `validate`, `doctor` — is deliberately absent. Each of those
//! freezes assumptions about manifests, service discovery, build systems,
//! package layout, SDKs, compatibility, and distribution, and none of those
//! things have been learned yet. A command added now would be a guess
//! preserved as an interface.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use instar_host::HostEffect;
use instar_host::bridge::{HostBridge, Wake};
use instar_shell::{Presenter, default_font};
use instar_window::{WindowOutput, WindowState, winit_adapter};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};

mod accessibility;
use accessibility::{Accessibility, Request as A11yRequest, UpdateSink};

/// Everything that reaches the loop from somewhere other than the window.
///
/// Two senders, one queue. The runtime thread's variant carries nothing -- the
/// bridge's channel carries the payload -- while AccessKit's carries the
/// request it wants answered on this thread.
#[derive(Debug)]
enum ShellEvent {
    /// The guest queued something. Look at the bridge.
    Runtime,
    /// A platform accessibility request, forwarded off whatever thread the
    /// platform adapter raised it on.
    Accessibility(accesskit_winit::Event),
}

impl From<accesskit_winit::Event> for ShellEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        Self::Accessibility(event)
    }
}

/// A native window and the one accessibility adapter that describes it.
///
/// One field rather than two `Option`s, because the requirement is that
/// exactly one adapter exists per native window and never outlives it. Two
/// options can drift apart -- one cleared, one not, or one set a few lines
/// before the other and a fallible step in between. This cannot: there is no
/// state in which a window exists without its adapter, and dropping the pair
/// drops both.
struct NativeWindow {
    /// `Arc` because softbuffer's surface holds the window too, and winit's
    /// `Window` is not `Clone`.
    window: Arc<Window>,
    adapter: accesskit_winit::Adapter,
}

/// The real sink: one `update_if_active` call, and nothing else.
///
/// This is the smallest thing that cannot be tested without a desktop and an
/// assistive technology attached, which is why it is the only thing here.
struct AdapterSink<'a>(&'a mut accesskit_winit::Adapter);

impl UpdateSink for AdapterSink<'_> {
    fn send(&mut self, update: accesskit::TreeUpdate) {
        self.0.update_if_active(|| update);
    }
}
use winit::window::{Window, WindowAttributes};

const USAGE: &str = "\
instar — a native host for WebAssembly application components

USAGE:
    instar run <component.wasm> [--debug]

ARGS:
    <component.wasm>    A component implementing instar:kernel/kernel

OPTIONS:
    --debug             Report lifecycle, commits, and frame timings on stderr
    -h, --help          Print this message

`run` is the only command. new/build/dev/package/inspect/validate/doctor do
not exist yet, deliberately: each would freeze assumptions about manifests,
build systems, package layout, SDKs, and distribution that this project has
not learned yet.
";

fn main() -> std::process::ExitCode {
    match Args::parse(std::env::args().skip(1)) {
        Ok(Some(args)) => match run(args) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("instar: {error}");
                std::process::ExitCode::FAILURE
            }
        },
        // --help: asked for, so it is not an error.
        Ok(None) => {
            print!("{USAGE}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("instar: {error}\n");
            eprint!("{USAGE}");
            std::process::ExitCode::from(2)
        }
    }
}

#[derive(Debug)]
struct Args {
    component: PathBuf,
    debug: bool,
}

impl Args {
    /// Hand-parsed. One command and one flag does not justify an argument
    /// parser, and a CLI framework's conventions are themselves assumptions
    /// about a surface that does not exist yet.
    fn parse(args: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut component: Option<PathBuf> = None;
        let mut debug = false;
        let mut command: Option<String> = None;

        for arg in args {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--debug" => debug = true,
                other if other.starts_with('-') => {
                    return Err(format!("unknown option {other:?}"));
                }
                other if command.is_none() => command = Some(other.to_string()),
                other if component.is_none() => component = Some(PathBuf::from(other)),
                other => return Err(format!("unexpected argument {other:?}")),
            }
        }

        match command.as_deref() {
            None => Err("no command given".to_string()),
            Some("run") => match component {
                Some(component) => Ok(Some(Self { component, debug })),
                None => Err("run needs the path to a component".to_string()),
            },
            // Named rather than lumped in with a typo: someone typing
            // `instar build` has a reasonable expectation, and the useful
            // answer is why it is missing rather than "unknown command".
            Some(
                other @ ("new" | "build" | "dev" | "package" | "inspect" | "validate" | "doctor"),
            ) => Err(format!(
                "`{other}` does not exist yet, deliberately -- it would freeze \
                 assumptions this project has not learned. `run` is the only command"
            )),
            Some(other) => Err(format!("unknown command {other:?}")),
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let component = load(&args.component)?;
    if args.debug {
        eprintln!(
            "instar: loaded {} ({} bytes)",
            args.component.display(),
            component.len()
        );
    }

    // `with_user_event` is what makes the proxy able to wake a loop parked in
    // `Wait`. Without it the runtime thread could queue a commit and the
    // window would sit there until the user happened to move the mouse.
    let event_loop = EventLoop::<ShellEvent>::with_user_event()
        .build()
        .map_err(|error| format!("could not start an event loop: {error}"))?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut shell = Shell::new(event_loop.create_proxy(), component, args.debug);
    let result = event_loop
        .run_app(&mut shell)
        .map_err(|error| format!("the event loop failed: {error}"));

    // Joins the runtime thread. Dropping would do it too — `RuntimeThread`'s
    // `Drop` shuts the guest down — but doing it explicitly means a guest that
    // refuses to leave is a visible hang here rather than a mystery at exit.
    shell.shutdown();
    result?;
    shell.startup_error.map_or(Ok(()), Err)
}

/// Reads the component, distinguishing the failures a user can act on.
fn load(path: &Path) -> Result<Vec<u8>, String> {
    match std::fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Err(format!("{} is empty", path.display())),
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("no such file: {}", path.display()))
        }
        Err(_) if path.is_dir() => Err(format!(
            "{} is a directory; pass the .wasm component itself",
            path.display()
        )),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

struct Shell {
    proxy: EventLoopProxy<ShellEvent>,
    component: Vec<u8>,
    debug: bool,
    /// Set when startup fails inside `resumed`, where there is nothing to
    /// return an error *to*. Reported after the loop ends, so a shell that
    /// could not start exits non-zero rather than looking like a clean run.
    startup_error: Option<String>,
    native: Option<NativeWindow>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    /// Whether anything is listening, and the rules that follow from it.
    a11y: Accessibility,
    state: Option<WindowState>,
    bridge: Option<HostBridge>,
    presenter: Option<Presenter>,
}

impl Shell {
    fn new(proxy: EventLoopProxy<ShellEvent>, component: Vec<u8>, debug: bool) -> Self {
        Self {
            proxy,
            component,
            debug,
            startup_error: None,
            native: None,
            surface: None,
            a11y: Accessibility::default(),
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

    /// Records a fatal startup problem and ends the loop.
    fn fail(&mut self, error: String, event_loop: &ActiveEventLoop) {
        self.startup_error.get_or_insert(error);
        event_loop.exit();
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
                    if let Some(native) = self.native.as_ref() {
                        native.window.request_redraw();
                    }
                }
                HostEffect::GuestGone { generation, error } => match error {
                    // Not fatal to the shell: the host owns the window now and
                    // is showing the crash surface. Exiting here would replace
                    // a visible explanation with a vanished window.
                    Some(error) => eprintln!("instar: {generation} trapped: {error}"),
                    // A guest that returned from `run` is done, and so is the
                    // application: there is nothing left to drive the window.
                    None => {
                        if self.debug {
                            eprintln!("instar: {generation} exited");
                        }
                        event_loop.exit();
                    }
                },
                HostEffect::Exit => event_loop.exit(),
                // Consumed by the bridge on its way to the runtime thread.
                HostEffect::SendToGuest(_) => {}
            }
        }
        self.flush_accessibility();
    }

    /// Tells the platform what changed, if anything, and if anyone is asking.
    ///
    /// Every path that touches host state ends in `apply`, so this is the one
    /// place the question has to be asked. `Accessibility::flush` decides
    /// whether to ask the bridge at all; see that module for why the order
    /// matters.
    fn flush_accessibility(&mut self) {
        let (Some(native), Some(bridge)) = (self.native.as_mut(), self.bridge.as_mut()) else {
            return;
        };
        self.a11y.flush(
            || bridge.accessibility_update(),
            &mut AdapterSink(&mut native.adapter),
        );
    }

    /// A platform accessibility request, now safely on the main thread.
    /// A platform accessibility request, now safely on the main thread.
    ///
    /// Under `--debug` this narrates what the platform actually asked for. It
    /// exists for the F4 manual pass: AccessKit defines a broad cross-platform
    /// action vocabulary, but which requests a given screen reader generates
    /// for a given gesture is the *native adapter's* business, and guessing at
    /// it is how a smoke test comes to demand a sequence no platform produces.
    /// So observe first. See `docs/F4-SMOKE.md`.
    fn on_accessibility(&mut self, event: accesskit_winit::Event, event_loop: &ActiveEventLoop) {
        match self.a11y.classify(event.window_event) {
            A11yRequest::SendFullTree => {
                if self.debug {
                    eprintln!("instar: a11y attached, sending the whole tree");
                }
                let (Some(native), Some(bridge)) = (self.native.as_mut(), self.bridge.as_mut())
                else {
                    return;
                };
                // Not `flush_accessibility`: an adapter that has just attached
                // holds nothing, so a diff would describe changes to a tree it
                // does not have.
                if let Some(update) = bridge.full_accessibility_tree() {
                    AdapterSink(&mut native.adapter).send(update);
                }
            }
            A11yRequest::Forward { action, target } => {
                let debug = self.debug;
                let Some(bridge) = self.bridge.as_mut() else {
                    return;
                };
                let before = bridge.host().interaction_stats();
                let effects = bridge.on_accessibility_action(action, target);

                if debug {
                    // Which canonical operation this entered, read from the
                    // F3 counters. Instrumentation, not behaviour: the
                    // counters describe what happened, and nothing branches
                    // on them.
                    let after = bridge.host().interaction_stats();
                    let entered = if after.activate > before.activate {
                        "Activate"
                    } else if after.focus > before.focus {
                        "Focus"
                    } else if after.blur > before.blur {
                        "Blur"
                    } else if after.reveal > before.reveal {
                        "Reveal"
                    } else {
                        "nothing"
                    };
                    let key = instar_ui::NodeKey::from_accesskit_id(target.0);
                    eprintln!(
                        "instar: a11y {action:?} node {} -> id {} gen {} -> entered {entered}",
                        target.0, key.id, key.generation,
                    );
                }

                // Through `apply` like everything else, which is also what
                // flushes whatever the action just changed back out.
                self.apply(effects, event_loop);
            }
            A11yRequest::Nothing => {
                if self.debug {
                    eprintln!("instar: a11y detached");
                }
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
        let debug = self.debug;
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
        let started = Instant::now();
        match presenter.render_into_window(scene, &mut buffer) {
            Ok(()) => {
                if let Err(error) = buffer.present() {
                    eprintln!("instar: could not present: {error}");
                } else if debug {
                    eprintln!(
                        "instar: frame {}x{} in {:.2}ms (revision {})",
                        scene.size.width,
                        scene.size.height,
                        started.elapsed().as_secs_f64() * 1000.0,
                        bridge.tree_revision(),
                    );
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
        if self.debug {
            let stats = bridge.stats();
            if stats.rejected_commits > 0 || stats.stale_commits > 0 || stats.dropped_commands > 0 {
                eprintln!(
                    "instar: applied {} rejected {} stale {} dropped {}",
                    stats.applied_commits,
                    stats.rejected_commits,
                    stats.stale_commits,
                    stats.dropped_commands,
                );
            }
        }
        self.apply(effects, event_loop);
    }
}

impl ApplicationHandler<ShellEvent> for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.native.is_some() {
            return;
        }

        // Invisible to begin with: `accesskit_winit` requires the adapter to
        // exist before the window is first shown, and panics otherwise.
        let attributes = WindowAttributes::default()
            .with_title("Instar")
            .with_visible(false)
            .with_inner_size(winit::dpi::LogicalSize::new(480.0, 320.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => window,
            Err(error) => {
                return self.fail(format!("could not create a window: {error}"), event_loop);
            }
        };

        // One adapter, this window, before it is shown. `with_event_loop_proxy`
        // rather than `with_mixed_handlers`: the mixed constructor wants an
        // activation handler that is `Send` and may be called on any thread,
        // and answering it means reading the retained tree. Routing activation
        // through the proxy costs a placeholder tree for one frame and buys the
        // rule that no platform callback ever touches host state.
        let adapter = accesskit_winit::Adapter::with_event_loop_proxy(
            event_loop,
            &window,
            self.proxy.clone(),
        );
        // The pair is built here, in one step, and only shown once both exist.
        // Nothing fallible runs between the adapter and the window becoming
        // visible, so there is no path on which a visible window lacks one.
        let native = NativeWindow {
            window: Arc::new(window),
            adapter,
        };
        native.window.set_visible(true);
        let window = Arc::clone(&native.window);
        self.native = Some(native);

        let physical: instar_window::PhysicalSize = window.inner_size().into();
        let state = WindowState::new(window.id().into(), window.scale_factor(), physical);

        // The proxy is the whole reason two threads are workable: it is
        // `Send + Sync` where `EventLoop` deliberately is not, so the runtime
        // thread can say "look at your queue" without touching anything winit
        // owns. The payload travels on the bridge's own channel; this only has
        // to make the loop wake up.
        let proxy = self.proxy.clone();
        let wake: Wake = Arc::new(move || {
            let _ = proxy.send_event(ShellEvent::Runtime);
        });

        let component = self.component.clone();
        let started = HostBridge::spawn_with_monospace_face(
            component,
            state.window_id(),
            wake,
            default_font(),
        );
        let mut bridge = match started {
            Ok(bridge) => bridge,
            Err(error) => {
                return self.fail(
                    format!(
                        "the guest did not start: {error}\n       \
                         the component must implement instar:kernel/kernel \
                         (see docs/PROTOCOL-0.md)"
                    ),
                    event_loop,
                );
            }
        };
        if self.debug {
            eprintln!("instar: {} started", bridge.generation());
        }

        match softbuffer::Context::new(Arc::clone(&window))
            .and_then(|context| softbuffer::Surface::new(&context, Arc::clone(&window)))
        {
            Ok(surface) => self.surface = Some(surface),
            Err(error) => {
                return self.fail(
                    format!("could not attach a surface to the window: {error}"),
                    event_loop,
                );
            }
        }

        self.presenter = match Presenter::new(instar_paint::PhysicalSize {
            width: physical.width,
            height: physical.height,
        }) {
            Ok(presenter) => Some(presenter),
            Err(error) => {
                return self.fail(format!("could not start the renderer: {error}"), event_loop);
            }
        };

        // The host needs geometry before it can lay anything out, and no
        // resize is guaranteed to arrive first.
        let effects = bridge.on_window_event(WindowOutput::MetricsChanged(state.metrics()));

        self.bridge = Some(bridge);
        self.state = Some(state);
        self.apply(effects, event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: ShellEvent) {
        match event {
            // The runtime thread queued something. The event carries no
            // payload — the bridge's channel does — so this just means "look".
            ShellEvent::Runtime => self.pump(event_loop),
            ShellEvent::Accessibility(event) => self.on_accessibility(event, event_loop),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        // Before anything else, as `accesskit_winit` requires: the adapter
        // tracks focus and geometry from the raw stream.
        if let Some(native) = self.native.as_mut() {
            native.adapter.process_event(&native.window, &event);
        }

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
        if self.debug
            && let WindowOutput::MetricsChanged(metrics) = &output
        {
            eprintln!(
                "instar: metrics {}x{} logical, {}x{} physical, scale {}",
                metrics.logical_size.width,
                metrics.logical_size.height,
                metrics.physical_size.width,
                metrics.physical_size.height,
                metrics.scale_factor,
            );
        }
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
        let (Some(state), Some(native), Some(bridge)) = (
            self.state.as_mut(),
            self.native.as_ref(),
            self.bridge.as_mut(),
        ) else {
            return;
        };
        let Some(metrics) = state.take_pending_metrics(native.window.inner_size().into()) else {
            return;
        };
        let effects = bridge.on_window_event(WindowOutput::MetricsChanged(metrics));
        self.apply(effects, event_loop);
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Args>, String> {
        Args::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn run_takes_a_component_path() {
        let args = parse(&["run", "app.wasm"])
            .expect("valid")
            .expect("not --help");
        assert_eq!(args.component, PathBuf::from("app.wasm"));
        assert!(!args.debug);
    }

    #[test]
    fn debug_is_accepted_on_either_side_of_the_path() {
        for form in [
            vec!["run", "app.wasm", "--debug"],
            vec!["run", "--debug", "app.wasm"],
            vec!["--debug", "run", "app.wasm"],
        ] {
            let args = parse(&form)
                .unwrap_or_else(|error| panic!("{form:?} should parse: {error}"))
                .expect("not --help");
            assert!(args.debug, "{form:?} should set debug");
            assert_eq!(args.component, PathBuf::from("app.wasm"));
        }
    }

    #[test]
    fn help_is_not_an_error() {
        assert!(parse(&["--help"]).expect("valid").is_none());
        assert!(parse(&["-h"]).expect("valid").is_none());
    }

    #[test]
    fn run_without_a_path_says_so() {
        let error = parse(&["run"]).unwrap_err();
        assert!(error.contains("needs the path"), "unhelpful: {error}");
    }

    /// Someone typing `instar build` has a reasonable expectation. The useful
    /// answer is why it is missing, not "unknown command".
    #[test]
    fn the_commands_that_do_not_exist_yet_explain_themselves() {
        for command in [
            "new", "build", "dev", "package", "inspect", "validate", "doctor",
        ] {
            let error = parse(&[command]).unwrap_err();
            assert!(
                error.contains("deliberately"),
                "{command} should explain itself, got: {error}"
            );
        }
    }

    #[test]
    fn an_unknown_command_is_refused() {
        assert!(parse(&["frobnicate"]).unwrap_err().contains("unknown"));
        assert!(parse(&["--nope"]).unwrap_err().contains("unknown option"));
    }

    #[test]
    fn no_arguments_is_an_error_rather_than_a_default() {
        // A bare `instar` used to run a compiled-in counter. It must not
        // silently do anything now: there is no default component.
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn a_missing_component_is_reported_as_such() {
        let error = load(Path::new("definitely/not/here.wasm")).unwrap_err();
        assert!(error.contains("no such file"), "unhelpful: {error}");
    }
}
