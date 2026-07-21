//! Containment fixture that traps while mounting.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

wit_bindgen::generate!({
    generate_all,
    world: "application",
    path: "../../wit/youth-app",
});

use exports::youth::app::lifecycle::Guest;
use youth::app::ui::{AppError, AppErrorCode, EventBatch, PatchBatch, TreeSnapshot};

struct TrapOnMount;

impl Guest for TrapOnMount {
    fn mount() -> Result<TreeSnapshot, AppError> {
        panic!("intentional mount trap")
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

export!(TrapOnMount);
