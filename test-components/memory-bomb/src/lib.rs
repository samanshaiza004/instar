//! Containment fixture that allocates until linear-memory growth is denied.

#![cfg(target_arch = "wasm32")]

wit_bindgen::generate!({
    world: "application",
    path: "../../wit/youth-app",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, AppErrorCode, BoxData, ButtonData, EventBatch, Node, NodeData, PatchBatch, TextData,
    TreeSnapshot,
};

struct MemoryBomb;

impl Guest for MemoryBomb {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(snapshot())
    }

    fn handle(_events: EventBatch) -> Result<PatchBatch, AppError> {
        let mut allocations = Vec::new();
        loop {
            allocations.push(Vec::<u8>::with_capacity(32 * 1024 * 1024));
            core::hint::black_box(&allocations);
        }
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

export!(MemoryBomb);
