use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use accesskit_winit::Adapter as AccessAdapter;
use softbuffer::{Context, Surface};
use thiserror::Error;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition as WinitLogicalPosition, LogicalSize as WinitLogicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::{
    Controller, ControllerCommand, DesktopEvent, InteractionState, LogicalKey, LogicalPoint,
    LogicalSize, Modifiers, Palette, PointerState, RenderState, RendererMirror, SemanticAction,
    access, layout, render,
};
use youth_interaction::EditorInput;
use youth_runtime::{
    PresentationReader, RuntimeErrorCategory, YouthAppConfig, YouthAppHandle,
    next_display_boundary_epoch_millis,
};
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
    Access(accesskit_winit::Event),
    Shutdown,
}

impl From<accesskit_winit::Event> for NativeEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        Self::Access(event)
    }
}

enum Mode {
    Application(Option<Box<YouthAppConfig>>),
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
    interaction: InteractionState,
    modifiers: ModifiersState,
    controller: Option<Controller>,
    presentation: Option<PresentationReader>,
    editor_rasterizer: std::cell::RefCell<youth_text_render_cpu::GlyphRasterizer>,
    /// Host-owned vertical scroll offset (logical pixels) per Editor node.
    /// Purely a paint-time transform -- never sent to the guest, never
    /// part of any revision.
    editor_scroll_offsets: HashMap<NodeId, f32>,
    /// The fixed end of an in-progress pointer text-selection drag: which
    /// Editor, and its click-down position in that Editor's own content
    /// coordinate space (see [`Self::editor_content_point`]). `None` when
    /// no primary-button drag is in progress over an Editor.
    text_drag_anchor: Option<(NodeId, f32, f32)>,
    /// `None` until the window exists (it needs a live `Window` to
    /// construct). Feeds tree updates to, and receives action requests
    /// from, whatever assistive technology is listening.
    access_adapter: Option<AccessAdapter>,
    fault: Option<String>,
    smoke_presented: bool,
}

pub fn run(options: DesktopOptions) -> Result<(), DesktopError> {
    run_mode(
        Mode::Application(Some(Box::new(options.config))),
        options.width,
        options.height,
        false,
    )
}

/// Runs an application and accepts an orderly shutdown byte on stdin.
pub fn run_with_shutdown(options: DesktopOptions) -> Result<(), DesktopError> {
    run_mode(
        Mode::Application(Some(Box::new(options.config))),
        options.width,
        options.height,
        true,
    )
}

pub fn window_smoke() -> Result<(), DesktopError> {
    run_mode(Mode::Smoke, 320, 180, false)
}

fn run_mode(mode: Mode, width: u32, height: u32, stdin_shutdown: bool) -> Result<(), DesktopError> {
    let event_loop = EventLoop::<NativeEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    if stdin_shutdown {
        watch_shutdown(proxy.clone());
    }
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
        interaction: InteractionState::default(),
        modifiers: ModifiersState::empty(),
        controller: None,
        presentation: None,
        editor_rasterizer: std::cell::RefCell::new(youth_text_render_cpu::GlyphRasterizer::new()),
        editor_scroll_offsets: HashMap::new(),
        text_drag_anchor: None,
        access_adapter: None,
        fault: None,
        smoke_presented: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl ApplicationHandler<NativeEvent> for NativeApp {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. })
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match &mut self.mode {
            Mode::Application(config) => {
                if let Some(config) = config.take() {
                    bootstrap(*config, self.proxy.clone());
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
                self.presentation = Some(handle.presentation());
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
            NativeEvent::Access(event) => self.handle_access_event(event),
            NativeEvent::Shutdown => {
                if let Some(controller) = &self.controller {
                    let _ = controller.send(ControllerCommand::Stop);
                }
                event_loop.exit();
            }
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
        if let Some(adapter) = &mut self.access_adapter {
            adapter.process_event(&window, &event);
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
                self.sync_ime(&window);
                window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } if self.fault.is_none() => {
                let scale = window.scale_factor();
                let logical_point = LogicalPoint {
                    x: position.x / scale,
                    y: position.y / scale,
                };
                if let Some(layout) = &self.layout {
                    let change = self.pointer.move_to(logical_point, layout);
                    if change.redraw {
                        window.request_redraw();
                    }
                }
                if let (Some((editor, anchor_x, anchor_y)), Some(controller)) =
                    (self.text_drag_anchor, &self.controller)
                    && let Some((x, y)) = self.editor_content_point(editor, &window, logical_point)
                {
                    let _ = controller.send(ControllerCommand::EditorInput {
                        editor,
                        input: EditorInput::ExtendSelectionToPoint {
                            anchor_x,
                            anchor_y,
                            x,
                            y,
                        },
                    });
                    window.request_redraw();
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
                    ElementState::Pressed => {
                        let change = self.pointer.press_primary();
                        if let (Some(mirror), Some(armed)) = (&self.mirror, self.pointer.armed) {
                            let focus = self.interaction.focus_pointer_target(mirror.tree(), armed);
                            if focus.redraw {
                                window.request_redraw();
                            }
                            let is_editor = matches!(
                                mirror.tree().node(armed).map(|n| &n.data),
                                Some(NodeData::Editor { .. })
                            );
                            if is_editor
                                && let Some(cursor) = self.pointer.cursor
                                && let Some((x, y)) =
                                    self.editor_content_point(armed, &window, cursor)
                                && let Some(controller) = &self.controller
                            {
                                self.text_drag_anchor = Some((armed, x, y));
                                let _ = controller.send(ControllerCommand::EditorInput {
                                    editor: armed,
                                    input: EditorInput::MoveToPoint { x, y },
                                });
                            }
                        }
                        change
                    }
                    ElementState::Released => {
                        self.text_drag_anchor = None;
                        self.layout
                            .as_ref()
                            .map_or_else(crate::InputChange::default, |layout| {
                                self.pointer.release_primary(layout)
                            })
                    }
                };
                if let Some(node) = change.activation
                    && let Some(controller) = &self.controller
                {
                    let _ = controller.send(ControllerCommand::Activate(node));
                }
                if state == ElementState::Pressed {
                    self.sync_ime(&window);
                    window.request_redraw();
                }
                if change.redraw {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.fault.is_none() => {
                let (Some(mirror), Some(layout), Some(node)) =
                    (&self.mirror, &self.layout, self.pointer.hovered)
                else {
                    return;
                };
                if !matches!(
                    mirror.tree().node(node).map(|n| &n.data),
                    Some(NodeData::Editor { .. })
                ) {
                    return;
                }
                let Some(rect) = layout.nodes.get(&node).map(|n| n.bounds) else {
                    return;
                };
                let delta_y = match delta {
                    MouseScrollDelta::LineDelta(_, lines) => lines * SCROLL_LINE_HEIGHT,
                    MouseScrollDelta::PixelDelta(position) => {
                        (position.y / window.scale_factor()) as f32
                    }
                };
                let content_height = self
                    .presentation
                    .as_ref()
                    .and_then(|reader| reader.editor(node))
                    .map_or(0.0, |presentation| presentation.content_height);
                let current = self
                    .editor_scroll_offsets
                    .get(&node)
                    .copied()
                    .unwrap_or(0.0);
                let updated =
                    clamp_scroll_offset(current - delta_y, rect.height as f32, content_height);
                self.editor_scroll_offsets.insert(node, updated);
                window.request_redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. } if self.fault.is_none() => {
                let Some(key) = logical_key(&event.logical_key) else {
                    return;
                };
                let Some(mirror) = &self.mirror else {
                    return;
                };
                if event.state == ElementState::Pressed {
                    let change = self.interaction.key(
                        mirror.tree(),
                        key,
                        modifiers(self.modifiers),
                        event.repeat,
                    );
                    if let Some(SemanticAction::Activate(node)) = change.action
                        && let Some(controller) = &self.controller
                    {
                        let _ = controller.send(ControllerCommand::Activate(node));
                    }
                    let has_editor_input = change.editor_input.is_some();
                    if let Some((editor, input)) = change.editor_input
                        && let Some(controller) = &self.controller
                    {
                        let _ = controller.send(ControllerCommand::EditorInput { editor, input });
                    }
                    self.sync_ime(&window);
                    if change.redraw || change.action.is_some() || has_editor_input {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::Ime(ime) if self.fault.is_none() => {
                let (Some(mirror), Some(controller)) = (&self.mirror, &self.controller) else {
                    return;
                };
                let Some(editor) = self.interaction.focused_editor(mirror.tree()) else {
                    return;
                };
                let input = match ime {
                    Ime::Enabled | Ime::Disabled => None,
                    Ime::Preedit(text, cursor) => Some(if text.is_empty() {
                        EditorInput::ImeClearCompose
                    } else {
                        EditorInput::ImeSetCompose { text, cursor }
                    }),
                    Ime::Commit(text) => Some(EditorInput::ImeCommit(text)),
                };
                if let Some(input) = input {
                    let _ = controller.send(ControllerCommand::EditorInput { editor, input });
                    window.request_redraw();
                }
            }
            WindowEvent::Focused(false) if self.pointer.deactivate_window().redraw => {
                window.request_redraw();
            }
            _ => {}
        }
    }
}

fn watch_shutdown(proxy: EventLoopProxy<NativeEvent>) {
    let _ = std::thread::Builder::new()
        .name("youth-desktop-shutdown".to_owned())
        .spawn(move || {
            use std::io::Read;

            let mut byte = [0_u8; 1];
            if std::io::stdin().read(&mut byte).is_ok() {
                let _ = proxy.send_event(NativeEvent::Shutdown);
            }
        });
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
        self.access_adapter = Some(AccessAdapter::with_event_loop_proxy(
            event_loop,
            &window,
            self.proxy.clone(),
        ));
        self.window = Some(window.clone());
        self.context = Some(context);
        self.surface = Some(surface);
        self.relayout();
        self.sync_ime(&window);
        self.sync_access();
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
            DesktopEvent::Resynced(snapshot) => {
                let _span = tracing::info_span!("desktop.resync").entered();
                self.install_snapshot(snapshot);
            }
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
        if let Some(mirror) = &self.mirror {
            self.interaction.reconcile(mirror.tree());
        }
    }

    /// Tells the platform IME whether text input is currently possible, and
    /// where its candidate window should avoid drawing. Called after any
    /// event that could change which node is focused, or move the caret
    /// within an already-focused Editor.
    fn sync_ime(&self, window: &Window) {
        let Some(mirror) = &self.mirror else {
            window.set_ime_allowed(false);
            return;
        };
        let Some(editor) = self.interaction.focused_editor(mirror.tree()) else {
            window.set_ime_allowed(false);
            return;
        };
        window.set_ime_allowed(true);
        let Some(presentation) = self.presentation.as_ref().and_then(|p| p.editor(editor)) else {
            return;
        };
        let area = presentation.ime_cursor_area;
        window.set_ime_cursor_area(
            WinitLogicalPosition::new(area.x0, area.y0),
            WinitLogicalSize::new((area.x1 - area.x0).max(1.0), (area.y1 - area.y0).max(1.0)),
        );
    }

    /// Adjusts the focused Editor's scroll offset to keep its caret in
    /// view, then clamps it to the live content bounds. Purely a paint-time
    /// concern -- no guest turn, no application-state write, no revision
    /// change -- so it runs on every `present()` rather than only after
    /// input, and is cheap enough to do so (a hash-map lookup plus a
    /// couple of float comparisons).
    fn sync_editor_scroll(&mut self) {
        let Some(mirror) = &self.mirror else { return };
        let Some(editor) = self.interaction.focused_editor(mirror.tree()) else {
            return;
        };
        let Some(layout) = &self.layout else { return };
        let Some(rect) = layout.nodes.get(&editor).map(|node| node.bounds) else {
            return;
        };
        let Some(presentation) = self
            .presentation
            .as_ref()
            .and_then(|reader| reader.editor(editor))
        else {
            return;
        };

        let viewport_height = rect.height as f32;
        let mut offset = self
            .editor_scroll_offsets
            .get(&editor)
            .copied()
            .unwrap_or(0.0);
        if let Some(cursor) = presentation.cursor {
            offset = follow_cursor_scroll_offset(
                offset,
                viewport_height,
                cursor.y0 as f32,
                cursor.y1 as f32,
            );
        }
        offset = clamp_scroll_offset(offset, viewport_height, presentation.content_height);
        self.editor_scroll_offsets.insert(editor, offset);
    }

    /// Converts a window-logical pointer position to `node`'s own text
    /// content coordinate space -- the same space the engine's `hit_test`
    /// and a presentation's glyph positions use. Mirrors
    /// `draw_editor_presentation`'s paint-time origin math exactly (physical
    /// rect origin, minus the physical scroll offset) so a click lands on
    /// the glyph the user is actually looking at.
    fn editor_content_point(
        &self,
        node: NodeId,
        window: &Window,
        point: LogicalPoint,
    ) -> Option<(f32, f32)> {
        let rect = self.layout.as_ref()?.nodes.get(&node)?.bounds;
        let scale = window.scale_factor();
        let scroll_offset_y = self
            .editor_scroll_offsets
            .get(&node)
            .copied()
            .unwrap_or(0.0)
            * scale as f32;
        let content_x = ((point.x - rect.x) * scale) as f32;
        let content_y = ((point.y - rect.y) * scale) as f32 + scroll_offset_y;
        Some((content_x, content_y))
    }

    /// Rebuilds the accessibility tree from current state and pushes it to
    /// the platform adapter. Cheap enough (a handful of nodes for a small
    /// UI) to call unconditionally on every redraw rather than tracked
    /// incrementally; `update_if_active` itself is a no-op until an actual
    /// assistive technology has activated accessibility.
    fn sync_access(&mut self) {
        let (Some(mirror), Some(layout), Some(window)) = (&self.mirror, &self.layout, &self.window)
        else {
            return;
        };
        let update = access::build_tree_update(
            mirror.tree(),
            layout,
            &self.interaction,
            self.presentation.as_ref(),
            window.scale_factor(),
            &self.editor_scroll_offsets,
        );
        if let Some(adapter) = &mut self.access_adapter {
            adapter.update_if_active(|| update);
        }
    }

    fn handle_access_event(&mut self, event: accesskit_winit::Event) {
        match event.window_event {
            accesskit_winit::WindowEvent::InitialTreeRequested => self.sync_access(),
            accesskit_winit::WindowEvent::ActionRequested(request) => {
                self.handle_access_action(request);
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => {}
        }
    }

    fn handle_access_action(&mut self, request: accesskit::ActionRequest) {
        let Some(target) = youth_tree::NodeId::new(request.target_node.0) else {
            return;
        };
        let Some(mirror) = &self.mirror else { return };
        let is_editor = matches!(
            mirror.tree().node(target).map(|node| &node.data),
            Some(NodeData::Editor { .. })
        );
        match request.action {
            accesskit::Action::Focus => {
                let change = self.interaction.focus_pointer_target(mirror.tree(), target);
                if change.redraw
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }
            accesskit::Action::Click if !is_editor => {
                if let Some(controller) = &self.controller {
                    let _ = controller.send(ControllerCommand::Activate(target));
                }
            }
            accesskit::Action::SetTextSelection if is_editor => {
                let Some(accesskit::ActionData::SetTextSelection(selection)) = request.data else {
                    return;
                };
                if let Some(controller) = &self.controller {
                    let _ = controller.send(ControllerCommand::EditorLocalEdit {
                        editor: target,
                        edit: youth_runtime::EditorLocalEdit::SetSelectionFromAccessKit(selection),
                    });
                }
            }
            accesskit::Action::ReplaceSelectedText if is_editor => {
                let Some(accesskit::ActionData::Value(text)) = request.data else {
                    return;
                };
                if let Some(controller) = &self.controller {
                    let _ = controller.send(ControllerCommand::EditorInput {
                        editor: target,
                        input: EditorInput::InsertText(text.into()),
                    });
                }
            }
            _ => {}
        }
    }

    fn present(&mut self, event_loop: &ActiveEventLoop) {
        let _span = tracing::info_span!("desktop.present").entered();
        self.sync_editor_scroll();
        self.sync_access();
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
            focused: self.interaction.focused(),
            fault_category: self.fault.as_deref(),
            presentation: self.presentation.as_ref(),
            editor_rasterizer: Some(&self.editor_rasterizer),
            editor_scroll_offsets: Some(&self.editor_scroll_offsets),
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
        self.arm_countdown_repaint(event_loop);
        if matches!(self.mode, Mode::Smoke) && !self.smoke_presented {
            self.smoke_presented = true;
            event_loop.exit();
        }
    }

    fn arm_countdown_repaint(&self, event_loop: &ActiveEventLoop) {
        let (Some(mirror), Some(presentation)) = (&self.mirror, &self.presentation) else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        let now_epoch_millis = presentation.now_epoch_millis();
        let next_epoch_millis = mirror
            .tree()
            .to_snapshot()
            .nodes
            .iter()
            .filter_map(|node| node.data.countdown_ref())
            .filter_map(|(schedule, _, _)| {
                let record = presentation.schedule(schedule.id)?;
                (record.generation == schedule.generation).then_some(record)
            })
            .filter_map(|record| {
                next_display_boundary_epoch_millis(Some(&record), now_epoch_millis)
            })
            .min();
        let Some(next_epoch_millis) = next_epoch_millis else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        let delay = next_epoch_millis.saturating_sub(now_epoch_millis);
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(delay),
        ));
    }
}

/// Logical pixels one wheel "line" scrolls, for platforms that report
/// [`MouseScrollDelta::LineDelta`] instead of pixel deltas.
const SCROLL_LINE_HEIGHT: f32 = 20.0;

/// Clamps a scroll offset to `[0, max(0, content_height - viewport_height)]`
/// -- scrolling can never move the viewport past either end of the content.
fn clamp_scroll_offset(offset: f32, viewport_height: f32, content_height: f32) -> f32 {
    let max_offset = (content_height - viewport_height).max(0.0);
    offset.clamp(0.0, max_offset)
}

/// Adjusts `offset` by the minimum amount needed to bring
/// `[cursor_top, cursor_bottom)` fully inside `[offset, offset +
/// viewport_height)`, preferring to leave `offset` untouched when the
/// caret is already visible.
fn follow_cursor_scroll_offset(
    offset: f32,
    viewport_height: f32,
    cursor_top: f32,
    cursor_bottom: f32,
) -> f32 {
    if cursor_top < offset {
        cursor_top
    } else if cursor_bottom > offset + viewport_height {
        cursor_bottom - viewport_height
    } else {
        offset
    }
    .max(0.0)
}

fn modifiers(state: ModifiersState) -> Modifiers {
    Modifiers {
        shift: state.shift_key(),
        control: state.control_key(),
        alt: state.alt_key(),
        super_key: state.super_key(),
    }
}

fn logical_key(key: &Key) -> Option<LogicalKey> {
    match key {
        Key::Character(value) => {
            let mut characters = value.chars();
            let character = characters.next()?;
            characters.next().is_none().then_some(if character == ' ' {
                LogicalKey::Space
            } else {
                LogicalKey::Character(character)
            })
        }
        Key::Named(NamedKey::Enter) => Some(LogicalKey::Enter),
        Key::Named(NamedKey::Escape) => Some(LogicalKey::Escape),
        Key::Named(NamedKey::Backspace) => Some(LogicalKey::Backspace),
        Key::Named(NamedKey::Space) => Some(LogicalKey::Space),
        Key::Named(NamedKey::Tab) => Some(LogicalKey::Tab),
        Key::Named(NamedKey::ArrowLeft) => Some(LogicalKey::ArrowLeft),
        Key::Named(NamedKey::ArrowRight) => Some(LogicalKey::ArrowRight),
        Key::Named(NamedKey::ArrowUp) => Some(LogicalKey::ArrowUp),
        Key::Named(NamedKey::ArrowDown) => Some(LogicalKey::ArrowDown),
        Key::Named(NamedKey::Home) => Some(LogicalKey::Home),
        Key::Named(NamedKey::End) => Some(LogicalKey::End),
        Key::Named(_) | Key::Dead(_) | Key::Unidentified(_) => None,
    }
}

fn bootstrap(config: YouthAppConfig, proxy: EventLoopProxy<NativeEvent>) {
    let _ = std::thread::Builder::new()
        .name("youth-desktop-bootstrap".to_owned())
        .spawn(move || {
            let _span = tracing::info_span!("desktop.mount").entered();
            match YouthAppHandle::spawn(config) {
                Ok(handle) => {
                    let runtime = tokio::runtime::Builder::new_current_thread().build();
                    match runtime {
                        Ok(runtime) => match runtime.block_on(handle.mount()) {
                            Ok(snapshot) => {
                                let _ = proxy.send_event(NativeEvent::Mounted(handle, snapshot));
                            }
                            Err(error) => {
                                let _ =
                                    proxy.send_event(NativeEvent::StartupFault(error.category()));
                            }
                        },
                        Err(_) => {
                            let _ = proxy.send_event(NativeEvent::StartupFault(
                                RuntimeErrorCategory::Internal,
                            ));
                        }
                    }
                }
                Err(error) => {
                    let _ = proxy.send_event(NativeEvent::StartupFault(error.category()));
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::NativeKey;

    #[test]
    fn logical_key_normalization_is_exact_and_rejects_compositions() {
        assert_eq!(
            logical_key(&Key::Character("7".into())),
            Some(LogicalKey::Character('7'))
        );
        assert_eq!(
            logical_key(&Key::Character("+".into())),
            Some(LogicalKey::Character('+'))
        );
        assert_eq!(
            logical_key(&Key::Character(" ".into())),
            Some(LogicalKey::Space)
        );
        assert_eq!(logical_key(&Key::Character("ab".into())), None);
        assert_eq!(logical_key(&Key::Dead(Some('\u{301}'))), None);
        assert_eq!(
            logical_key(&Key::Unidentified(NativeKey::Unidentified)),
            None
        );
    }

    #[test]
    fn named_keys_cover_focus_default_cancel_and_editing_policy() {
        assert_eq!(
            logical_key(&Key::Named(NamedKey::Enter)),
            Some(LogicalKey::Enter)
        );
        assert_eq!(
            logical_key(&Key::Named(NamedKey::Escape)),
            Some(LogicalKey::Escape)
        );
        assert_eq!(
            logical_key(&Key::Named(NamedKey::Backspace)),
            Some(LogicalKey::Backspace)
        );
        assert_eq!(
            logical_key(&Key::Named(NamedKey::Tab)),
            Some(LogicalKey::Tab)
        );
        assert_eq!(
            logical_key(&Key::Named(NamedKey::ArrowLeft)),
            Some(LogicalKey::ArrowLeft)
        );
        assert_eq!(
            logical_key(&Key::Named(NamedKey::Home)),
            Some(LogicalKey::Home)
        );
        assert_eq!(
            logical_key(&Key::Named(NamedKey::End)),
            Some(LogicalKey::End)
        );
    }

    #[test]
    fn scroll_offset_clamps_to_content_bounds() {
        assert_eq!(
            clamp_scroll_offset(-5.0, 100.0, 400.0),
            0.0,
            "offset never goes negative"
        );
        assert_eq!(
            clamp_scroll_offset(9_999.0, 100.0, 400.0),
            300.0,
            "offset never scrolls past the last page of content"
        );
        assert_eq!(
            clamp_scroll_offset(50.0, 100.0, 40.0),
            0.0,
            "content shorter than the viewport cannot scroll at all"
        );
    }

    #[test]
    fn cursor_follow_scroll_only_moves_when_the_caret_leaves_the_viewport() {
        // Caret already fully visible: offset is untouched.
        assert_eq!(follow_cursor_scroll_offset(20.0, 100.0, 40.0, 56.0), 20.0);
        // Caret above the viewport: scroll up to reveal it exactly.
        assert_eq!(follow_cursor_scroll_offset(50.0, 100.0, 10.0, 26.0), 10.0);
        // Caret below the viewport: scroll down the minimum amount.
        assert_eq!(follow_cursor_scroll_offset(0.0, 100.0, 120.0, 136.0), 36.0);
        // Never scrolls negative even for a caret near the very top.
        assert_eq!(follow_cursor_scroll_offset(5.0, 100.0, 0.0, 16.0), 0.0);
    }
}
