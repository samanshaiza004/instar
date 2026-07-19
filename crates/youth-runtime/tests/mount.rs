//! Loader and mount integration tests against the real counter component.

mod common;

use common::{MOUNTED_TREE, counter_component};
use youth_runtime::{AppLifecycle, RuntimeErrorCategory, RuntimeLimits, YouthApp};

#[test]
fn loads_and_instantiates_the_counter_component() {
    let app = YouthApp::load(counter_component()).expect("counter component loads");
    assert_eq!(app.lifecycle(), AppLifecycle::Loaded);
    assert!(app.tree().is_none(), "no tree exists before mount");
}

#[test]
fn mount_returns_revision_zero_and_the_canonical_tree() {
    let mut app = YouthApp::load(counter_component()).expect("counter component loads");
    let snapshot = app.mount().expect("mount succeeds");

    assert_eq!(snapshot.revision, 0, "initial snapshot must be revision 0");
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);

    let tree = app.tree().expect("mounted app retains a tree");
    assert_eq!(tree.revision(), 0);
    assert_eq!(tree.node_count(), 4);
    assert_eq!(tree.depth(), 3);
    assert_eq!(tree.canonical(), MOUNTED_TREE);
}

#[test]
fn mount_twice_is_rejected_as_a_lifecycle_error() {
    let mut app = YouthApp::load(counter_component()).expect("counter component loads");
    app.mount().expect("first mount succeeds");

    let error = app.mount().expect_err("second mount is rejected");
    assert_eq!(error.category(), RuntimeErrorCategory::InvalidLifecycle);
    assert_eq!(
        app.lifecycle(),
        AppLifecycle::Mounted,
        "a rejected duplicate mount must not fault the instance"
    );
}

#[test]
fn oversized_component_files_are_rejected_before_compilation() {
    let dir = tempdir();
    let path = dir.join("oversized.wasm");
    std::fs::write(&path, vec![0_u8; 4096]).expect("fixture written");

    let limits = RuntimeLimits {
        max_component_size: 1024,
        ..RuntimeLimits::default()
    };
    let error = YouthApp::load_with_limits(&path, limits).expect_err("oversized file is rejected");
    assert_eq!(error.category(), RuntimeErrorCategory::ComponentTooLarge);
}

#[test]
fn garbage_bytes_are_rejected_without_panicking() {
    let dir = tempdir();
    let path = dir.join("garbage.wasm");
    std::fs::write(&path, b"this is definitely not a wasm component").expect("fixture written");

    let error = YouthApp::load(&path).expect_err("garbage is rejected");
    assert_eq!(error.category(), RuntimeErrorCategory::InvalidComponent);
}

#[test]
fn a_component_without_the_youth_world_is_rejected() {
    // A syntactically valid component that exports nothing Youth needs.
    let empty_component = wat::parse_str(r#"(component)"#).expect("empty component assembles");
    let dir = tempdir();
    let path = dir.join("empty-world.wasm");
    std::fs::write(&path, empty_component).expect("fixture written");

    let error = YouthApp::load(&path).expect_err("wrong world is rejected");
    assert_eq!(error.category(), RuntimeErrorCategory::UnsupportedWorld);
}

#[test]
fn missing_files_are_reported_not_panicked() {
    let error = YouthApp::load("does/not/exist.wasm").expect_err("missing file is rejected");
    assert_eq!(error.category(), RuntimeErrorCategory::InvalidComponent);
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "youth-runtime-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir created");
    dir
}
