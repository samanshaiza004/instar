use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use youth_project::{CLI_VERSION, Project, SUPPORTED_PROTOCOL, TEMPLATE_VERSION};
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
    (
        "wit/youth/deps/youth-time/scheduler.wit",
        include_str!("../templates/tally/wit/youth/deps/youth-time/scheduler.wit"),
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
            .replace("youth-template-app", package)
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
    println!("template: {TEMPLATE_VERSION}");
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
    let (project, validation) = build_and_validate(false)?;
    println!("project: {}", project.root().display());
    println!("app: {}", project.app_id);
    println!("protocol: {}", project.manifest.app.protocol);
    println!("component size: {} bytes", validation.size);
    println!("check: ok");
    Ok(())
}

pub fn build_project(release: bool) -> Result<(), CliError> {
    let (project, validation) = build_and_validate(release)?;
    let source = project.cargo_component(release);
    let destination = project.dist_component();
    publish_component(&source, &destination)?;
    println!("component: {}", destination.display());
    println!("size: {} bytes", validation.size);
    println!("sha256: {}", validation.sha256);
    Ok(())
}

pub async fn test_project(verify_view_convergence: bool) -> Result<(), CliError> {
    let (project, _) = build_and_validate(false)?;
    let count = youth_test::run_directory_with_options(
        &project.root().join("tests"),
        &project.cargo_component(false),
        &project.app_id,
        youth_test::RunOptions {
            verify_view_convergence,
        },
    )
    .await
    .map_err(|error| message(error.to_string()))?;
    println!("tests: {count} passed");
    Ok(())
}

pub async fn dev_project(headless: bool) -> Result<(), CliError> {
    let (mut project, _) = build_and_validate(false)?;
    let mut child = Some(spawn_dev_child(&project, headless)?);
    let mut observed = watch_fingerprint(project.root())?;
    let mut changed_at = None;
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    println!("dev: watching {}", project.root().display());

    loop {
        tokio::select! {
            result = &mut shutdown => {
                result.map_err(|error| message(format!("failed to listen for Ctrl-C: {error}")))?;
                if let Some(mut running) = child.take() {
                    stop_child(&mut running).await?;
                }
                println!("dev: stopped");
                return Ok(());
            }
            _ = interval.tick() => {
                if child
                    .as_mut()
                    .is_some_and(|running| running.try_wait().ok().flatten().is_some())
                {
                    child = None;
                    println!("dev: application window closed; still watching");
                }
                let current = watch_fingerprint(project.root())?;
                if current != observed {
                    observed = current;
                    changed_at = Some(Instant::now());
                    continue;
                }
                if !changed_at.is_some_and(|changed| changed.elapsed() >= Duration::from_millis(300)) {
                    continue;
                }

                println!("dev: rebuilding");
                match build_and_validate(false) {
                    Ok((rebuilt, _)) => {
                        let after_build = watch_fingerprint(rebuilt.root())?;
                        if after_build != observed {
                            observed = after_build;
                            changed_at = Some(Instant::now());
                            println!("dev: more changes queued");
                            project = rebuilt;
                            continue;
                        }
                        if let Some(mut running) = child.take() {
                            stop_child(&mut running).await?;
                        }
                        child = Some(spawn_dev_child(&rebuilt, headless)?);
                        project = rebuilt;
                        changed_at = None;
                        println!("dev: restarted");
                    }
                    Err(error) => {
                        eprintln!("dev: rebuild failed; retaining the last valid application\n{error}");
                        changed_at = None;
                    }
                }
            }
        }
    }
}

fn spawn_dev_child(project: &Project, headless: bool) -> Result<Child, CliError> {
    let executable = std::env::current_exe().map_err(|error| {
        message(format!(
            "could not locate the current Youth executable: {error}"
        ))
    })?;
    let mut command = Command::new(executable);
    command.arg(if headless { "__dev-child" } else { "run" });
    command
        .arg(project.cargo_component(false))
        .arg("--app-id")
        .arg(project.app_id.as_str())
        .arg("--state-dir")
        .arg(project.state_root())
        .current_dir(project.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if !headless {
        command.arg("--supervised");
    }
    command
        .spawn()
        .map_err(|error| message(format!("could not start Youth application: {error}")))
}

pub async fn headless_dev_child(
    component: PathBuf,
    app_id: AppId,
    state_root: PathBuf,
) -> Result<(), CliError> {
    let app = youth_runtime::YouthAppHandle::spawn(youth_runtime::YouthAppConfig {
        component_path: component,
        state: youth_state::StateLocation::File(
            state_root.join(app_id.as_str()).join("state.sqlite3"),
        ),
        app_id,
        limits: youth_runtime::RuntimeLimits::default(),
    })
    .map_err(CliError::Runtime)?;
    app.mount().await.map_err(CliError::Runtime)?;
    println!("dev child: mounted");
    tokio::task::spawn_blocking(|| {
        use std::io::Read;

        let mut byte = [0_u8; 1];
        std::io::stdin().read(&mut byte)
    })
    .await
    .map_err(|error| message(format!("shutdown reader failed: {error}")))?
    .map_err(|error| message(format!("shutdown input failed: {error}")))?;
    app.stop().await.map_err(CliError::Runtime)
}

async fn stop_child(child: &mut Child) -> Result<(), CliError> {
    if child
        .try_wait()
        .map_err(|error| message(error.to_string()))?
        .is_some()
    {
        return Ok(());
    }
    if let Some(mut input) = child.stdin.take() {
        input
            .write_all(b"shutdown\n")
            .and_then(|()| input.flush())
            .map_err(|error| message(format!("could not signal application shutdown: {error}")))?;
    }
    for _ in 0..40 {
        if child
            .try_wait()
            .map_err(|error| message(error.to_string()))?
            .is_some()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    child
        .kill()
        .map_err(|error| message(format!("could not stop application after timeout: {error}")))?;
    child
        .wait()
        .map_err(|error| message(format!("could not reap stopped application: {error}")))?;
    Ok(())
}

fn watch_fingerprint(root: &Path) -> Result<[u8; 32], CliError> {
    let mut paths = Vec::new();
    for file in [
        "Cargo.toml",
        "Cargo.lock",
        "Youth.toml",
        "Youth.lock",
        "rust-toolchain.toml",
    ] {
        let path = root.join(file);
        if path.is_file() {
            paths.push(path);
        }
    }
    for directory in ["src", ".cargo", "wit"] {
        collect_regular_files(&root.join(directory), &mut paths)?;
    }
    paths.sort();
    let mut digest = Sha256::new();
    for path in paths {
        let relative = path
            .strip_prefix(root)
            .map_err(|error| message(format!("invalid watched path: {error}")))?
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let bytes = fs::read(&path).map_err(|error| io(&path, error))?;
        digest.update((relative.len() as u64).to_be_bytes());
        digest.update(relative.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    Ok(digest.finalize().into())
}

fn collect_regular_files(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<(), CliError> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory).map_err(|error| io(directory, error))? {
        let entry = entry.map_err(|error| io(directory, error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| io(&entry.path(), error))?;
        if file_type.is_symlink() {
            return Err(message(format!(
                "watched project inputs must not be symlinks: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            collect_regular_files(&entry.path(), paths)?;
        } else if file_type.is_file() {
            paths.push(entry.path());
        }
    }
    Ok(())
}

fn build_and_validate(
    release: bool,
) -> Result<(Project, youth_runtime::ComponentValidation), CliError> {
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
    let mut build_arguments = vec![
        "build",
        "--locked",
        "--package",
        &project.manifest.build.package,
        "--target",
        &project.manifest.build.target,
    ];
    if release {
        build_arguments.push("--release");
    }
    run_cargo(&project, &build_arguments)?;
    let component = project.cargo_component(release);
    let validation = youth_runtime::validate_component(&component)
        .map_err(|error| message(format!("component validation failed: {error}")))?;
    Ok((project, validation))
}

fn publish_component(source: &Path, destination: &Path) -> Result<(), CliError> {
    let directory = destination
        .parent()
        .ok_or_else(|| message("distribution component has no parent directory"))?;
    fs::create_dir_all(directory).map_err(|error| io(directory, error))?;
    let temporary = directory.join(format!(".component-{}.tmp", std::process::id()));
    if temporary.exists() {
        return Err(message(format!(
            "temporary distribution path already exists: {}",
            temporary.display()
        )));
    }
    let result = (|| {
        fs::copy(source, &temporary).map_err(|error| io(&temporary, error))?;
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&temporary)
            .map_err(|error| io(&temporary, error))?;
        file.sync_all().map_err(|error| io(&temporary, error))?;
        replace_file(&temporary, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn replace_file(temporary: &Path, destination: &Path) -> Result<(), CliError> {
    match fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(error)
            if destination.exists()
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) =>
        {
            let backup = destination.with_extension(format!("wasm.old-{}", std::process::id()));
            fs::rename(destination, &backup).map_err(|error| io(destination, error))?;
            match fs::rename(temporary, destination) {
                Ok(()) => {
                    fs::remove_file(&backup).map_err(|error| io(&backup, error))?;
                    Ok(())
                }
                Err(error) => {
                    let _ = fs::rename(&backup, destination);
                    Err(io(destination, error))
                }
            }
        }
        Err(error) => Err(io(destination, error)),
    }
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
        fs::create_dir_all(root.join("deps/youth-time")).unwrap();
        fs::write(
            root.join("deps/youth-time/scheduler.wit"),
            include_str!("../templates/tally/wit/youth/deps/youth-time/scheduler.wit"),
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

    #[test]
    fn watcher_tracks_inputs_and_ignores_build_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("Cargo.toml"), "initial").unwrap();
        fs::write(root.join("src/lib.rs"), "initial").unwrap();
        fs::write(root.join("target/output"), "one").unwrap();
        let initial = watch_fingerprint(root).unwrap();

        fs::write(root.join("target/output"), "two").unwrap();
        fs::write(root.join("dist/output"), "two").unwrap();
        assert_eq!(watch_fingerprint(root).unwrap(), initial);

        fs::write(root.join("src/lib.rs"), "changed").unwrap();
        assert_ne!(watch_fingerprint(root).unwrap(), initial);
    }
}
