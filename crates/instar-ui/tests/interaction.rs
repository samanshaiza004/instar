//! WP5: a button, end to end.
//!
//! The loop these tests prove, with nothing simulated in the middle:
//!
//! ```text
//! guest commits a tree  ->  host decodes it  ->  host hit-tests a point
//!        ^                                              |
//!        |                                              v
//! guest updates state   <-  host delivers a click event for the hit node
//! ```
//!
//! A real `wasm32-wasip2` guest runs inside a real `instar-kernel` generation.
//! The host never inspects guest memory or calls guest functions directly; it
//! only sees committed bytes and only replies with encoded events, which is
//! exactly the contract a windowed host will have in WP6/WP7.

use std::time::Duration;

use instar_kernel::runtime::{GenerationHandle, Runtime, RuntimeGeneration};
use instar_ui::protocol::decode_batch;
use instar_ui::{
    LayoutSnapshot, NodeKey, NodeKind, ProtocolError, TextEngine, Tree, TreeError, UiAction,
    Viewport,
};

macro_rules! run_for {
    ($fut:expr, $window:expr) => {
        tokio::select! {
            biased;
            result = &mut $fut => panic!("guest exited before the test finished: {result:?}"),
            _ = tokio::time::sleep($window) => {}
        }
    };
}

/// The counter guest's node keys, as a host learns them: off the wire, with
/// no shared header. Named here so a change to the guest's structure fails
/// with a name rather than silently retargeting a click at a different button.
const READOUT: NodeKey = NodeKey::first(3);
const INCREMENT: NodeKey = NodeKey::first(4);
const RESET: NodeKey = NodeKey::first(5);

/// The guest the shell actually ships (WP8).
///
/// These tests used to run against a fixture that was a near-copy of it. A UI
/// contract worth testing is worth testing against the program people run —
/// a fixture that drifts from the real guest tests something nobody has.
fn guest_component_bytes() -> Vec<u8> {
    std::fs::read(env!("COUNTER_WASM")).expect("the counter guest is built by build.rs")
}

async fn started() -> (Runtime, RuntimeGeneration, GenerationHandle) {
    let mut runtime = Runtime::new(&guest_component_bytes()).expect("runtime builds");
    let generation = runtime.new_generation().await.expect("generation");
    let handle = generation.handle();
    (runtime, generation, handle)
}

/// The viewport these tests lay out against. A host would take this from
/// `WindowMetricsChanged`; nothing here needs a real window.
const VIEWPORT: Viewport = Viewport::new(400.0, 300.0);

/// The tree the guest most recently committed, plus the geometry *the host*
/// computed for it.
///
/// The guest sends no rectangles. Every coordinate these tests hit-test
/// against was produced here, which is the WP7A exit gate in one function.
fn latest(kernel: &instar_kernel::runtime::SharedKernel) -> (Tree, LayoutSnapshot) {
    let commits = kernel.commits();
    let (_, bytes) = commits.last().expect("guest has committed at least once");
    let batch = decode_batch(bytes).expect("guest commits a decodable batch");
    let tree = Tree::from_wire(&batch).expect("guest commits a meaningful tree");
    let mut text = TextEngine::new();
    let layout = tree.layout(&mut text, VIEWPORT);
    (tree, layout)
}

/// Hit-tests the centre of whatever the host placed at `key`.
fn click_point(layout: &LayoutSnapshot, key: NodeKey) -> (i32, i32) {
    let rect = layout
        .get(key)
        .unwrap_or_else(|| panic!("{key} should have host-computed geometry"));
    (rect.x + rect.width / 2, rect.y + rect.height / 2)
}

/// The whole point of WP5: click a button, watch the guest's state change.
#[tokio::test]
async fn clicking_a_button_updates_the_committed_tree() {
    let (mut runtime, mut generation, handle) = started().await;
    let kernel = runtime.kernel();

    {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(300));

        // The guest's first commit describes its initial interface.
        let (tree, layout) = latest(&kernel);
        assert_eq!(
            tree.find(READOUT).map(|n| &n.kind),
            Some(&NodeKind::Text {
                text: "Not clicked yet".to_string()
            })
        );

        // Hit-test the button, exactly as a windowed host would with a real
        // cursor position -- at coordinates the *host* computed, since the
        // guest supplied none.
        let (x, y) = click_point(&layout, INCREMENT);
        let hit = tree
            .hit_test(&layout, x, y)
            .expect("the button the host laid out should be hit-testable");
        assert_eq!(hit.key, INCREMENT);

        // Deliver the click. The host addresses the node it hit -- it does not
        // know or care what the guest will do about it.
        handle
            .send(UiAction::ButtonActivated(hit.key).encode())
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));

        assert_eq!(
            latest(&kernel).0.find(READOUT).map(|n| &n.kind),
            Some(&NodeKind::Text {
                text: "Clicked once".to_string()
            }),
            "the guest should have re-committed with an updated label"
        );

        // Again, to show it accumulates rather than toggling.
        for _ in 0..3 {
            let (tree, layout) = latest(&kernel);
            let (x, y) = click_point(&layout, INCREMENT);
            let hit = tree.hit_test(&layout, x, y).expect("button still there");
            handle
                .send(UiAction::ButtonActivated(hit.key).encode())
                .expect("guest accepts events");
            run_for!(run, Duration::from_millis(200));
        }

        assert_eq!(
            latest(&kernel).0.find(READOUT).map(|n| &n.kind),
            Some(&NodeKind::Text {
                text: "Clicked 4 times".to_string()
            })
        );
    }

    runtime.destroy_generation(generation);
}

/// State the guest expresses in its tree is enforced by the *host*: a disabled
/// node is not hit, so no click is ever synthesized for it.
#[tokio::test]
async fn a_disabled_button_cannot_be_clicked() {
    let (mut runtime, mut generation, handle) = started().await;
    let kernel = runtime.kernel();

    {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(300));

        // Reset starts disabled, because the count is zero.
        let (tree, layout) = latest(&kernel);
        assert_eq!(
            tree.find(RESET).map(|n| &n.kind),
            Some(&NodeKind::Button {
                label: "Reset".to_string(),
                enabled: false
            })
        );
        let (x, y) = click_point(&layout, RESET);
        assert_eq!(
            tree.hit_test(&layout, x, y),
            None,
            "a disabled button must not be hit-testable, even though the host \
             gave it geometry"
        );

        // Click the counter once; Reset becomes enabled and now hit-tests.
        handle
            .send(UiAction::ButtonActivated(INCREMENT).encode())
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));

        let (tree, layout) = latest(&kernel);
        let (x, y) = click_point(&layout, RESET);
        assert_eq!(
            tree.hit_test(&layout, x, y).map(|n| n.key),
            Some(RESET),
            "Reset should be live once there is something to reset"
        );

        // And it works.
        handle
            .send(UiAction::ButtonActivated(RESET).encode())
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));

        assert_eq!(
            latest(&kernel).0.find(READOUT).map(|n| &n.kind),
            Some(&NodeKind::Text {
                text: "Not clicked yet".to_string()
            })
        );
    }

    runtime.destroy_generation(generation);
}

/// Every commit the guest makes is decodable, and interaction survives a
/// generation restart with the state reset that implies.
#[tokio::test]
async fn a_new_generation_starts_from_a_clean_interface() {
    let (mut runtime, mut generation, handle) = started().await;
    let kernel = runtime.kernel();

    {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(300));
        handle
            .send(UiAction::ButtonActivated(INCREMENT).encode())
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));
        assert_eq!(
            latest(&kernel).0.find(READOUT).map(|n| &n.kind),
            Some(&NodeKind::Text {
                text: "Clicked once".to_string()
            })
        );
    }

    // Tear the generation down and start another. Guest state is in the guest,
    // so it goes with it -- WP4's lifetime rule, observed through the UI.
    runtime.destroy_generation(generation);
    let mut generation = runtime.new_generation().await.expect("generation");
    {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(300));
        assert_eq!(
            latest(&kernel).0.find(READOUT).map(|n| &n.kind),
            Some(&NodeKind::Text {
                text: "Not clicked yet".to_string()
            }),
            "a fresh generation should start from a fresh interface"
        );
    }

    // Every commit across both generations decoded cleanly.
    for (generation_id, bytes) in kernel.commits() {
        assert!(
            decode_batch(&bytes).is_ok(),
            "{generation_id} committed an undecodable tree"
        );
    }
    assert_eq!(kernel.stale_commits_rejected(), 0);

    runtime.destroy_generation(generation);
}

/// A guest that sends nonsense is rejected, not trusted.
///
/// The host-side decoder is the boundary here; this test exists to keep the
/// integration honest about the fact that a real guest's bytes go through the
/// same adversarial path the unit tests cover.
#[tokio::test]
async fn malformed_host_events_are_rejected_by_the_guest() {
    let (mut runtime, mut generation, handle) = started().await;

    let outcome = {
        let mut run = std::pin::pin!(generation.run());
        run_for!(run, Duration::from_millis(300));

        // Not a valid event encoding.
        handle.send(vec![0xde, 0xad, 0xbe, 0xef]).expect("queued");

        tokio::time::timeout(Duration::from_secs(5), &mut run)
            .await
            .expect("guest should fail fast on an undecodable event, not hang")
    };

    let message = outcome
        .expect("an undecodable event is a guest-side error, not a host trap")
        .expect_err("the guest should refuse to act on an event it cannot decode");
    assert!(
        message.contains("undecodable host event"),
        "expected a decode failure, got: {message}"
    );

    runtime.destroy_generation(generation);
}

/// Sanity check that the error type is reachable from the integration surface,
/// so a future refactor cannot quietly make decode failures unrepresentable.
#[test]
fn decode_errors_are_public() {
    // Wire failures and semantic failures stay distinguishable from the
    // integration surface, which is the whole reason they are separate types.
    assert!(matches!(
        Tree::decode(b"nope"),
        Err(TreeError::Protocol(ProtocolError::BadMagic))
    ));
}
