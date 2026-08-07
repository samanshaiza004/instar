//! Instar's counter guest.
//!
//! The whole program: describe an interface, suspend, wake on an event,
//! describe it again. There is no loop that polls, no timer, and no thread —
//! between clicks this guest is suspended at `next-event` and costs nothing,
//! which is the premise the whole runtime was built to make true.
//!
//! It also has a button that traps, on purpose. A crash screen that has never
//! been seen is a crash screen that does not work, and the cheapest way to
//! keep this one honest is to make it reachable by clicking.

wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
});

use instar_ui_protocol::{
    BatchEncoder, NodeKey, WireDimension, WireEvent, WireLayout, flags, opcode,
};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;

const ROOT: NodeKey = NodeKey(0);
const COLUMN: NodeKey = NodeKey(1);
const TITLE: NodeKey = NodeKey(2);
const READOUT: NodeKey = NodeKey(3);
const INCREMENT: NodeKey = NodeKey(4);
const RESET: NodeKey = NodeKey(5);
const CRASH: NodeKey = NodeKey(6);

fn container(padding: u16, gap: u16) -> WireLayout {
    WireLayout {
        width: WireDimension::Fill,
        height: WireDimension::Content,
        padding,
        gap,
    }
}

struct Counter {
    count: u64,
}

impl Counter {
    /// The interface, as bytes. Structure and text only: no rectangle appears
    /// anywhere in this function, because the protocol has no way to express
    /// one. The host owns every number on screen.
    fn encode(&self) -> Vec<u8> {
        let readout = match self.count {
            0 => "Not clicked yet".to_string(),
            1 => "Clicked once".to_string(),
            n => format!("Clicked {n} times"),
        };
        // Resetting a zero counter does nothing, and a control that does
        // nothing should look like it. The host refuses to hit disabled nodes
        // outright, so this is a real behavioural difference.
        let reset = if self.count == 0 { 0 } else { flags::ENABLED };

        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, ROOT, 0, None, container(16, 0), 1)
            .node(opcode::NODE_COLUMN, COLUMN, 0, None, container(0, 12), 5)
            .node(
                opcode::NODE_TEXT,
                TITLE,
                0,
                Some("Instar counter"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_TEXT,
                READOUT,
                0,
                Some(&readout),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                INCREMENT,
                flags::ENABLED,
                Some("Click me"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                RESET,
                reset,
                Some("Reset"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                CRASH,
                flags::ENABLED,
                Some("Crash on purpose"),
                WireLayout::default(),
                0,
            );
        encoder.finish()
    }

    /// Suspends until the host has accepted this interface as something it can
    /// show. Resuming means layout succeeded, not merely that the bytes parsed.
    async fn commit(&self) -> Result<(), String> {
        kernel_ui::commit(self.encode())
            .await
            .map(|_| ())
            .map_err(|error| format!("commit failed: {error:?}"))
    }

    fn handle(&mut self, event: WireEvent) {
        match event {
            WireEvent::Click { node } if node == INCREMENT => self.count += 1,
            WireEvent::Click { node } if node == RESET => self.count = 0,
            WireEvent::Click { node } if node == CRASH => {
                panic!("the counter guest trapped because you asked it to");
            }
            // Not an error: the host may address nodes this version of the
            // guest does not act on.
            WireEvent::Click { .. } => {}
        }
    }
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), String> {
        let mut counter = Counter { count: 0 };
        counter.commit().await?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let event = WireEvent::decode(&payload)
                        .map_err(|error| format!("undecodable host event: {error}"))?;
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
