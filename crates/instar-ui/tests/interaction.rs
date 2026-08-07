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
use instar_ui::{DecodeError, NodeId, NodeKind, Tree, UiEvent};

macro_rules! run_for {
    ($fut:expr, $window:expr) => {
        tokio::select! {
            biased;
            result = &mut $fut => panic!("guest exited before the test finished: {result:?}"),
            _ = tokio::time::sleep($window) => {}
        }
    };
}

fn guest_component_bytes() -> Vec<u8> {
    std::fs::read(env!("UI_GUEST_WASM")).expect("ui-guest fixture built by build.rs")
}

async fn started() -> (Runtime, RuntimeGeneration, GenerationHandle) {
    let mut runtime = Runtime::new(&guest_component_bytes()).expect("runtime builds");
    let generation = runtime.new_generation().await.expect("generation");
    let handle = generation.handle();
    (runtime, generation, handle)
}

/// The tree the guest most recently committed, decoded.
fn latest_tree(kernel: &instar_kernel::runtime::SharedKernel) -> Tree {
    let commits = kernel.commits();
    let (_, bytes) = commits.last().expect("guest has committed at least once");
    Tree::decode(bytes).expect("guest commits a decodable tree")
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
        let tree = latest_tree(&kernel);
        assert_eq!(
            tree.find(NodeId(1)).map(|n| &n.kind),
            Some(&NodeKind::Label {
                text: "Clicked 0 times".to_string()
            })
        );

        // Hit-test a point over the button, exactly as a windowed host would
        // with a real cursor position.
        let hit = tree.hit_test(20, 50).expect("the button is at (20, 50)");
        assert_eq!(hit.id, NodeId(2));

        // Deliver the click. The host addresses the node it hit -- it does not
        // know or care what the guest will do about it.
        handle
            .send(UiEvent::Click { node: hit.id }.encode())
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));

        assert_eq!(
            latest_tree(&kernel).find(NodeId(1)).map(|n| &n.kind),
            Some(&NodeKind::Label {
                text: "Clicked 1 times".to_string()
            }),
            "the guest should have re-committed with an updated label"
        );

        // Again, to show it accumulates rather than toggling.
        for _ in 0..3 {
            let tree = latest_tree(&kernel);
            let hit = tree.hit_test(20, 50).expect("button still there");
            handle
                .send(UiEvent::Click { node: hit.id }.encode())
                .expect("guest accepts events");
            run_for!(run, Duration::from_millis(200));
        }

        assert_eq!(
            latest_tree(&kernel).find(NodeId(1)).map(|n| &n.kind),
            Some(&NodeKind::Label {
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
        let tree = latest_tree(&kernel);
        assert_eq!(
            tree.find(NodeId(3)).map(|n| &n.kind),
            Some(&NodeKind::Button {
                label: "Reset".to_string(),
                enabled: false
            })
        );
        assert_eq!(
            tree.hit_test(150, 50),
            None,
            "a disabled button must not be hit-testable"
        );

        // Click the counter once; Reset becomes enabled and now hit-tests.
        handle
            .send(UiEvent::Click { node: NodeId(2) }.encode())
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));

        let tree = latest_tree(&kernel);
        assert_eq!(
            tree.hit_test(150, 50).map(|n| n.id),
            Some(NodeId(3)),
            "Reset should be live once there is something to reset"
        );

        // And it works.
        handle
            .send(UiEvent::Click { node: NodeId(3) }.encode())
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));

        assert_eq!(
            latest_tree(&kernel).find(NodeId(1)).map(|n| &n.kind),
            Some(&NodeKind::Label {
                text: "Clicked 0 times".to_string()
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
            .send(UiEvent::Click { node: NodeId(2) }.encode())
            .expect("guest accepts events");
        run_for!(run, Duration::from_millis(300));
        assert_eq!(
            latest_tree(&kernel).find(NodeId(1)).map(|n| &n.kind),
            Some(&NodeKind::Label {
                text: "Clicked 1 times".to_string()
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
            latest_tree(&kernel).find(NodeId(1)).map(|n| &n.kind),
            Some(&NodeKind::Label {
                text: "Clicked 0 times".to_string()
            }),
            "a fresh generation should start from a fresh interface"
        );
    }

    // Every commit across both generations decoded cleanly.
    for (generation_id, bytes) in kernel.commits() {
        assert!(
            Tree::decode(&bytes).is_ok(),
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
    let error = Tree::decode(b"nope").unwrap_err();
    assert_eq!(error, DecodeError::BadMagic);
}
