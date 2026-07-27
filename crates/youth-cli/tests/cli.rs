//! End-to-end tests for the stable CLI stdout contract.

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::process::{Command, Output};
use tempfile::tempdir;

const MOUNTED_TREE: &str =
    "root #1\n└── box #2\n    ├── text #3 \"Count: 0\"\n    └── button #4 \"Increment\"\n";

const ACTIVATED_TREE: &str =
    "root #1\n└── box #2\n    ├── text #3 \"Count: 1\"\n    └── button #4 \"Increment\"\n";

#[test]
fn mount_output_is_exact() {
    let component = counter_component();
    let output = youth(["mount", path_str(&component)]);
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "component: youth_counter.wasm\n\
             world: youth:app/application@0.0.2\n\
             lifecycle: mounted\n\
             revision: 0\n\
             nodes: 4\n\
             depth: 3\n\n{MOUNTED_TREE}"
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn activate_output_is_exact() {
    let component = counter_component();
    let output = youth(["activate", path_str(&component), "--node", "4"]);
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "turn: 1\n\
             event sequence: 1\n\
             base revision: 0\n\
             next revision: 1\n\
             patches: 1\n\
             status: committed\n\n{ACTIVATED_TREE}"
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn script_output_equals_committed_fixture() {
    let component = counter_component();
    let script = workspace_root().join("tests/scripts/three-clicks.json");
    let output = youth(["script", path_str(&component), path_str(&script)]);
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("../../../tests/fixtures/three-clicks.expected")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn explicit_headless_state_persists_and_maintenance_prints_no_values() {
    let component = counter_component();
    let directory = tempdir().unwrap();
    let root = path_str(directory.path());
    let activated = youth([
        "activate",
        path_str(&component),
        "--node",
        "4",
        "--app-id",
        "dev.youth.counter",
        "--state-dir",
        root,
    ]);
    assert_success(&activated);

    let mounted = youth([
        "mount",
        path_str(&component),
        "--app-id",
        "dev.youth.counter",
        "--state-dir",
        root,
    ]);
    assert_success(&mounted);
    assert!(
        String::from_utf8(mounted.stdout)
            .unwrap()
            .contains("Count: 1")
    );

    let inspected = youth([
        "state",
        "inspect",
        "--app-id",
        "dev.youth.counter",
        "--state-dir",
        root,
    ]);
    assert_success(&inspected);
    assert_eq!(
        String::from_utf8(inspected.stdout).unwrap(),
        "app: dev.youth.counter\nschema: 3\nkeys: 1\nbytes: 45\n"
    );

    let verified = youth([
        "state",
        "verify",
        "--app-id",
        "dev.youth.counter",
        "--state-dir",
        root,
    ]);
    assert_success(&verified);
    let verification = String::from_utf8(verified.stdout).unwrap();
    assert!(verification.contains("integrity: ok"));
    assert!(verification.contains("usage: ok"));
    assert!(!verification.contains("Count: 1"));

    let refused = youth([
        "state",
        "reset",
        "--app-id",
        "dev.youth.counter",
        "--state-dir",
        root,
    ]);
    assert!(!refused.status.success());

    let reset = youth([
        "state",
        "reset",
        "--app-id",
        "dev.youth.counter",
        "--state-dir",
        root,
        "--yes",
    ]);
    assert_success(&reset);
}

#[test]
fn supervised_headless_child_mounts_and_stops_from_stdin() {
    let component = counter_component();
    let directory = tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_youth"))
        .args([
            "__dev-child",
            path_str(&component),
            "--app-id",
            "dev.youth.supervisor-test",
            "--state-dir",
            path_str(directory.path()),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("headless child starts");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"shutdown\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "dev child: mounted\n"
    );
    assert!(
        directory
            .path()
            .join("dev.youth.supervisor-test/state.sqlite3")
            .is_file()
    );
}

fn youth<const N: usize>(args: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_youth"))
        .args(args)
        .env_remove("RUST_LOG")
        .output()
        .expect("youth CLI runs")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "CLI failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("fixture path is UTF-8")
}

fn counter_component() -> PathBuf {
    let path = workspace_root()
        .join("target/wasm32-wasip2/release")
        .join("youth_counter.wasm");
    assert!(
        path.exists(),
        "counter component not found at {}\nbuild it first:\n    \
         cargo build -p youth-counter --target wasm32-wasip2 --release",
        path.display()
    );
    path
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}
