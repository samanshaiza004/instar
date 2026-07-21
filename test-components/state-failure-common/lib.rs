//! Shared source for state-write rollback fixtures.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, AppErrorCode, BoxData, ButtonData, EventBatch, Node, NodeData, Patch, PatchBatch,
    SetText, TextData, TreeSnapshot,
};
use youth::state::store;

struct StateFailure;

impl Guest for StateFailure {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
    }

    fn handle(events: EventBatch) -> Result<PatchBatch, AppError> {
        store::set("count", &store::Value::Integer(2)).map_err(|_| error())?;
        let processed_through = events.events.last().map_or(0, |event| event.sequence);
        match env!("CARGO_PKG_NAME") {
            "youth-trap-after-state-write" => panic!("intentional trap after state write"),
            "youth-invalid-patch-after-state-write" => Ok(PatchBatch {
                base_tree_revision: events.tree_revision,
                next_tree_revision: events.tree_revision + 1,
                processed_through,
                patches: vec![Patch::SetText(SetText {
                    id: 4,
                    value: String::from("wrong kind"),
                })],
            }),
            "youth-bad-revision-after-state-write" => Ok(PatchBatch {
                base_tree_revision: events.tree_revision,
                next_tree_revision: events.tree_revision + 2,
                processed_through,
                patches: Vec::new(),
            }),
            "youth-app-error-after-state-write" => Err(AppError {
                code: AppErrorCode::RejectedEvent,
                message: None,
            }),
            _ => unreachable!(),
        }
    }

    fn resync() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
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
                    value: String::from("Count: 1"),
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

export!(StateFailure);
