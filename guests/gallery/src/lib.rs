//! Instar's UI Gallery.
//!
//! **First an integration harness, second a visual catalog.** That ordering is
//! not a preference; it is what the defects found by the first non-counter
//! guest argued for. Three complete, tested subsystems — the wheel, the
//! pointer move, the keyboard — were each disconnected by a single missing arm
//! in one `match`. Every package was correct. No package-level test could see
//! it, because at package level nothing was missing.
//!
//! So the Interaction Lab comes before the typography, and its rule is that
//! every native input modality must be shown *entering through the real
//! platform adapter* and producing a visible effect.
//!
//! ```text
//! Interaction Lab
//! ├── Pointer      hover, press/release, activation
//! ├── Scroll       wheel, thumb drag, nested scrolling
//! ├── Keyboard     Tab, Shift+Tab, Enter, Space
//! ├── Focus        offscreen reveal, focus ring
//! ├── Accessibility  focus, activate, scroll into view
//! └── Guest Stall  block wasm for 500ms
//! ```
//!
//! # Why the readout is the proof
//!
//! Every control here changes a counter, and the counters are printed at the
//! top, outside every viewport. A test that asserts a button was *hit* proves
//! the host resolved a coordinate. A test that asserts the readout *changed*
//! proves the event reached the guest, the guest committed, and the host
//! applied it — the whole round trip, which is the part a missing seam breaks.
//!
//! # Why the stall button exists
//!
//! Instar's central claim is that native interaction is independent of guest
//! liveness: scrolling, focus, the focus ring and pressed presentation are all
//! host-owned, and a guest that is busy cannot make the window stop responding.
//! That claim was previously only visible in tests. Pressing **Stall guest
//! 500ms** blocks the wasm thread outright, and everything above must keep
//! working while it is blocked. Application consequences queue; interaction
//! does not.

wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
});

use instar_ui_protocol::{
    BatchEncoder, NodeKey, WireAlign, WireEvent, WireLayout, WireSize, flags, opcode,
};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;

const ROOT: NodeKey = NodeKey::first(0);
const STATUS: NodeKey = NodeKey::first(1);
const STALL: NodeKey = NodeKey::first(2);

const OUTER: NodeKey = NodeKey::first(10);
const OUTER_COLUMN: NodeKey = NodeKey::first(11);
const POINTER_TARGET: NodeKey = NodeKey::first(12);
const DISABLED: NodeKey = NodeKey::first(13);
const OUTER_SPACER: NodeKey = NodeKey::first(14);
const OFFSCREEN: NodeKey = NodeKey::first(15);

const INNER: NodeKey = NodeKey::first(20);
const INNER_COLUMN: NodeKey = NodeKey::first(21);
const INNER_TOP: NodeKey = NodeKey::first(22);
const INNER_SPACER: NodeKey = NodeKey::first(23);
const INNER_BOTTOM: NodeKey = NodeKey::first(24);

/// Taller than the inner viewport, so the inner scroll has somewhere to go —
/// and short enough that the outer scroll still has its own overflow. Nested
/// scrolling is only tested by a fixture where *both* can move.
const INNER_SPACER_HEIGHT: u16 = 300;
/// The inner viewport. Fixed, because a nested scroll that grows would take
/// the outer scroll's overflow with it.
const INNER_VIEWPORT: u16 = 120;
/// Pushes the last control well outside the outer viewport, so reaching it is
/// a real reveal.
const OUTER_SPACER_HEIGHT: u16 = 400;

/// Long enough to be unmistakably a stall rather than a slow frame, short
/// enough that a test can wait it out.
const STALL_MILLIS: u64 = 500;

#[derive(Default)]
struct Gallery {
    pointer: u32,
    inner_top: u32,
    inner_bottom: u32,
    offscreen: u32,
    stalls: u32,
}

impl Gallery {
    /// The proof surface. Outside every viewport, so it cannot scroll away at
    /// the moment it is needed.
    fn status(&self) -> String {
        format!(
            "pointer {} inner {}/{} offscreen {} stalls {}",
            self.pointer, self.inner_top, self.inner_bottom, self.offscreen, self.stalls
        )
    }

    fn encode(&self) -> Vec<u8> {
        let status = self.status();
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                ROOT,
                0,
                None,
                WireLayout {
                    padding: 12,
                    gap: 8,
                    ..WireLayout::default()
                },
                3,
            )
            .node(
                opcode::NODE_TEXT,
                STATUS,
                0,
                Some(&status),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                STALL,
                flags::ENABLED,
                Some("Stall guest 500ms"),
                WireLayout::default(),
                0,
            )
            // The outer viewport takes what the header leaves.
            .node(
                opcode::NODE_SCROLL,
                OUTER,
                0,
                None,
                WireLayout {
                    grow: 1.0,
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                },
                1,
            )
            .node(
                opcode::NODE_COLUMN,
                OUTER_COLUMN,
                0,
                None,
                WireLayout {
                    align_self: Some(WireAlign::Stretch),
                    gap: 8,
                    ..WireLayout::default()
                },
                5,
            )
            .node(
                opcode::NODE_BUTTON,
                POINTER_TARGET,
                flags::ENABLED,
                Some("Pointer target"),
                WireLayout::default(),
                0,
            )
            // Present and disabled, never absent: the host refuses to hit it,
            // and an assistive technology should announce it as unavailable.
            .node(
                opcode::NODE_BUTTON,
                DISABLED,
                0,
                Some("Disabled control"),
                WireLayout::default(),
                0,
            )
            // A viewport inside a viewport. What this exists to catch is a
            // wheel that stops at the innermost scroll instead of bubbling its
            // residual delta outwards once the inner one is at its limit.
            .node(
                opcode::NODE_SCROLL,
                INNER,
                0,
                None,
                WireLayout {
                    height: WireSize::Fixed(INNER_VIEWPORT),
                    align_self: Some(WireAlign::Stretch),
                    ..WireLayout::default()
                },
                1,
            )
            .node(
                opcode::NODE_COLUMN,
                INNER_COLUMN,
                0,
                None,
                WireLayout {
                    align_self: Some(WireAlign::Stretch),
                    gap: 8,
                    ..WireLayout::default()
                },
                3,
            )
            .node(
                opcode::NODE_BUTTON,
                INNER_TOP,
                flags::ENABLED,
                Some("Inner top"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_TEXT,
                INNER_SPACER,
                0,
                Some("inner overflow"),
                WireLayout {
                    height: WireSize::Fixed(INNER_SPACER_HEIGHT),
                    ..WireLayout::default()
                },
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                INNER_BOTTOM,
                flags::ENABLED,
                Some("Inner bottom"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_TEXT,
                OUTER_SPACER,
                0,
                Some("outer overflow"),
                WireLayout {
                    height: WireSize::Fixed(OUTER_SPACER_HEIGHT),
                    ..WireLayout::default()
                },
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                OFFSCREEN,
                flags::ENABLED,
                Some("Offscreen target"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    async fn commit(&self) -> Result<(), String> {
        kernel_ui::commit(self.encode())
            .await
            .map(|_| ())
            .map_err(|error| format!("commit failed: {error:?}"))
    }

    fn handle(&mut self, event: WireEvent) {
        match event {
            WireEvent::Click { node } if node == POINTER_TARGET => self.pointer += 1,
            WireEvent::Click { node } if node == INNER_TOP => self.inner_top += 1,
            WireEvent::Click { node } if node == INNER_BOTTOM => self.inner_bottom += 1,
            WireEvent::Click { node } if node == OFFSCREEN => self.offscreen += 1,
            WireEvent::Click { node } if node == STALL => {
                self.stalls += 1;
                stall();
            }
            // Includes the disabled control, which the host refuses to hit at
            // all. If this fixture ever counts a press there, something
            // upstream stopped enforcing it.
            WireEvent::Click { .. } => {}
        }
    }
}

/// Blocks the guest outright, on purpose.
///
/// A busy loop rather than a sleep: the point is to occupy the runtime thread
/// the way a guest doing real work would, not to park politely somewhere the
/// runtime could schedule around.
fn stall() {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(STALL_MILLIS);
    while std::time::Instant::now() < until {
        std::hint::spin_loop();
    }
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), String> {
        let mut gallery = Gallery::default();
        gallery.commit().await?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let event = WireEvent::decode(&payload)
                        .map_err(|error| format!("undecodable host event: {error}"))?;
                    gallery.handle(event);
                    gallery.commit().await?;
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);
