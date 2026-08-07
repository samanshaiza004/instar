//! WP5 interaction guest: a counter with a button.
//!
//! Deliberately the smallest thing that exercises the whole round trip —
//! commit a tree, receive a click aimed at a node in it, update state, commit
//! a new tree. No operations, no timers, nothing else in the way.

wit_bindgen::generate!({
    path: "../../../../instar-kernel/wit",
    world: "kernel",
});

use instar_ui::{Node, Rect, Tree, UiEvent};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::RuntimeError;
use crate::instar::kernel::kernel_ui;

const ROOT: u32 = 0;
const LABEL: u32 = 1;
const BUTTON: u32 = 2;
const RESET: u32 = 3;

/// The guest's entire state.
struct Counter {
    count: u32,
}

impl Counter {
    /// Builds the tree for the current state.
    ///
    /// Rects are explicit because WP5 has no layout engine (see instar-ui's
    /// crate docs). A guest with real layout would compute these.
    fn view(&self) -> Tree {
        let reset = Node::button(RESET, Rect::new(120, 40, 70, 30), "Reset");
        // Reset is meaningless at zero, and the host refuses clicks on
        // disabled nodes -- so this is a real behavioural difference, not
        // decoration.
        let reset = if self.count == 0 {
            reset.disabled()
        } else {
            reset
        };

        Tree::new(Node::container(
            ROOT,
            Rect::new(0, 0, 200, 100),
            vec![
                Node::label(
                    LABEL,
                    Rect::new(10, 10, 180, 20),
                    format!("Clicked {} times", self.count),
                ),
                Node::button(BUTTON, Rect::new(10, 40, 100, 30), "Press me"),
                reset,
            ],
        ))
    }

    fn commit(&self) -> Result<(), String> {
        kernel_ui::commit(&self.view().encode())
            .map(|_| ())
            .map_err(|e| format!("commit failed: {e:?}"))
    }

    fn handle(&mut self, event: UiEvent) {
        match event {
            UiEvent::Click { node } if node.0 == BUTTON => self.count += 1,
            UiEvent::Click { node } if node.0 == RESET => self.count = 0,
            // A click on anything else is not an error: the host may know
            // about nodes this version of the guest does not act on.
            UiEvent::Click { .. } => {}
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
                    let event = UiEvent::decode(&payload)
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
