use std::num::NonZeroU32;
use std::sync::Arc;

use softbuffer::{Context, Surface};
use thiserror::Error;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize as WinitLogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::{
    Controller, ControllerCommand, DesktopEvent, LogicalPoint, LogicalSize, Palette, PointerState,
    RenderState, RendererMirror, layout, render,
};
use youth_runtime::{RuntimeErrorCategory, YouthAppConfig, YouthAppHandle};
use youth_tree::{Node, NodeData, NodeId, TreeSnapshot};

#[derive(Clone, Debug)]
pub struct DesktopOptions {
    pub config: YouthAppConfig,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Error)]
pub enum DesktopError {
    #[error("invalid desktop arguments: {0}")]
    Arguments(String),
    #[error("native event loop could not be created: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),
}

enum NativeEvent {
    Mounted(YouthAppHandle, TreeSnapshot),
    Runtime(DesktopEvent),
    StartupFault(RuntimeErrorCategory),
}

enum Mode {
    Application(Option<YouthAppConfig>),
    Smoke,
}

struct NativeApp {
    mode: Mode,
    initial_size: (u32, u32),
    proxy: EventLoopProxy<NativeEvent>,
    window: Option<Arc<Window>>,
    context: Option<Context<Arc<Window>>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    mirror: Option<RendererMirror>,
    layout: Option<crate::LayoutSnapshot>,
    pointer: PointerState,
    controller: Option<Controller>,
    fault: Option<String>,
    smoke_presented: bool,
}

pub fn run(options: DesktopOptions) -> Result<(), DesktopError> {
    run_mode(
        Mode::Application(Some(options.config)),
        options.width,
        options.height,
    )
}

pub fn window_smoke() -> Result<(), DesktopError> {
    run_mode(Mode::Smoke, 320, 180)
}

fn run_mode(mode: Mode, width: u32, height: u32) -> Result<(), DesktopError> {
    let event_loop = EventLoop::<NativeEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = NativeApp {
        mode,
        initial_size: (width, height),
        proxy,
        window: None,
        context: None,
        surface: None,
        mirror: None,
        layout: None,
        pointer: PointerState::default(),
        controller: None,
        fault: None,
        smoke_presented: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl ApplicationHandler<NativeEvent> for NativeApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match &mut self.mode {
            Mode::Application(config) => {
                if let Some(config) = config.take() {
                    bootstrap(config, self.proxy.clone());
                }
            }
            Mode::Smoke if self.window.is_none() => {
                self.install_snapshot(smoke_snapshot());
                self.create_window(event_loop, "Youth window smoke");
            }
            Mode::Smoke => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: NativeEvent) {
        match event {
            NativeEvent::Mounted(handle, snapshot) => {
                self.install_snapshot(snapshot);
                let proxy = self.proxy.clone();
                self.controller = Some(Controller::spawn(handle, move |event| {
                    let _ = proxy.send_event(NativeEvent::Runtime(event));
                }));
                self.create_window(event_loop, "Youth");
            }
            NativeEvent::StartupFault(category) => {
                self.fault = Some(category_name(category).to_owned());
                self.create_window(event_loop, "Youth fault");
            }
            NativeEvent::Runtime(event) => self.runtime_event(event, event_loop),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                if let Some(controller) = &self.controller {
                    let _ = controller.send(ControllerCommand::Stop);
                }
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.present(event_loop),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.relayout();
                window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } if self.fault.is_none() => {
                let scale = window.scale_factor();
                if let Some(layout) = &self.layout {
                    let change = self.pointer.move_to(
                        LogicalPoint {
                            x: position.x / scale,
                            y: position.y / scale,
                        },
                        layout,
                    );
                    if change.redraw {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::CursorLeft { .. } if self.fault.is_none() => {
                if self.pointer.leave().redraw {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if self.fault.is_none() => {
                let change = match state {
                    ElementState::Pressed => self.pointer.press_primary(),
                    ElementState::Released => self
                        .layout
                        .as_ref()
                        .map_or_else(crate::InputChange::default, |layout| {
                            self.pointer.release_primary(layout)
                        }),
                };
                if let Some(node) = change.activation {
                    if let Some(controller) = &self.controller {
                        let _ = controller.send(ControllerCommand::Activate(node));
                    }
                }
                if change.redraw {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

impl NativeApp {
    fn create_window(&mut self, event_loop: &ActiveEventLoop, title: &str) {
        if self.window.is_some() {
            return;
        }
        let attributes = WindowAttributes::default()
            .with_title(title)
            .with_inner_size(WinitLogicalSize::new(
                f64::from(self.initial_size.0),
                f64::from(self.initial_size.1),
            ));
        let Ok(window) = event_loop.create_window(attributes) else {
            self.fault = Some("window_creation".to_owned());
            event_loop.exit();
            return;
        };
        let window = Arc::new(window);
        let Ok(context) = Context::new(window.clone()) else {
            self.fault = Some("surface_creation".to_owned());
            event_loop.exit();
            return;
        };
        let Ok(surface) = Surface::new(&context, window.clone()) else {
            self.fault = Some("surface_creation".to_owned());
            event_loop.exit();
            return;
        };
        self.window = Some(window.clone());
        self.context = Some(context);
        self.surface = Some(surface);
        self.relayout();
        window.request_redraw();
    }

    fn install_snapshot(&mut self, snapshot: TreeSnapshot) {
        match &mut self.mirror {
            Some(mirror) => {
                if mirror.replace(snapshot).is_err() {
                    self.fault = Some("renderer_snapshot".to_owned());
                }
            }
            None => match RendererMirror::from_snapshot(snapshot, youth_tree::Limits::default()) {
                Ok(mirror) => self.mirror = Some(mirror),
                Err(_) => self.fault = Some("renderer_snapshot".to_owned()),
            },
        }
        self.relayout();
    }

    fn runtime_event(&mut self, event: DesktopEvent, event_loop: &ActiveEventLoop) {
        match event {
            DesktopEvent::TurnCommitted(receipt) => {
                let applied = self
                    .mirror
                    .as_mut()
                    .is_some_and(|mirror| mirror.apply(receipt.patch_batch).is_ok());
                if !applied {
                    if let Some(controller) = &self.controller {
                        let _ = controller.send(ControllerCommand::Resync);
                    }
                    return;
                }
                self.relayout();
            }
            DesktopEvent::Resynced(snapshot) => self.install_snapshot(snapshot),
            DesktopEvent::AppRejected(_) => {}
            DesktopEvent::AppFaulted(error) => {
                self.fault = Some(category_name(error.category).to_owned());
            }
            DesktopEvent::Stopped => event_loop.exit(),
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn relayout(&mut self) {
        let (Some(window), Some(mirror)) = (&self.window, &self.mirror) else {
            return;
        };
        let size = window.inner_size();
        let scale = window.scale_factor();
        let viewport = LogicalSize::new(
            f64::from(size.width) / scale,
            f64::from(size.height) / scale,
        );
        self.layout = viewport
            .ok()
            .and_then(|viewport| layout(mirror.tree(), viewport).ok());
        if let Some(layout) = &self.layout {
            self.pointer.reconcile_layout(layout);
        }
    }

    fn present(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(surface), Some(mirror), Some(layout)) =
            (&self.window, &mut self.surface, &self.mirror, &self.layout)
        else {
            return;
        };
        let size = window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        let state = RenderState {
            hovered: self.pointer.hovered,
            pressed: self.pointer.pressed.then_some(self.pointer.armed).flatten(),
            fault_category: self.fault.as_deref(),
        };
        let Ok(frame) = render(
            mirror.tree(),
            layout,
            size.width,
            size.height,
            window.scale_factor(),
            &state,
            Palette::default(),
        ) else {
            self.fault = Some("renderer_failure".to_owned());
            return;
        };
        if surface.resize(width, height).is_err() {
            self.fault = Some("surface_failure".to_owned());
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            self.fault = Some("surface_failure".to_owned());
            return;
        };
        buffer.copy_from_slice(frame.pixels());
        if buffer.present().is_err() {
            self.fault = Some("surface_failure".to_owned());
            return;
        }
        if matches!(self.mode, Mode::Smoke) && !self.smoke_presented {
            self.smoke_presented = true;
            event_loop.exit();
        }
    }
}

fn bootstrap(config: YouthAppConfig, proxy: EventLoopProxy<NativeEvent>) {
    let _ = std::thread::Builder::new()
        .name("youth-desktop-bootstrap".to_owned())
        .spawn(move || match YouthAppHandle::spawn(config) {
            Ok(handle) => {
                let runtime = tokio::runtime::Builder::new_current_thread().build();
                match runtime {
                    Ok(runtime) => match runtime.block_on(handle.mount()) {
                        Ok(snapshot) => {
                            let _ = proxy.send_event(NativeEvent::Mounted(handle, snapshot));
                        }
                        Err(error) => {
                            let _ = proxy.send_event(NativeEvent::StartupFault(error.category()));
                        }
                    },
                    Err(_) => {
                        let _ = proxy
                            .send_event(NativeEvent::StartupFault(RuntimeErrorCategory::Internal));
                    }
                }
            }
            Err(error) => {
                let _ = proxy.send_event(NativeEvent::StartupFault(error.category()));
            }
        });
}

fn smoke_snapshot() -> TreeSnapshot {
    let id = |value| NodeId::new(value).expect("smoke IDs are non-zero");
    TreeSnapshot {
        revision: 0,
        root: id(1),
        nodes: vec![
            Node {
                id: id(1),
                data: NodeData::Root,
                children: vec![id(2)],
            },
            Node {
                id: id(2),
                data: NodeData::Box { enabled: true },
                children: vec![id(3), id(4)],
            },
            Node {
                id: id(3),
                data: NodeData::Text {
                    value: "Count: 0".to_owned(),
                },
                children: vec![],
            },
            Node {
                id: id(4),
                data: NodeData::Button {
                    label: "Increment".to_owned(),
                    enabled: true,
                },
                children: vec![],
            },
        ],
    }
}

fn category_name(category: RuntimeErrorCategory) -> &'static str {
    match category {
        RuntimeErrorCategory::ComponentTooLarge => "component_too_large",
        RuntimeErrorCategory::InvalidComponent => "invalid_component",
        RuntimeErrorCategory::UnsupportedWorld => "unsupported_world",
        RuntimeErrorCategory::LinkFailure => "link_failure",
        RuntimeErrorCategory::InstantiationFailure => "instantiation_failure",
        RuntimeErrorCategory::InvalidLifecycle => "invalid_lifecycle",
        RuntimeErrorCategory::GuestRejected => "guest_rejected",
        RuntimeErrorCategory::GuestTrap => "guest_trap",
        RuntimeErrorCategory::FuelExhausted => "fuel_exhausted",
        RuntimeErrorCategory::DeadlineExceeded => "deadline_exceeded",
        RuntimeErrorCategory::MemoryLimitExceeded => "memory_limit_exceeded",
        RuntimeErrorCategory::TransferLimitExceeded => "transfer_limit_exceeded",
        RuntimeErrorCategory::InvalidSnapshot => "invalid_snapshot",
        RuntimeErrorCategory::InvalidPatchBatch => "invalid_patch_batch",
        RuntimeErrorCategory::RevisionMismatch => "revision_mismatch",
        RuntimeErrorCategory::EventSequenceViolation => "event_sequence_violation",
        RuntimeErrorCategory::StateUnavailable => "state_unavailable",
        RuntimeErrorCategory::StateCommitFailed => "state_commit_failed",
        RuntimeErrorCategory::WorkerStopped => "worker_stopped",
        RuntimeErrorCategory::Internal => "internal",
    }
}
