//! Headless application tests through the real Instar runtime.
//!
//! The harness owns the same two pieces the native shell owns: a
//! [`WindowState`] that translates platform events and a [`HostBridge`] that
//! runs the component on its real runtime thread. Test helpers intentionally
//! start with `winit::event::WindowEvent` wherever winit permits it:
//!
//! ```text
//! WindowEvent
//!     -> instar-window translation
//!     -> HostBridge
//!     -> host
//!     -> real guest
//!     -> retained tree / host presentation
//! ```
//!
//! This is harness machinery, not a test grammar. Keep application-specific
//! selectors, fixtures, and assertions in the application test that owns
//! them.
//!
//! One name per action, and no aliases. Two spellings of `leave_window` are
//! how machinery starts becoming a vocabulary: readers begin asking what the
//! difference is, and eventually someone gives the two names different
//! meanings.

use std::sync::Arc;
use std::time::{Duration, Instant};

use accesskit::Action;
use instar_host::bridge::{HostBridge, Wake};
use instar_host::{HostEffect, NodeKey, Rect, Tree};
use instar_ui::NodeKind;
use instar_window::{WindowId, WindowMetricsChanged, WindowOutput, WindowState, winit_adapter};
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};

/// Default time allowed for a real guest round trip.
pub const DEFAULT_PATIENCE: Duration = Duration::from_secs(5);

/// A component running through the production window-to-guest path, without
/// creating a native window.
pub struct RuntimeHarness {
    state: WindowState,
    bridge: HostBridge,
    patience: Duration,
}

/// Launches a component with coherent window metrics and waits for its opening
/// retained tree.
pub fn launch_component(component: Vec<u8>, metrics: WindowMetricsChanged) -> RuntimeHarness {
    let wake: Wake = Arc::new(|| {});
    let mut bridge = HostBridge::spawn_with_monospace_face(
        component,
        metrics.window_id,
        wake,
        crate::default_font(),
    )
    .unwrap_or_else(|error| panic!("the component did not start: {error}"));

    bridge.on_window_event(WindowOutput::MetricsChanged(metrics));
    let mut harness = RuntimeHarness {
        state: WindowState::new(
            metrics.window_id,
            metrics.scale_factor,
            metrics.physical_size,
        ),
        bridge,
        patience: DEFAULT_PATIENCE,
    };
    harness
        .await_guest_commit()
        .unwrap_or_else(|error| panic!("the component did not commit its opening tree: {error}"));
    harness
}

impl RuntimeHarness {
    /// Changes the round-trip patience for a deliberately slow fixture.
    pub fn with_patience(mut self, patience: Duration) -> Self {
        self.patience = patience;
        self
    }

    pub fn window_id(&self) -> WindowId {
        self.state.window_id()
    }

    /// Sends one native window event through `instar-window` and the bridge.
    ///
    /// Events that only update translator state, such as modifiers, return no
    /// host effects by design.
    pub fn send_window_event(&mut self, event: WindowEvent) -> Vec<HostEffect> {
        let window_id = self.window_id();
        match winit_adapter::translate(&mut self.state, window_id, &event) {
            Some(output) => self.bridge.on_window_event(output),
            None => Vec::new(),
        }
    }

    /// Clicks the centre of a retained node using cursor movement and primary
    /// mouse button events. The target is resolved from current host layout,
    /// never by calling a host interaction method directly.
    pub fn click_node(&mut self, key: NodeKey) -> Vec<HostEffect> {
        let (x, y) = self.screen_point_of(key);
        self.move_to(x, y);
        let mut effects = self.button(ElementState::Pressed);
        effects.extend(self.button(ElementState::Released));
        effects
    }

    /// Starts a guest-controlled stall by clicking its fixture control and
    /// returns the instant immediately after the event was queued. No commit
    /// wait is performed: callers use this to prove host-owned interaction
    /// remains live while the guest is busy.
    pub fn stall_guest(&mut self, key: NodeKey) -> Instant {
        self.click_node(key);
        Instant::now()
    }

    /// Scrolls at a logical window position using a winit line-wheel event.
    pub fn wheel(&mut self, x: f64, y: f64, lines: f32) -> Vec<HostEffect> {
        self.move_to(x, y);
        self.send_window_event(WindowEvent::MouseWheel {
            device_id: winit::event::DeviceId::dummy(),
            delta: MouseScrollDelta::LineDelta(0.0, lines),
            phase: winit::event::TouchPhase::Moved,
        })
    }

    /// Sends a key press or release through the same logical-key mapping the
    /// adapter uses in production.
    ///
    /// Winit does not expose a constructible `KeyEvent` outside its event
    /// loop, so this uses the adapter's public mapping at the unavoidable
    /// boundary and still routes the resulting `WindowOutput` through the
    /// real bridge. Modifiers are sent as real `WindowEvent`s.
    pub fn key(&mut self, named: NamedKey, pressed: bool) -> Vec<HostEffect> {
        let key = winit_adapter::instar_key(&WinitKey::Named(named));
        let event = self.state.on_key(key, pressed, false);
        self.bridge.on_window_event(WindowOutput::Key(event))
    }

    pub fn press_key(&mut self, named: NamedKey) -> Vec<HostEffect> {
        let mut effects = self.key(named, true);
        effects.extend(self.key(named, false));
        effects
    }

    pub fn set_shift(&mut self, held: bool) {
        let modifiers = if held {
            ModifiersState::SHIFT
        } else {
            ModifiersState::empty()
        };
        self.send_window_event(WindowEvent::ModifiersChanged(modifiers.into()));
    }

    pub fn set_window_focus(&mut self, focused: bool) -> Vec<HostEffect> {
        self.send_window_event(WindowEvent::Focused(focused))
    }

    pub fn move_to(&mut self, x: f64, y: f64) -> Vec<HostEffect> {
        self.send_window_event(WindowEvent::CursorMoved {
            device_id: winit::event::DeviceId::dummy(),
            position: PhysicalPosition::new(
                x * self.state.scale_factor(),
                y * self.state.scale_factor(),
            ),
        })
    }

    pub fn leave_window(&mut self) -> Vec<HostEffect> {
        self.send_window_event(WindowEvent::CursorLeft {
            device_id: winit::event::DeviceId::dummy(),
        })
    }

    pub fn button(&mut self, state: ElementState) -> Vec<HostEffect> {
        self.send_window_event(WindowEvent::MouseInput {
            device_id: winit::event::DeviceId::dummy(),
            state,
            button: MouseButton::Left,
        })
    }

    pub fn click_at(&mut self, x: f64, y: f64) -> Vec<HostEffect> {
        self.move_to(x, y);
        let mut effects = self.button(ElementState::Pressed);
        effects.extend(self.button(ElementState::Released));
        effects
    }

    /// Waits for the next accepted guest commit. Host-only effects do not need
    /// this wait; use it after an interaction expected to change guest state.
    pub fn await_guest_commit(&mut self) -> Result<(), String> {
        let target = self.bridge.commit_sequence() + 1;
        let started = Instant::now();
        while started.elapsed() < self.patience {
            self.bridge.wait(Duration::from_millis(25));
            if self.bridge.commit_sequence() >= target {
                return Ok(());
            }
        }
        Err(format!(
            "no guest commit arrived within {:?} (last sequence {})",
            self.patience,
            self.bridge.commit_sequence()
        ))
    }

    /// Reads the host-owned retained tree. Assertions should inspect this
    /// rather than a guest-side state shortcut: it is the observable result
    /// after decode, validation, diff, layout, and presentation lowering.
    pub fn read_retained_tree(&self) -> &Tree {
        self.bridge
            .host()
            .window(self.window_id())
            .and_then(|window| window.tree())
            .unwrap_or_else(|| panic!("the component has no retained tree"))
    }

    pub fn expect_text(&self, key: NodeKey, expected: &str) {
        let node = self
            .read_retained_tree()
            .find(key)
            .unwrap_or_else(|| panic!("retained tree has no node {key:?}"));
        match &node.kind {
            NodeKind::Text { text } => assert_eq!(text, expected, "text at {key:?}"),
            kind => panic!("node {key:?} is {}, not text", kind.name()),
        }
    }

    pub fn read_text(&self, key: NodeKey) -> String {
        let node = self
            .read_retained_tree()
            .find(key)
            .unwrap_or_else(|| panic!("retained tree has no node {key:?}"));
        match &node.kind {
            NodeKind::Text { text } => text.clone(),
            kind => panic!("node {key:?} is {}, not text", kind.name()),
        }
    }

    pub fn expect_focus(&self, expected: Option<NodeKey>) {
        assert_eq!(self.focused(), expected, "focused node");
    }

    pub fn expect_guest_message_count(&self, expected: u64) {
        assert_eq!(
            self.bridge.guest_message_count(),
            expected,
            "messages queued from host to guest"
        );
    }

    pub fn focused(&self) -> Option<NodeKey> {
        self.window().focus().focused()
    }

    pub fn focus_ring_shown(&self) -> bool {
        self.window().focus().focus_visible()
    }

    pub fn hovered(&self) -> Option<(NodeKey, instar_ui::ScrollbarPart)> {
        self.window().scroll().hovered()
    }

    pub fn scroll_offset(&self, key: NodeKey) -> i32 {
        self.window().scroll().get(key).y
    }

    pub fn rect_of(&self, key: NodeKey) -> Rect {
        self.window()
            .layout()
            .and_then(|layout| layout.get(key))
            .unwrap_or_else(|| panic!("{key:?} should be laid out"))
    }

    pub fn read_layout(&self) -> &instar_ui::LayoutSnapshot {
        self.window().layout().expect("the component has no layout")
    }

    /// Returns the node's current screen-space centre, accounting for every
    /// retained scroll ancestor.
    pub fn screen_point_of(&self, key: NodeKey) -> (f64, f64) {
        let rect = self.rect_of(key);
        let mut ancestors = Vec::new();
        assert!(
            find_scroll_ancestors(&self.read_retained_tree().root, key, &mut ancestors),
            "{key:?} should be in the retained tree"
        );
        let offset: i32 = ancestors
            .into_iter()
            .map(|viewport| self.window().scroll().get(viewport).y)
            .sum();
        (
            f64::from(rect.x + rect.width / 2),
            f64::from(rect.y + rect.height / 2 - offset),
        )
    }

    pub fn scrollbar(&self, viewport: NodeKey) -> instar_ui::Scrollbar {
        let window = self.window();
        let layout = window.layout().expect("layout");
        let content = self
            .read_retained_tree()
            .find(viewport)
            .and_then(|node| node.children.first())
            .unwrap_or_else(|| panic!("{viewport:?} should have one content child"));
        let viewport_rect = layout.get(viewport).expect("the viewport");
        let content_rect = layout.get(content.key).expect("the content");
        instar_ui::Scrollbar::for_viewport(
            viewport_rect,
            content_rect.height,
            self.scroll_offset(viewport),
        )
        .unwrap_or_else(|| panic!("{viewport:?} should overflow"))
    }

    pub fn on_accessibility_action(
        &mut self,
        action: Action,
        target: accesskit::NodeId,
    ) -> Vec<HostEffect> {
        self.bridge.on_accessibility_action(action, target)
    }

    pub fn bridge_stats(&self) -> instar_host::bridge::BridgeStats {
        self.bridge.stats()
    }

    pub fn scene(&self) -> Option<&instar_paint::PaintScene> {
        self.window().scene()
    }

    fn window(&self) -> &instar_host::HostWindow {
        self.bridge
            .host()
            .window(self.window_id())
            .expect("the harness window")
    }
}

impl Drop for RuntimeHarness {
    fn drop(&mut self) {
        self.bridge.shutdown();
    }
}

fn find_scroll_ancestors(
    node: &instar_ui::Node,
    target: NodeKey,
    ancestors: &mut Vec<NodeKey>,
) -> bool {
    if node.key == target {
        return true;
    }
    for child in &node.children {
        if child.key == target {
            return true;
        }
        let pushed = matches!(child.kind, NodeKind::Scroll);
        if pushed {
            ancestors.push(child.key);
        }
        if find_scroll_ancestors(child, target, ancestors) {
            return true;
        }
        if pushed {
            ancestors.pop();
        }
    }
    false
}
