//! Gate 0 spike guest.
//!
//! Implements the `kernel-spike` world: submit an initial commit, then loop
//! on `next-event` until the host reports shutdown. Between events it obeys a
//! tiny command protocol (see [`handle`]) so that a *single* fixture can serve
//! every Gate 0 test rather than needing one guest binary per gate.
//!
//! The command protocol is spike-only and deliberately dumb (ASCII, no
//! versioning, no framing). It is not a draft of the real Instar protocol.

wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel-spike",
});

use crate::instar::kernel::runtime;
use crate::instar::kernel::test_support;
use crate::instar::kernel::types::RuntimeError;
use crate::instar::kernel::ui;

struct Component;

/// Interprets one event payload and returns the bytes to commit in response.
///
/// - `echo:<rest>`      -> commits `<rest>` unchanged. The trivial round-trip,
///   used by the suspend/wake gate where the point is the *wake*, not the work.
/// - `delay:<millis>`   -> awaits `test-support.delay`, then commits
///   `delayed:<millis>`. A single in-flight async import.
/// - `join:<a>,<b>`     -> awaits delay(a) and delay(b) *concurrently* and
///   commits `joined:<first>,<second>` naming them in completion order. This is
///   the one that matters: if the Component Model serialized these, a
///   long-then-short pair would complete in issue order, and the test asserts
///   it does not.
async fn handle(payload: Vec<u8>) -> Result<Vec<u8>, String> {
    let text = String::from_utf8(payload).map_err(|e| format!("non-utf8 command: {e}"))?;

    if let Some(rest) = text.strip_prefix("echo:") {
        return Ok(rest.as_bytes().to_vec());
    }

    if let Some(rest) = text.strip_prefix("delay:") {
        let millis: u32 = rest.parse().map_err(|e| format!("bad delay millis: {e}"))?;
        let got = test_support::delay(millis).await;
        return Ok(format!("delayed:{got}").into_bytes());
    }

    if let Some(rest) = text.strip_prefix("join:") {
        let (a, b) = rest
            .split_once(',')
            .ok_or_else(|| format!("join needs two millis values, got {rest:?}"))?;
        let a: u32 = a.parse().map_err(|e| format!("bad join millis: {e}"))?;
        let b: u32 = b.parse().map_err(|e| format!("bad join millis: {e}"))?;

        // Not `join!`-and-report-both: we specifically want *completion order*,
        // since that's the observable that distinguishes real concurrency from
        // sequential execution. `select`-style biasing would hide it, so race
        // the two and then drain the loser.
        let delay_a = core::pin::pin!(test_support::delay(a));
        let delay_b = core::pin::pin!(test_support::delay(b));
        let raced = futures::future::select(delay_a, delay_b).await;

        let (first, second) = match raced {
            futures::future::Either::Left((first, rest)) => (first, rest.await),
            futures::future::Either::Right((first, rest)) => (first, rest.await),
        };

        return Ok(format!("joined:{first},{second}").into_bytes());
    }

    Err(format!("unknown command: {text:?}"))
}

impl Guest for Component {
    async fn run() -> Result<(), String> {
        // Proves a synchronous host import is callable from inside an async
        // guest task before any suspension has happened.
        ui::commit(b"ready").map_err(|e| format!("initial commit failed: {e:?}"))?;

        loop {
            match runtime::next_event().await {
                Ok(payload) => {
                    let response = handle(payload).await?;
                    ui::commit(&response).map_err(|e| format!("commit failed: {e:?}"))?;
                }
                // The host is done sending events; unwind the loop and let
                // `run` return normally. A clean return here (rather than a
                // trap) is itself part of what the shutdown gate checks.
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);
