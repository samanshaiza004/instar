//! `.youth-test` parser and real headless-runtime runner.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;
use youth_runtime::{RuntimeLimits, YouthAppConfig, YouthAppHandle};
use youth_state::{AppId, StateLocation};
use youth_tree::{NodeData, NodeId, TreeSnapshot};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Mount,
    Activate { name: String },
    Restart,
    ExpectText { name: String, expected: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocatedCommand {
    pub line: usize,
    pub source: String,
    pub command: Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Script {
    pub commands: Vec<LocatedCommand>,
}

pub fn parse(path: &Path, source: &str) -> Result<Script, TestError> {
    let mut commands = Vec::new();
    let mut mounted = false;
    let mut mount_seen = false;
    for (offset, raw) in source.lines().enumerate() {
        let line = offset + 1;
        let source_line = strip_comment(raw).trim();
        if source_line.is_empty() {
            continue;
        }
        let command = parse_command(path, line, source_line)?;
        match command {
            Command::Mount => {
                if mount_seen {
                    return Err(diagnostic(
                        path,
                        line,
                        source_line,
                        "every test must contain exactly one explicit initial mount",
                    ));
                }
                mount_seen = true;
                mounted = true;
            }
            Command::Activate { .. } | Command::ExpectText { .. } | Command::Restart => {
                if !mounted {
                    return Err(diagnostic(
                        path,
                        line,
                        source_line,
                        "command appears before the required initial mount",
                    ));
                }
            }
        }
        commands.push(LocatedCommand {
            line,
            source: source_line.to_owned(),
            command,
        });
    }
    if !mount_seen {
        return Err(diagnostic(
            path,
            1,
            "<end of file>",
            "every test must contain exactly one explicit initial mount",
        ));
    }
    Ok(Script { commands })
}

fn parse_command(path: &Path, line: usize, source: &str) -> Result<Command, TestError> {
    if source == "mount" {
        return Ok(Command::Mount);
    }
    if source == "restart" {
        return Ok(Command::Restart);
    }
    if let Some(name) = source.strip_prefix("activate ") {
        validate_name(path, line, source, name)?;
        return Ok(Command::Activate {
            name: name.to_owned(),
        });
    }
    if let Some(arguments) = source.strip_prefix("expect text ") {
        let (name, encoded) = arguments.split_once(' ').ok_or_else(|| {
            diagnostic(
                path,
                line,
                source,
                "expected: expect text <node-name> <JSON-string>",
            )
        })?;
        validate_name(path, line, source, name)?;
        let expected: String = serde_json::from_str(encoded).map_err(|error| {
            diagnostic(path, line, source, &format!("invalid JSON string: {error}"))
        })?;
        return Ok(Command::ExpectText {
            name: name.to_owned(),
            expected,
        });
    }
    Err(diagnostic(
        path,
        line,
        source,
        "unknown command; expected mount, activate, restart, or expect text",
    ))
}

fn validate_name(path: &Path, line: usize, source: &str, name: &str) -> Result<(), TestError> {
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        Err(diagnostic(
            path,
            line,
            source,
            "node name must be one nonempty token",
        ))
    } else {
        Ok(())
    }
}

fn strip_comment(source: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in source.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '#' if !quoted => return &source[..index],
            _ => {}
        }
    }
    source
}

pub async fn run_directory(
    tests_directory: &Path,
    component: &Path,
    app_id: &AppId,
) -> Result<usize, TestError> {
    let mut files = fs::read_dir(tests_directory)
        .map_err(|source| TestError::Io {
            path: tests_directory.to_path_buf(),
            source,
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("youth-test"))
        .collect::<Vec<_>>();
    files.sort();
    if files.is_empty() {
        return Err(TestError::Diagnostic {
            path: tests_directory.to_path_buf(),
            line: 1,
            command: "<directory>".into(),
            message: "no tests/*.youth-test files found".into(),
        });
    }
    for path in &files {
        let metadata = fs::symlink_metadata(path).map_err(|source| TestError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(TestError::Diagnostic {
                path: path.clone(),
                line: 1,
                command: "<file>".into(),
                message: "test entries must be regular files, not symlinks".into(),
            });
        }
        run_file(path, component, app_id).await?;
    }
    Ok(files.len())
}

pub async fn run_file(path: &Path, component: &Path, app_id: &AppId) -> Result<(), TestError> {
    let source = fs::read_to_string(path).map_err(|source| TestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let script = parse(path, &source)?;
    let state = tempfile::tempdir().map_err(|source| TestError::Io {
        path: std::env::temp_dir(),
        source,
    })?;
    let state_file = state.path().join("state.sqlite3");
    let mut app = spawn(component, app_id, &state_file)?;
    let mut snapshot = None;

    for located in script.commands {
        match &located.command {
            Command::Mount => {
                snapshot = Some(
                    app.mount()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
            }
            Command::Activate { name } => {
                let node = named_id(name);
                app.activate(node)
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                snapshot = Some(
                    app.snapshot()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
            }
            Command::Restart => {
                app.stop()
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                app = spawn(component, app_id, &state_file)?;
                snapshot = Some(
                    app.mount()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
            }
            Command::ExpectText { name, expected } => {
                expect_text(
                    path,
                    &located,
                    snapshot.as_ref().expect("parser requires mount"),
                    name,
                    expected,
                )?;
            }
        }
    }
    app.stop().await.map_err(|error| TestError::Diagnostic {
        path: path.to_path_buf(),
        line: 1,
        command: "<shutdown>".into(),
        message: error.to_string(),
    })?;
    Ok(())
}

fn spawn(component: &Path, app_id: &AppId, state_file: &Path) -> Result<YouthAppHandle, TestError> {
    YouthAppHandle::spawn(YouthAppConfig {
        component_path: component.to_path_buf(),
        app_id: app_id.clone(),
        state: StateLocation::File(state_file.to_path_buf()),
        limits: RuntimeLimits::default(),
    })
    .map_err(|error| TestError::Diagnostic {
        path: component.to_path_buf(),
        line: 1,
        command: "<spawn>".into(),
        message: error.to_string(),
    })
}

fn named_id(name: &str) -> NodeId {
    NodeId::new(youth_sdk::named_node_id(name)).expect("named IDs are nonzero")
}

fn expect_text(
    path: &Path,
    located: &LocatedCommand,
    snapshot: &TreeSnapshot,
    name: &str,
    expected: &str,
) -> Result<(), TestError> {
    let id = named_id(name);
    let node = snapshot.nodes.iter().find(|node| node.id == id);
    match node.map(|node| &node.data) {
        Some(NodeData::Text { value }) if value == expected => Ok(()),
        observed => Err(TestError::Diagnostic {
            path: path.to_path_buf(),
            line: located.line,
            command: located.source.clone(),
            message: format!(
                "expected text {name:?} to equal {expected:?}; observed {}",
                describe(observed)
            ),
        }),
    }
}

fn describe(observed: Option<&NodeData>) -> String {
    match observed {
        None => "no semantic node".into(),
        Some(NodeData::Root) => "root node".into(),
        Some(NodeData::Box { enabled }) => format!("column(enabled={enabled})"),
        Some(NodeData::Row { enabled }) => format!("row(enabled={enabled})"),
        Some(NodeData::Grid { columns, enabled }) => {
            format!("grid(columns={columns}, enabled={enabled})")
        }
        Some(NodeData::Text { value }) | Some(NodeData::AlignedText { value, .. }) => {
            format!("text({value:?})")
        }
        Some(NodeData::Button { label, enabled })
        | Some(NodeData::ShortcutButton { label, enabled, .. }) => {
            format!("button(label={label:?}, enabled={enabled})")
        }
    }
}

fn runtime(path: &Path, command: &LocatedCommand, error: youth_runtime::RuntimeError) -> TestError {
    TestError::Diagnostic {
        path: path.to_path_buf(),
        line: command.line,
        command: command.source.clone(),
        message: error.to_string(),
    }
}

fn diagnostic(path: &Path, line: usize, command: &str, message: &str) -> TestError {
    TestError::Diagnostic {
        path: path.to_path_buf(),
        line,
        command: command.to_owned(),
        message: message.to_owned(),
    }
}

#[derive(Debug, Error)]
pub enum TestError {
    #[error("{path}:{line}: {message}\n  command: {command}")]
    Diagnostic {
        path: PathBuf,
        line: usize,
        command: String,
        message: String,
    },
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_json_strings_and_restart() {
        let script = parse(
            Path::new("basic.youth-test"),
            "# start\nmount\nexpect text count \"Count: #0\\n\" # note\nactivate increment\nrestart\n",
        )
        .unwrap();
        assert_eq!(script.commands.len(), 4);
        assert_eq!(
            script.commands[1].command,
            Command::ExpectText {
                name: "count".into(),
                expected: "Count: #0\n".into()
            }
        );
    }

    #[test]
    fn enforces_exact_initial_mount_rules() {
        for source in [
            "activate increment\nmount\n",
            "mount\nmount\n",
            "mount\nrestart\nmount\n",
            "expect text count \"x\"\n",
            "# empty\n",
        ] {
            assert!(parse(Path::new("bad.youth-test"), source).is_err());
        }
        parse(Path::new("ok.youth-test"), "mount\nrestart\nrestart\n").unwrap();
    }

    #[test]
    fn locks_symbolic_id_vectors_in_the_test_runner() {
        assert_eq!(named_id("count").get(), 0xf700_b2fe_97f6_53d6);
        assert_eq!(named_id("increment").get(), 0xd9e1_c44e_444d_fb74);
        assert_eq!(named_id("café").get(), 0xcab8_7ecf_2aee_1d93);
    }
}
