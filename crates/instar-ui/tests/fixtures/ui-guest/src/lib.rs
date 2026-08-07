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

use instar_ui_protocol::{flags, opcode, BatchEncoder, NodeKey, WireEvent, WireRect};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;

const ROOT: NodeKey = NodeKey(0);
const LABEL: NodeKey = NodeKey(1);
const BUTTON: NodeKey = NodeKey(2);
const RESET: NodeKey = NodeKey(3);

struct Counter {
    count: u32,
}

impl Counter {
    /// Encodes the tree for the current state.
    ///
    /// The layout section is WP5 scaffolding: the guest should not be
    /// authoritative over geometry, and once the host computes layout this
    /// goes away without the tree section changing at all.
    fn encode(&self) -> Vec<u8> {
        // Reset is meaningless at zero, and the host refuses to hit disabled
        // nodes -- a real behavioural difference, not decoration.
        let reset_flags = if self.count == 0 { 0 } else { flags::ENABLED };

        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_CONTAINER, ROOT, 0, None, 3)
            .node(
                opcode::NODE_LABEL,
                LABEL,
                0,
                Some(&format!("Clicked {} times", self.count)),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                BUTTON,
                flags::ENABLED,
                Some("Press me"),
                0,
            )
            .node(opcode::NODE_BUTTON, RESET, reset_flags, Some("Reset"), 0)
            .layout_entry(ROOT, WireRect::new(0, 0, 200, 100))
            .layout_entry(LABEL, WireRect::new(10, 10, 180, 20))
            .layout_entry(BUTTON, WireRect::new(10, 40, 100, 30))
            .layout_entry(RESET, WireRect::new(120, 40, 70, 30));
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
