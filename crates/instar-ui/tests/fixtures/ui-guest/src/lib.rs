//! WP5 interaction guest: a counter with a button.
//!
//! Note what this links: `instar-ui-protocol` and nothing else from Instar. A
//! guest speaks the wire format; it does not link the host's UI
//! implementation, its layout engine, or its hit-testing. Building the tree
//! here means calling the encoder directly, which is slightly more verbose
//! than a tree-builder API would be — and that verbosity is the point, since
//! it is exactly the surface a non-Rust guest would have to reimplement.

wit_bindgen::generate!({
    path: "../../../../instar-kernel/wit",
    world: "kernel",
});

use instar_ui_protocol::{
    flags, opcode, BatchEncoder, NodeKey, WireDimension, WireEvent, WireLayout,
};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;

const ROOT: NodeKey = NodeKey(0);
const COLUMN: NodeKey = NodeKey(1);
const LABEL: NodeKey = NodeKey(2);
const BUTTON: NodeKey = NodeKey(3);
const RESET: NodeKey = NodeKey(4);

/// Fills the available width and pads its contents.
fn container(padding: u16, gap: u16) -> WireLayout {
    WireLayout {
        width: WireDimension::Fill,
        height: WireDimension::Content,
        padding,
        gap,
    }
}

struct Counter {
    count: u32,
}

impl Counter {
    /// Encodes the tree for the current state.
    ///
    /// Note what is absent: any geometry at all. This guest states layout
    /// *intent* -- fill the width, pad by 8 -- and the host decides every
    /// number. There is no longer a way to express a rectangle on this wire
    /// even deliberately.
    fn encode(&self) -> Vec<u8> {
        // Reset is meaningless at zero, and the host refuses to hit disabled
        // nodes -- a real behavioural difference, not decoration.
        let reset_flags = if self.count == 0 { 0 } else { flags::ENABLED };

        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, ROOT, 0, None, container(8, 0), 1)
            .node(opcode::NODE_COLUMN, COLUMN, 0, None, container(0, 4), 3)
            .node(
                opcode::NODE_TEXT,
                LABEL,
                0,
                Some(&format!("Clicked {} times", self.count)),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                BUTTON,
                flags::ENABLED,
                Some("Press me"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                RESET,
                reset_flags,
                Some("Reset"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    fn commit(&self) -> Result<(), String> {
        kernel_ui::commit(&self.encode())
            .map(|_| ())
            .map_err(|e| format!("commit failed: {e:?}"))
    }

    fn handle(&mut self, event: WireEvent) {
        match event {
            WireEvent::Click { node } if node == BUTTON => self.count += 1,
            WireEvent::Click { node } if node == RESET => self.count = 0,
            // Not an error: the host may address nodes this version does not
            // act on.
            WireEvent::Click { .. } => {}
        }
    }
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), String> {
        let mut counter = Counter { count: 0 };
        counter.commit()?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let event = WireEvent::decode(&payload)
                        .map_err(|e| format!("undecodable host event: {e}"))?;
                    counter.handle(event);
                    counter.commit()?;
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);
