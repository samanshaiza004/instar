use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use youth_project::{CLI_VERSION, Project, SUPPORTED_PROTOCOL};
use youth_state::AppId;

use crate::CliError;

const TOOLCHAIN: &str = include_str!("../templates/tally/rust-toolchain.toml");

const TEMPLATE_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("../templates/tally/Cargo.toml")),
    ("Cargo.lock", include_str!("../templates/tally/Cargo.lock")),
    ("Youth.toml", include_str!("../templates/tally/Youth.toml")),
    ("Youth.lock", include_str!("../templates/tally/Youth.lock")),
    ("rust-toolchain.toml", TOOLCHAIN),
    (
        ".cargo/config.toml",
        include_str!("../templates/tally/.cargo/config.toml"),
    ),
    (".gitignore", include_str!("../templates/tally/.gitignore")),
    ("README.md", include_str!("../templates/tally/README.md")),
    ("src/lib.rs", include_str!("../templates/tally/src/lib.rs")),
    (
        "tests/basic.youth-test",
        include_str!("../templates/tally/tests/basic.youth-test"),
    ),
    (
        "wit/youth/youth-app.wit",
        include_str!("../templates/tally/wit/youth/youth-app.wit"),
    ),
    (
        "wit/youth/deps/youth-state/store.wit",
        include_str!("../templates/tally/wit/youth/deps/youth-state/store.wit"),
    ),
];

pub fn new_project(destination: &Path, app_id: &AppId) -> Result<(), CliError> {
    if destination.exists() {
        return Err(message(format!(
            "refusing to overwrite existing destination {}",
            destination.display()
        )));
    }
    let file_name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| message("destination must have a UTF-8 final path component"))?;
    let package = cargo_package_name(file_name)?;
    let display_name = display_name(file_name);
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(message(format!(
            "destination parent does not exist: {}",
            parent.display()
        )));
    }

    let temporary = unique_temporary_sibling(parent, file_name)?;
    let result = write_template(&temporary, &package, &display_name, app_id)
        .and_then(|()| fs::rename(&temporary, destination).map_err(|error| io(destination, error)));
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result?;
    println!("created: {}", destination.display());
    println!("app ID: {app_id}");
    println!("next: cd {} && youth check", destination.display());
    Ok(())
}

fn unique_temporary_sibling(parent: &Path, name: &str) -> Result<PathBuf, CliError> {
    for attempt in 0..100_u32 {
        let candidate = parent.join(format!(
            ".{name}.youth-new-{}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io(&candidate, error)),
        }
    }
    Err(message("could not allocate a temporary sibling directory"))
}

fn write_template(
    root: &Path,
    package: &str,
    display_name: &str,
    app_id: &AppId,
) -> Result<(), CliError> {
    for (relative, template) in TEMPLATE_FILES {
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
        }
        let rendered = template
            .replace("{{package}}", package)
            .replace("{{display_name}}", display_name)
            .replace("{{app_id}}", app_id.as_str());
        fs::write(&destination, rendered).map_err(|error| io(&destination, error))?;
    }
    Ok(())
}

fn cargo_package_name(name: &str) -> Result<String, CliError> {
    let mut package = String::with_capacity(name.len());
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            package.push(character.to_ascii_lowercase());
            separator = false;
        } else if !separator && !package.is_empty() {
            package.push('-');
            separator = true;
        }
    }
    while package.ends_with('-') {
        package.pop();
    }
    if package.is_empty() {
        return Err(message(
            "destination name cannot produce a Rust package name",
        ));
    }
    if package.as_bytes()[0].is_ascii_digit() {
        package.insert_str(0, "app-");
    }
    Ok(package)
}

fn display_name(name: &str) -> String {
    let mut result = String::new();
    for word in name.split(|character: char| !character.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if !result.is_empty() {
            result.push(' ');
        }
        let mut characters = word.chars();
        if let Some(first) = characters.next() {
            result.extend(first.to_uppercase());
            result.extend(characters);
        }
    }
    if result.is_empty() {
        "Youth App".into()
    } else {
        result
    }
}

pub fn doctor(full: bool) -> Result<(), CliError> {
    let required_rust = embedded_toolchain_channel()?;
    println!("Youth CLI: {CLI_VERSION}");
    println!("template: 1");
    println!("protocol: {SUPPORTED_PROTOCOL}");
    probe_tool(
        "cargo",
        &["--version"],
        "install Rust from https://rustup.rs",
    )?;
    probe_tool(
        "rustc",
        &["--version"],
        "install Rust from https://rustup.rs",
    )?;
    probe_tool(
        "rustup",
        &["--version"],
        "install rustup from https://rustup.rs",
    )?;

    let active = command_output("rustc", &["--version"])?;
    if !active
        .split_whitespace()
        .any(|part| part == required_rust.as_str())
    {
        return Err(message(format!(
            "required Rust toolchain {required_rust} is not active; run: rustup toolchain install {required_rust}"
        )));
    }
    let targets = command_output(
        "rustup",
        &[
            "target",
            "list",
            "--installed",
            "--toolchain",
            &required_rust,
        ],
    )?;
    if !targets.lines().any(|target| target == "wasm32-wasip2") {
        return Err(message(format!(
            "required target wasm32-wasip2 is missing; run: rustup target add wasm32-wasip2 --toolchain {required_rust}"
        )));
    }
    probe_state_location()?;
    println!("toolchain: {required_rust}");
    println!("target: wasm32-wasip2");
    println!("state location: writable");
    if full {
        youth_desktop::window_smoke().map_err(CliError::Desktop)?;
        println!("native window: presented");
    }
    println!("doctor: ok");
    Ok(())
}

fn embedded_toolchain_channel() -> Result<String, CliError> {
    let value: toml::Value = toml::from_str(TOOLCHAIN)
        .map_err(|error| message(format!("embedded toolchain contract is invalid: {error}")))?;
    value
        .get("toolchain")
        .and_then(|toolchain| toolchain.get("channel"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| message("embedded toolchain contract has no channel"))
}

fn probe_tool(program: &str, arguments: &[&str], remediation: &str) -> Result<(), CliError> {
    command_output(program, arguments)
        .map(|output| println!("{program}: {}", output.lines().next().unwrap_or("ok")))
        .map_err(|_| message(format!("{program} is unavailable; {remediation}")))
}

fn command_output(program: &str, arguments: &[&str]) -> Result<String, CliError> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| message(format!("failed to run {program}: {error}")))?;
    if !output.status.success() {
        return Err(message(format!(
            "{program} {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn probe_state_location() -> Result<(), CliError> {
    let base = directories::BaseDirs::new()
        .ok_or_else(|| message("platform state directory is unavailable"))?;
    let directory = base.data_local_dir().join("youth");
    fs::create_dir_all(&directory).map_err(|error| {
        message(format!(
            "platform state directory is not writable ({}): {error}",
            directory.display()
        ))
    })?;
    let probe = directory.join(format!(".doctor-write-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|error| message(format!("platform state directory is not writable: {error}")))?;
    file.write_all(b"youth doctor")
        .map_err(|error| message(format!("platform state directory is not writable: {error}")))?;
    drop(file);
    fs::remove_file(&probe).map_err(|error| {
        message(format!(
            "could not clean up state write probe {}: {error}",
            probe.display()
        ))
    })
}

pub fn check_project() -> Result<(), CliError> {
    let current = std::env::current_dir()
        .map_err(|error| message(format!("could not read current directory: {error}")))?;
    let project = Project::discover(&current).map_err(|error| message(error.to_string()))?;
    project
        .verify_locked_inputs(env!("CARGO_PKG_VERSION"))
        .map_err(|error| message(error.to_string()))?;
    validate_cargo_metadata(&project)?;
    run_cargo(
        &project,
        &[
            "check",
            "--locked",
            "--package",
            &project.manifest.build.package,
            "--target",
            &project.manifest.build.target,
        ],
    )?;
    run_cargo(
        &project,
        &[
            "build",
            "--locked",
            "--package",
            &project.manifest.build.package,
            "--target",
            &project.manifest.build.target,
        ],
    )?;
    let component = project.cargo_component(false);
    let validation = youth_runtime::validate_component(&component)
        .map_err(|error| message(format!("component validation failed: {error}")))?;
    println!("project: {}", project.root().display());
    println!("app: {}", project.app_id);
    println!("protocol: {}", project.manifest.app.protocol);
    println!("component size: {} bytes", validation.size);
    println!("check: ok");
    Ok(())
}

fn validate_cargo_metadata(project: &Project) -> Result<(), CliError> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked", "--no-deps"])
        .current_dir(project.root())
        .stdin(Stdio::null())
        .output()
        .map_err(|error| message(format!("failed to run cargo metadata: {error}")))?;
    if !output.status.success() {
        return Err(message(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| message(format!("cargo metadata returned invalid JSON: {error}")))?;
    let packages = metadata["packages"]
        .as_array()
        .ok_or_else(|| message("cargo metadata has no packages"))?;
    let matches = packages
        .iter()
        .filter(|package| package["name"].as_str() == Some(&project.manifest.build.package))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(message(format!(
            "build.package {:?} resolved to {} Cargo packages, expected exactly one",
            project.manifest.build.package,
            matches.len()
        )));
    }
    let cdylib = matches[0]["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|target| {
            target["crate_types"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|kind| kind.as_str() == Some("cdylib"))
        });
    if !cdylib {
        return Err(message(format!(
            "Cargo package {:?} must define a cdylib target",
            project.manifest.build.package
        )));
    }
    Ok(())
}

fn run_cargo(project: &Project, arguments: &[&str]) -> Result<(), CliError> {
    let status = Command::new("cargo")
        .args(arguments)
        .current_dir(project.root())
        .status()
        .map_err(|error| message(format!("failed to run cargo: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(message(format!(
            "cargo {} failed with {status}",
            arguments.join(" ")
        )))
    }
}

fn io(path: &Path, error: std::io::Error) -> CliError {
    message(format!("could not write {}: {error}", path.display()))
}

fn message(value: impl Into<String>) -> CliError {
    CliError::Message(value.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_contract_has_one_source_of_toolchain_truth() {
        assert_eq!(embedded_toolchain_channel().unwrap(), "1.97.1");
    }

    #[test]
    fn package_and_display_names_are_deterministic() {
        assert_eq!(cargo_package_name("My Tally").unwrap(), "my-tally");
        assert_eq!(cargo_package_name("123").unwrap(), "app-123");
        assert_eq!(display_name("my-tally_app"), "My Tally App");
    }

    #[test]
    fn embedded_wit_matches_the_locked_hash() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("wit/youth");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("youth-app.wit"),
            include_str!("../templates/tally/wit/youth/youth-app.wit"),
        )
        .unwrap();
        fs::create_dir_all(root.join("deps/youth-state")).unwrap();
        fs::write(
            root.join("deps/youth-state/store.wit"),
            include_str!("../templates/tally/wit/youth/deps/youth-state/store.wit"),
        )
        .unwrap();
        assert_eq!(
            youth_project::hash_wit_tree(root).unwrap(),
            youth_project::TEMPLATE_WIT_SHA256
        );
    }

    #[test]
    fn generator_writes_a_locked_external_project_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("My Tally");
        let app_id = AppId::parse("dev.saman.generated").unwrap();
        new_project(&destination, &app_id).unwrap();

        let project = Project::load(&destination).unwrap();
        project.verify_locked_inputs(CLI_VERSION).unwrap();
        assert_eq!(project.manifest.build.package, "my-tally");
        assert_eq!(project.manifest.app.name, "My Tally");
        let cargo = fs::read_to_string(destination.join("Cargo.toml")).unwrap();
        assert!(cargo.contains(&format!("rev = \"{}\"", youth_project::SDK_REVISION)));
        assert!(!cargo.contains("path ="));
        assert!(destination.join("Cargo.lock").is_file());
        assert!(destination.join("tests/basic.youth-test").is_file());
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("youth-new")
        }));
    }

    #[test]
    fn generator_refuses_every_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("tally");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("owned.txt"), "keep").unwrap();
        let error = new_project(&destination, &AppId::parse("dev.saman.generated").unwrap())
            .expect_err("existing destination must fail");
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            fs::read_to_string(destination.join("owned.txt")).unwrap(),
            "keep"
        );
    }
}
