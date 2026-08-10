//! Instar's accessibility smoke fixture.
//!
//! One deliberately compact tree, shaped so that a single pass with a real
//! screen reader exercises most of Phase 2 vertically:
//!
//! ```text
//! Window
//! ├── Text readout          -- outside the scroll, so it stays announceable
//! └── Scroll                -- bounded, so it is a viewport and not just a box
//!     ├── disabled Button   -- state, not omission
//!     ├── ordinary Button   -- name, role, activation
//!     ├── tall spacer
//!     └── offscreen Button  -- reveal, then activation
//! ```
//!
//! The offscreen button is the point. Reaching it requires the projection to
//! describe a node that is not painted, focus to move to it, the E3 reveal path
//! to scroll it into view, the F3 activation seam to fire from an accessibility
//! source rather than a pointer, and the F2 incremental update to describe what
//! changed afterwards. If a screen reader can press it and hear the result,
//! every one of those worked.
//!
//! The readout exists so that activation has a *visible, announceable*
//! consequence. A button that does nothing observable proves only that the
//! click was delivered, not that the update came back. It sits outside the
//! scroll deliberately: inside, it would scroll away exactly when the offscreen
//! button is reached, which is the moment it is needed.
//!
//! The scroll grows to fill what the readout leaves, so the viewport follows a
//! resized window instead of being a literal that stops matching it.

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
const SCROLL: NodeKey = NodeKey::first(1);
const COLUMN: NodeKey = NodeKey::first(2);
const READOUT: NodeKey = NodeKey::first(3);
const UNAVAILABLE: NodeKey = NodeKey::first(4);
const ORDINARY: NodeKey = NodeKey::first(5);
const SPACER: NodeKey = NodeKey::first(6);
const OFFSCREEN: NodeKey = NodeKey::first(7);

/// Tall enough that the last button starts well outside the viewport, so
/// reaching it is a real reveal rather than an accident of rounding.
const SPACER_HEIGHT: u16 = 600;

struct Smoke {
    ordinary: u32,
    offscreen: u32,
}

impl Smoke {
    fn readout(&self) -> String {
        match (self.ordinary, self.offscreen) {
            (0, 0) => "Nothing pressed yet".to_string(),
            (n, 0) => format!("Ordinary pressed {n}"),
            (0, n) => format!("Offscreen pressed {n}"),
            (a, b) => format!("Ordinary pressed {a}, offscreen pressed {b}"),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let readout = self.readout();
        let mut encoder = BatchEncoder::new();
        encoder
            .node(
                opcode::NODE_ROOT,
                ROOT,
                0,
                None,
                WireLayout {
                    padding: 16,
                    gap: 12,
                    ..WireLayout::default()
                },
                2,
            )
            .node(
                opcode::NODE_TEXT,
                READOUT,
                0,
                Some(&readout),
                WireLayout::default(),
                0,
            )
            // A bounded viewport with taller content, which is what puts the
            // last button out of sight.
            .node(
                opcode::NODE_SCROLL,
                SCROLL,
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
                COLUMN,
                0,
                None,
                WireLayout {
                    align_self: Some(WireAlign::Stretch),
                    gap: 12,
                    ..WireLayout::default()
                },
                4,
            )
            // Present and disabled, never absent. A screen reader should find
            // this and say it is unavailable.
            .node(
                opcode::NODE_BUTTON,
                UNAVAILABLE,
                0,
                Some("Unavailable"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                ORDINARY,
                flags::ENABLED,
                Some("Ordinary button"),
                WireLayout::default(),
                0,
            )
            // Nothing but height. Its only job is to push what follows out of
            // the viewport.
            .node(
                opcode::NODE_TEXT,
                SPACER,
                0,
                Some("Scroll past this"),
                WireLayout {
                    height: WireSize::Fixed(SPACER_HEIGHT),
                    ..WireLayout::default()
                },
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                OFFSCREEN,
                flags::ENABLED,
                Some("Offscreen button"),
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
            WireEvent::Click { node } if node == ORDINARY => self.ordinary += 1,
            WireEvent::Click { node } if node == OFFSCREEN => self.offscreen += 1,
            // Includes the disabled button, which the host refuses to hit at
            // all — if this fixture ever counts a press there, something
            // upstream stopped enforcing it.
            WireEvent::Click { .. } => {}
        }
    }
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), String> {
        let mut smoke = Smoke {
            ordinary: 0,
            offscreen: 0,
        };
        smoke.commit().await?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let event = WireEvent::decode(&payload)
                        .map_err(|error| format!("undecodable host event: {error}"))?;
                    smoke.handle(event);
                    smoke.commit().await?;
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);
