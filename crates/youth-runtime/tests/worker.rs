//! Asynchronous actor-boundary integration tests.

mod common;

use common::counter_component;
use youth_runtime::{RuntimeErrorCategory, YouthAppHandle};
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
