//! Containment fixture that mounts validly and traps while handling an event.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    world: "application",
    path: "../../wit/youth-app",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, AppErrorCode, BoxData, ButtonData, EventBatch, Node, NodeData, PatchBatch, TextData,
    TreeSnapshot,
};

struct TrapOnHandle;

impl Guest for TrapOnHandle {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
    }

    fn handle(_events: EventBatch) -> Result<PatchBatch, AppError> {
        panic!("intentional handle trap")
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

export!(TrapOnHandle);
