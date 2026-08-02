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

#[tokio::test]
async fn advance_time_delivers_a_due_schedule_without_a_wall_clock_wait() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_countdown_presentation.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-countdown-presentation --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("advance-time.youth-test");
    fs::write(
        &test,
        r#"youth-test 1

mount
expect text remaining "--:--"
invoke start
restart
expect countdown remaining
expect state missing "elapsed-count"
advance time 300000ms
expect state integer "elapsed-count" 1
"#,
    )
    .unwrap();

    let started = Instant::now();
    youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-advance-time-fixture").unwrap(),
    )
    .await
    .unwrap();
    // The fixture's schedule is 300 real seconds out; a genuine regression
    // back to a real wait would take at least that long. This bound is
    // deliberately set close to that ceiling (not a tight latency budget)
    // because in-process work alone -- component instantiation, mailbox
    // round-trips -- has been observed on a heavily contended machine to
    // take minutes even with no real wait involved anywhere in the path;
    // 280s still leaves a real regression nowhere to hide (it would need
    // at least ~300s) while tolerating that contention.
    assert!(
        started.elapsed() < Duration::from_secs(280),
        "advance time took {:?}; it must not wait on real elapsed time",
        started.elapsed()
    );
}

#[tokio::test]
async fn sleep_real_also_blocks_real_runtime_progress() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_tally.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-tally --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("sleep-real.youth-test");
    fs::write(&test, "mount\nsleep real 50ms\n").unwrap();

    let started = Instant::now();
    youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-sleep-real-fixture").unwrap(),
    )
    .await
    .unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(50),
        "sleep real command returned after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn click_requires_presence_enabled_state_and_button_role() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_tally.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-tally --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();

    let success = directory.path().join("click.youth-test");
    fs::write(
        &success,
        "mount\nexpect text count \"Count: 0\"\nclick increment\nexpect text count \"Count: 1\"\n",
    )
    .unwrap();
    youth_test::run_file(
        &success,
        &component,
        &AppId::parse("dev.youth.dsl-click-fixture").unwrap(),
    )
    .await
    .unwrap();

    let non_button = directory.path().join("click-non-button.youth-test");
    fs::write(&non_button, "mount\nclick count\n").unwrap();
    let error = youth_test::run_file(
        &non_button,
        &component,
        &AppId::parse("dev.youth.dsl-click-non-button-fixture").unwrap(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("no activatable role"), "{error}");

    let absent = directory.path().join("click-absent.youth-test");
    fs::write(&absent, "mount\nclick \"does-not-exist\"\n").unwrap();
    let error = youth_test::run_file(
        &absent,
        &component,
        &AppId::parse("dev.youth.dsl-click-absent-fixture").unwrap(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("is not present"), "{error}");
}

#[tokio::test]
async fn type_replace_selection_paste_and_compose_drive_a_real_editor_session() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_editor.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-editor --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("editor.youth-test");
    fs::write(
        &test,
        r#"youth-test 1

mount
expect text document ""
expect editor text document ""

type document "Hello"
expect editor text document "Hello"
expect editor selection document graphemes 5..5
# Host-local edits never touch the retained tree -- it stays whatever
# the last full view()-derived install declared, proving the two reads
# are genuinely independent.
expect text document ""

replace-selection document " World"
expect editor text document "Hello World"

paste document "!!"
expect editor text document "Hello World!!"

compose document start "~"
compose document cancel
expect editor text document "Hello World!!"

compose document commit "?"
expect editor text document "Hello World!!?"
"#,
    )
    .unwrap();

    youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-editor-fixture").unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn editor_selection_is_reported_in_grapheme_clusters_not_bytes() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_editor.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-editor --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("editor-graphemes.youth-test");
    fs::write(
        &test,
        // "Hi, 世界" is 6 grapheme clusters but 10 UTF-8 bytes; asserting
        // graphemes 6..6 (not bytes 10..10) is the whole point of this
        // test -- a naive byte-offset comparison here would be wrong.
        "mount\ntype document \"Hi, \u{4e16}\u{754c}\"\nexpect editor selection document graphemes 6..6\nexpect editor text document \"Hi, \u{4e16}\u{754c}\"\n",
    )
    .unwrap();

    youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-editor-grapheme-fixture").unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn measure_proves_host_local_typing_makes_zero_guest_turns_and_save_makes_one() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_editor.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-editor --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("measure.youth-test");
    fs::write(
        &test,
        r#"youth-test 1

mount

measure begin "typing"
type document "clean architecture"
replace-selection document " over clever tricks"
paste document "!"
measure expect "typing" guest-turns 0

measure begin "save"
invoke save
measure expect "save" guest-turns 1
"#,
    )
    .unwrap();

    youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-measure-fixture").unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn measure_expect_reports_a_clear_mismatch() {
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-wasip2/release/youth_sdk_editor.wasm");
    assert!(
        component.is_file(),
        "build the fixture first: cargo build -p youth-sdk-editor --target wasm32-wasip2 --release"
    );
    let directory = tempfile::tempdir().unwrap();
    let test = directory.path().join("measure-mismatch.youth-test");
    fs::write(
        &test,
        "mount\nmeasure begin \"save\"\ninvoke save\nmeasure expect \"save\" guest-turns 0\n",
    )
    .unwrap();

    let error = youth_test::run_file(
        &test,
        &component,
        &AppId::parse("dev.youth.dsl-measure-mismatch-fixture").unwrap(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("expected measurement \"save\" to record 0 guest turns; observed 1"),
        "{error}"
    );
}
