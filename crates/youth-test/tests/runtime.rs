use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use youth_state::AppId;
use youth_test::RunOptions;

#[tokio::test]
async fn sleep_command_blocks_real_runtime_progress() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_tally.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-tally --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("sleep.youth-test");
    fs::write(&test, "mount\nsleep 50\n").unwrap();

    let started = Instant::now();
    youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-sleep-fixture").unwrap(),
    )
    .await
    .unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(50),
        "sleep command returned after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn semantic_dsl_activates_and_persists_through_implicit_restart_mount() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_tally.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-tally --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("persistence.youth-test");
    fs::write(
        &test,
        r#"mount
expect text count "Count: 0"
activate increment
restart
expect text count "Count: 1"
"#,
    )
    .unwrap();

    youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-fixture").unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn convergence_mode_verifies_mount_committed_turn_and_restart() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_tally.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-tally --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("convergence.youth-test");
    fs::write(
        &test,
        r#"mount
activate increment
restart
expect text count "Count: 1"
"#,
    )
    .unwrap();

    youth_test::run_file_with_options(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-convergence-fixture").unwrap(),
        RunOptions {
            verify_view_convergence: true,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn semantic_dsl_seeds_and_independently_asserts_isolated_state() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_tally.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-tally --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("seeded-state.youth-test");
    fs::write(
        &test,
        r#"state integer "count" 41
state text "legacy-label" "old timer"
state boolean "migration-pending" true
state bytes "legacy-payload" "UTF-8 bytes"
mount
expect text count "Count: 41"
expect state integer "count" 41
expect state text "legacy-label" "old timer"
expect state boolean "migration-pending" true
expect state missing "never-created"
activate increment
expect state integer "count" 42
restart
expect text count "Count: 42"
"#,
    )
    .unwrap();

    youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-seed-fixture").unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn semantic_dsl_asserts_countdown_content_kind() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_countdown_presentation.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-countdown-presentation --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let success = directory.path().join("countdown.youth-test");
    fs::write(
        &success,
        r#"mount
activate start
restart
expect countdown remaining
"#,
    )
    .unwrap();

    youth_test::run_file(
        &success,
        &component,
        &AppId::parse("dev.youth.dsl-countdown-fixture").unwrap(),
    )
    .await
    .unwrap();

    let failure = directory.path().join("ordinary-text.youth-test");
    fs::write(
        &failure,
        r#"mount
expect countdown remaining
"#,
    )
    .unwrap();
    let error = youth_test::run_file(
        &failure,
        &component,
        &AppId::parse("dev.youth.dsl-text-fixture").unwrap(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("expected countdown \"remaining\"; observed text(\"--:--\")"),
        "{error}"
    );
}
