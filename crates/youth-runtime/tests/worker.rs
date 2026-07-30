//! Asynchronous actor-boundary integration tests.

mod common;

use common::counter_component;
use youth_runtime::{RuntimeErrorCategory, RuntimeEvent, YouthAppHandle};
use youth_tree::NodeId;

fn id(value: u64) -> NodeId {
    NodeId::new(value).expect("test IDs are nonzero")
}

#[tokio::test(flavor = "multi_thread")]
async fn one_hundred_concurrent_callers_are_serialized_without_loss() {
    let app = YouthAppHandle::spawn_ephemeral(counter_component()).expect("worker starts");
    app.mount().await.expect("mount succeeds");

    let mut calls = Vec::new();
    for _ in 0..100 {
        let app = app.clone();
        calls.push(tokio::spawn(async move { app.activate(id(4)).await }));
    }

    let mut sequences = Vec::new();
    for call in calls {
        let receipt = call
            .await
            .expect("caller task completes")
            .expect("activation succeeds");
        sequences.push(receipt.event_sequence);
    }
    sequences.sort_unstable();
    assert_eq!(sequences, (1..=100).collect::<Vec<_>>());

    let inspection = app.inspect().await.expect("inspection succeeds");
    assert_eq!(inspection.current_revision, Some(100));
    assert_eq!(inspection.last_event_sequence, Some(100));
    assert!(inspection.canonical_tree.contains("text #3 \"Count: 100\""));

    app.stop().await.expect("stop succeeds");
    let error = app.inspect().await.expect_err("worker exits after stop");
    assert_eq!(error.category(), RuntimeErrorCategory::WorkerStopped);
}

#[tokio::test]
async fn verification_request_is_invisible_to_runtime_observers() {
    let app = YouthAppHandle::spawn_ephemeral(counter_component()).expect("worker starts");
    let mut events = app.subscribe();
    app.mount().await.expect("mount succeeds");
    assert!(matches!(
        events.recv().await,
        Ok(RuntimeEvent::SnapshotReplaced(_))
    ));

    let verification = app.verify_view().await.expect("verification succeeds");
    assert_eq!(verification.retained, verification.reconstructed);
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    app.stop().await.expect("stop succeeds");
}
