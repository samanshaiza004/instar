use std::fs;
use std::path::PathBuf;

use youth_state::AppId;

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
