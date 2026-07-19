//! Containment tests against deliberately malicious or invalid components.

mod common;

use std::time::{Duration, Instant};

use common::{MOUNTED_TREE, test_component};
use youth_runtime::{AppLifecycle, RuntimeErrorCategory, YouthApp};
use youth_tree::NodeId;

fn button_id() -> NodeId {
    NodeId::new(4).expect("fixture button ID is nonzero")
}

fn assert_faulted_and_poisoned(app: &mut YouthApp) {
    assert_eq!(app.lifecycle(), AppLifecycle::Faulted);
    assert_eq!(
        app.mount()
            .expect_err("mount after fault is rejected")
            .category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
    assert_eq!(
        app.activate(button_id())
            .expect_err("activate after fault is rejected")
            .category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
    assert_eq!(
        app.resync()
            .expect_err("resync after fault is rejected")
            .category(),
        RuntimeErrorCategory::InvalidLifecycle
    );
}

fn assert_mounted_tree_unchanged(app: &YouthApp) {
    let tree = app.tree().expect("fault retains the last committed tree");
    assert_eq!(tree.revision(), 0);
    assert_eq!(tree.canonical(), MOUNTED_TREE);
}

#[test]
fn trap_during_mount_faults_and_poisons_the_instance() {
    let mut app = YouthApp::load(test_component("youth-trap-on-mount"))
        .expect("trap-on-mount component loads");

    let error = app.mount().expect_err("mount trap is contained");

    assert_eq!(error.category(), RuntimeErrorCategory::GuestTrap);
    assert!(app.tree().is_none());
    assert_faulted_and_poisoned(&mut app);
}

#[test]
fn trap_during_handle_faults_without_mutating_the_tree() {
    let mut app = YouthApp::load(test_component("youth-trap-on-handle"))
        .expect("trap-on-handle component loads");
    app.mount().expect("fixture mounts validly");

    let error = app
        .activate(button_id())
        .expect_err("handle trap is contained");

    assert_eq!(error.category(), RuntimeErrorCategory::GuestTrap);
    assert_mounted_tree_unchanged(&app);
    assert_faulted_and_poisoned(&mut app);
}

#[test]
fn invalid_snapshot_faults_and_poisons_the_instance() {
    let mut app = YouthApp::load(test_component("youth-invalid-snapshot"))
        .expect("invalid-snapshot component loads");

    let error = app.mount().expect_err("invalid snapshot is rejected");

    assert_eq!(error.category(), RuntimeErrorCategory::InvalidSnapshot);
    assert!(app.tree().is_none());
    assert_faulted_and_poisoned(&mut app);
}

#[test]
fn invalid_patch_faults_without_mutating_the_tree() {
    let mut app = YouthApp::load(test_component("youth-invalid-patch"))
        .expect("invalid-patch component loads");
    app.mount().expect("fixture mounts validly");

    let error = app
        .activate(button_id())
        .expect_err("invalid patch is rejected");

    assert_eq!(error.category(), RuntimeErrorCategory::InvalidPatchBatch);
    assert_mounted_tree_unchanged(&app);
    assert_faulted_and_poisoned(&mut app);
}

#[test]
fn infinite_loop_is_interrupted_quickly_and_tree_is_unchanged() {
    let mut app = YouthApp::load(test_component("youth-infinite-loop"))
        .expect("infinite-loop component loads");
    app.mount().expect("fixture mounts validly");

    let started = Instant::now();
    let error = app
        .activate(button_id())
        .expect_err("infinite loop is interrupted");
    let elapsed = started.elapsed();

    assert!(
        matches!(
            error.category(),
            RuntimeErrorCategory::FuelExhausted | RuntimeErrorCategory::DeadlineExceeded
        ),
        "unexpected category: {:?}",
        error.category()
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "infinite loop took {elapsed:?} to contain"
    );
    assert_mounted_tree_unchanged(&app);
    assert_faulted_and_poisoned(&mut app);
}

#[test]
fn memory_bomb_hits_the_linear_memory_limit_and_tree_is_unchanged() {
    let mut app =
        YouthApp::load(test_component("youth-memory-bomb")).expect("memory-bomb component loads");
    app.mount().expect("fixture mounts validly");

    let error = app
        .activate(button_id())
        .expect_err("memory bomb is contained");

    assert_eq!(error.category(), RuntimeErrorCategory::MemoryLimitExceeded);
    assert_mounted_tree_unchanged(&app);
    assert_faulted_and_poisoned(&mut app);
}
