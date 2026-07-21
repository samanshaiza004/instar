//! Containment fixture whose declared root is absent from its node list.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{
    AppError, AppErrorCode, EventBatch, Node, NodeData, PatchBatch, TreeSnapshot,
};

struct InvalidSnapshot;

impl Guest for InvalidSnapshot {
    fn mount() -> Result<TreeSnapshot, AppError> {
        Ok(TreeSnapshot {
            revision: 0,
            root: 99,
            nodes: vec![Node {
                id: 1,
                data: NodeData::Root,
                children: Vec::new(),
            }],
        })
    }

    fn handle(_events: EventBatch) -> Result<PatchBatch, AppError> {
        Err(error())
    }

    fn resync() -> Result<TreeSnapshot, AppError> {
        Err(error())
    }
}

const fn error() -> AppError {
    AppError {
        code: AppErrorCode::InvalidState,
        message: None,
    }
}

export!(InvalidSnapshot);
