//! WP4 lifecycle guest fixture.
//!
//! Implements the `kernel` world. Like the Gate 0 fixture, it obeys a tiny
//! ASCII command protocol so one guest binary can serve every lifecycle test.
//! Unlike that one, this fixture's commands are about *operations* and about
//! dying badly on purpose.

wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
    // The world now spans two packages: instar:kernel and the optional
    // instar:text capability. Without this, types from the second are an
    // error rather than generated bindings.
    generate_all,
});

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::{OpError, RuntimeError};
use crate::instar::kernel::kernel_ui;
use crate::instar::kernel::ops;

struct Component;

fn render_op_error(error: OpError) -> String {
    match error {
        OpError::Cancelled => "cancelled".to_string(),
        OpError::Unknown => "unknown".to_string(),
        OpError::Failed(message) => format!("failed({message})"),
    }
}

/// Interprets one event payload and returns bytes to commit.
///
/// - `echo:<rest>`        -> commit `<rest>`.
/// - `op:<kind>:<payload>`-> start an operation, await it, commit the result.
/// - `op-cancel:<millis>` -> start a long operation, ask the host to cancel it,
///   then await it. Commits `op-cancelled`, and critically **keeps running
///   afterwards** — the point of the test is that per-operation cancellation
///   leaves the guest task alive.
/// - `op-unknown`         -> await an id that was never issued.
/// - `trap`               -> panic, which traps the guest. Used to prove the
///   host tears the generation down and starts a fresh one.
async fn handle(payload: Vec<u8>) -> Result<Vec<u8>, String> {
    let text = String::from_utf8(payload).map_err(|e| format!("non-utf8 command: {e}"))?;

    if let Some(rest) = text.strip_prefix("echo:") {
        return Ok(rest.as_bytes().to_vec());
    }

    if text == "trap" {
        // A deliberate trap. `unreachable` in wasm terms once it unwinds.
        panic!("guest trapping on purpose (trap fixture)");
    }

    if text == "op-unknown" {
        let result = ops::await_op(9_999_999).await;
        return Ok(match result {
            Ok(bytes) => format!("unexpected-ok:{}", String::from_utf8_lossy(&bytes)),
            Err(error) => format!("op-error:{}", render_op_error(error)),
        }
        .into_bytes());
    }

    if let Some(rest) = text.strip_prefix("op-cancel:") {
        let id = ops::start("delay", rest.as_bytes());
        // Cancellation is requested while the operation is genuinely in
        // flight, and is a plain synchronous call -- no suspension, no
        // dropping of anything on the host side.
        let cancelled = ops::cancel(id);
        let result = ops::await_op(id).await;
        let outcome = match result {
            Ok(bytes) => format!("unexpected-ok:{}", String::from_utf8_lossy(&bytes)),
            Err(error) => render_op_error(error),
        };
        return Ok(format!("op-cancel:requested={cancelled},outcome={outcome}").into_bytes());
    }

    if let Some(rest) = text.strip_prefix("op:") {
        let (kind, payload) = rest
            .split_once(':')
            .ok_or_else(|| format!("op needs kind:payload, got {rest:?}"))?;
        let id = ops::start(kind, payload.as_bytes());
        let result = ops::await_op(id).await;
        return Ok(match result {
            Ok(bytes) => format!("op-ok:{}", String::from_utf8_lossy(&bytes)),
            Err(error) => format!("op-error:{}", render_op_error(error)),
        }
        .into_bytes());
    }

    Err(format!("unknown command: {text:?}"))
}

impl Guest for Component {
    async fn run() -> Result<(), String> {
        kernel_ui::commit(b"ready".to_vec())
            .await
            .map_err(|e| format!("initial commit failed: {e:?}"))?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let response = handle(payload).await?;
                    kernel_ui::commit(response)
                        .await
                        .map_err(|e| format!("commit failed: {e:?}"))?;
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);
