//! Containment fixture that applies set-text to a button node.

#![cfg(target_arch = "wasm32")]

wit_bindgen::generate!({
    world: "application",
    path: "../../wit/youth-app",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, AppErrorCode, BoxData, ButtonData, EventBatch, Node, NodeData, Patch, PatchBatch,
    SetText, TextData, TreeSnapshot,
};

struct InvalidPatch;

impl Guest for InvalidPatch {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        Ok(PatchBatch {
            base_tree_revision: events.tree_revision,
            next_tree_revision: events.tree_revision + 1,
            processed_through: events.events.last().map_or(0, |event| event.sequence),
            patches: vec![Patch::SetText(SetText {
                id: 4,
                value: String::from("wrong kind"),
            })],
        })
    }

    fn resync() -> Result<TreeSnapshot, AppError> {
        Err(error())
    }
}

fn snapshot() -> TreeSnapshot {
    TreeSnapshot {
        revision: 0,
        root: 1,
        nodes: vec![
            Node {
                id: 1,
                data: NodeData::Root,
                children: vec![2],
            },
            Node {
                id: 2,
                data: NodeData::Box(BoxData { enabled: true }),
                children: vec![3, 4],
            },
            Node {
                id: 3,
                data: NodeData::Text(TextData {
                    value: String::from("Count: 0"),
                }),
                children: Vec::new(),
            },
            Node {
                id: 4,
                data: NodeData::Button(ButtonData {
                    label: String::from("Increment"),
                    enabled: true,
                }),
                children: Vec::new(),
            },
        ],
    }
}

const fn error() -> AppError {
    AppError {
        code: AppErrorCode::InvalidState,
        message: None,
    }
}

export!(InvalidPatch);
