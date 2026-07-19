//! Counter guest: exports the `youth:app/application` world.
//!
//! In-memory count is intentionally disposable; restarting the
//! component resets it.

// Component export symbols only link on the WASIp2 target; host-target
// builds of the workspace compile this crate as empty. This is
// build-target gating, not host-OS or host-architecture behavior — the
// emitted component is identical regardless of the machine building it.
#![cfg(all(target_os = "wasi", target_env = "p2"))]

use std::cell::RefCell;

wit_bindgen::generate!({
    world: "application",
    path: "../../wit/youth-app",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, AppErrorCode, BoxData, ButtonData, EventBatch, EventKind, Node, NodeData, Patch,
    PatchBatch, SetText, TextData, TreeSnapshot,
};

const ROOT_ID: u64 = 1;
const BOX_ID: u64 = 2;
const TEXT_ID: u64 = 3;
const BUTTON_ID: u64 = 4;

struct State {
    mounted: bool,
    count: u64,
    revision: u64,
}

thread_local! {
    static STATE: RefCell<State> = const {
        RefCell::new(State {
            mounted: false,
            count: 0,
            revision: 0,
        })
    };
}

struct Counter;

impl Guest for Counter {
    fn mount() -> Result<TreeSnapshot, AppError> {
        with_state(|state| {
            if state.mounted {
                return Err(error(AppErrorCode::InvalidState));
            }

            state.mounted = true;
            Ok(snapshot(state))
        })
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        with_state(|state| {
            if !state.mounted || events.tree_revision != state.revision {
                return Err(error(AppErrorCode::InvalidState));
            }

            if events
                .events
                .iter()
                .any(|event| !matches!(event.kind, EventKind::Activate(BUTTON_ID)))
            {
                return Err(error(AppErrorCode::RejectedEvent));
            }

            let processed_through = events.events.last().map_or(0, |event| event.sequence);
            if events.events.is_empty() {
                return Ok(PatchBatch {
                    base_tree_revision: events.tree_revision,
                    next_tree_revision: events.tree_revision,
                    processed_through,
                    patches: Vec::new(),
                });
            }

            let increment = events.events.len() as u64;
            let next_count = state
                .count
                .checked_add(increment)
                .ok_or_else(|| error(AppErrorCode::Internal))?;
            let next_revision = events
                .tree_revision
                .checked_add(1)
                .ok_or_else(|| error(AppErrorCode::Internal))?;

            state.count = next_count;
            state.revision = next_revision;

            Ok(PatchBatch {
                base_tree_revision: events.tree_revision,
                next_tree_revision: next_revision,
                processed_through,
                patches: vec![Patch::SetText(SetText {
                    id: TEXT_ID,
                    value: count_text(next_count),
                })],
            })
        })
    }

    fn resync() -> Result<TreeSnapshot, AppError> {
        with_state(|state| {
            if !state.mounted {
                return Err(error(AppErrorCode::InvalidState));
            }

            Ok(snapshot(state))
        })
    }
}

fn with_state<T>(operation: impl FnOnce(&mut State) -> Result<T, AppError>) -> Result<T, AppError> {
    STATE.with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return Err(error(AppErrorCode::Internal));
        };
        operation(&mut state)
    })
}

fn snapshot(state: &State) -> TreeSnapshot {
    TreeSnapshot {
        revision: state.revision,
        root: ROOT_ID,
        nodes: vec![
            Node {
                id: ROOT_ID,
                data: NodeData::Root,
                children: vec![BOX_ID],
            },
            Node {
                id: BOX_ID,
                data: NodeData::Box(BoxData { enabled: true }),
                children: vec![TEXT_ID, BUTTON_ID],
            },
            Node {
                id: TEXT_ID,
                data: NodeData::Text(TextData {
                    value: count_text(state.count),
                }),
                children: Vec::new(),
            },
            Node {
                id: BUTTON_ID,
                data: NodeData::Button(ButtonData {
                    label: String::from("Increment"),
                    enabled: true,
                }),
                children: Vec::new(),
            },
        ],
    }
}

fn count_text(count: u64) -> String {
    format!("Count: {count}")
}

const fn error(code: AppErrorCode) -> AppError {
    AppError {
        code,
        message: None,
    }
}

export!(Counter);
