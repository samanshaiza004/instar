//! WP7B1 bridge guest: a counter that can also misbehave on request.
//!
//! The acceptance gate needs a guest that does more than count, because the
//! properties under test are about what happens *around* a commit — while bulk
//! host work is in flight, while a generation is dying, while a hundred events
//! are queued behind it. So this fixture's extra buttons are the failure modes,
//! reachable the same way any other interaction is: the host hit-tests a click
//! and sends an event, and nothing about the wire format knows these buttons
//! are special.
//!
//! Every button press ends in a `commit`, and every commit suspends until the
//! main thread answers — which is the round-trip the gate measures.

wit_bindgen::generate!({
    path: "../../../../instar-kernel/wit",
    world: "kernel",
});

use instar_ui_protocol::{
    BatchEncoder, NodeKey, WireDimension, WireEvent, WireLayout, flags, opcode,
};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;
use crate::instar::kernel::ops;

const ROOT: NodeKey = NodeKey(0);
const COLUMN: NodeKey = NodeKey(1);
const LABEL: NodeKey = NodeKey(2);
const COUNT: NodeKey = NodeKey(3);
const RESET: NodeKey = NodeKey(4);
const BULK: NodeKey = NodeKey(5);
const CRASH: NodeKey = NodeKey(6);

/// How long the bulk operation runs. Long enough that it is unambiguously
/// still in flight while the gate measures a click round-trip against it.
const BULK_MILLIS: &str = "3000";

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
    bulk: u32,
}

impl Counter {
    fn encode(&self) -> Vec<u8> {
        // Reset is meaningless at zero, and the host refuses to hit disabled
        // nodes -- a real behavioural difference, not decoration.
        let reset_flags = if self.count == 0 { 0 } else { flags::ENABLED };

        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, ROOT, 0, None, container(8, 0), 1)
            .node(opcode::NODE_COLUMN, COLUMN, 0, None, container(0, 4), 5)
            .node(
                opcode::NODE_TEXT,
                LABEL,
                0,
                Some(&format!(
                    "Clicked {} times, {} bulk",
                    self.count, self.bulk
                )),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                COUNT,
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
            )
            .node(
                opcode::NODE_BUTTON,
                BULK,
                flags::ENABLED,
                Some("Start bulk work"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                CRASH,
                flags::ENABLED,
                Some("Crash"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    /// Suspends until the main thread has applied this interface.
    async fn commit(&self) -> Result<(), String> {
        kernel_ui::commit(self.encode())
            .await
            .map(|_| ())
            .map_err(|e| format!("commit failed: {e:?}"))
    }

    fn handle(&mut self, event: WireEvent) {
        match event {
            WireEvent::Click { node } if node == COUNT => self.count += 1,
            WireEvent::Click { node } if node == RESET => self.count = 0,
            WireEvent::Click { node } if node == BULK => {
                // Started and deliberately *not* awaited. The point is that
                // real host work is in flight on the runtime thread while the
                // guest carries on committing, so a UI round-trip measured
                // against it is measured under genuine concurrent load.
                //
                // The operation therefore stays in the host's registry until
                // the generation is destroyed, which cancels it. That is
                // acceptable for a fixture and is exactly the shape the
                // teardown tests want to see.
                ops::start("delay", BULK_MILLIS.as_bytes());
                self.bulk += 1;
            }
            WireEvent::Click { node } if node == CRASH => {
                panic!("guest trapping on purpose (WP7B1 crash button)");
            }
            // Not an error: the host may address nodes this version does not
            // act on.
            WireEvent::Click { .. } => {}
        }
    }
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), String> {
        let mut counter = Counter { count: 0, bulk: 0 };
        counter.commit().await?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let event = WireEvent::decode(&payload)
                        .map_err(|e| format!("undecodable host event: {e}"))?;
                    counter.handle(event);
                    counter.commit().await?;
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);
