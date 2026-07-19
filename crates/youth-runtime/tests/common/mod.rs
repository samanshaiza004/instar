//! Shared fixture helpers for runtime integration tests.

use std::path::PathBuf;

/// The canonical tree the counter guest reports at mount (spec section 16).
#[allow(dead_code)] // This shared module is compiled separately for each integration test.
pub const MOUNTED_TREE: &str =
    "root #1\n└── box #2\n    ├── text #3 \"Count: 0\"\n    └── button #4 \"Increment\"\n";

/// Locates the release counter component built for `wasm32-wasip2`.
///
/// # Panics
///
/// Panics with build instructions when the artifact is missing, so a
/// forgotten guest build fails loudly instead of silently skipping.
#[allow(dead_code)] // This shared module is compiled separately for each integration test.
pub fn counter_component() -> PathBuf {
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

/// Locates a release containment component built for `wasm32-wasip2`.
///
/// `package_name` is the Cargo package name, such as
/// `youth-trap-on-mount`; Cargo writes its artifact with underscores.
///
/// # Panics
///
/// Panics with build instructions when the artifact is missing.
#[allow(dead_code)] // This shared module is compiled separately for each integration test.
pub fn test_component(package_name: &str) -> PathBuf {
    let artifact_name = package_name.replace('-', "_") + ".wasm";
    let path = workspace_root()
        .join("target/wasm32-wasip2/release")
        .join(artifact_name);
    assert!(
        path.exists(),
        "test component not found at {}\nbuild it first:\n    \
         cargo build -p {package_name} --target wasm32-wasip2 --release",
        path.display()
    );
    path
}

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves")
}
