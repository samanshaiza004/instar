//! `.youth-test` parser and real headless-runtime runner.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;
use youth_runtime::{RuntimeLimits, YouthAppConfig, YouthAppHandle};
use youth_state::{AppId, GuestCallPhase, StateLimits, StateLocation, StateStore, StateValue};
use youth_tree::{NodeData, NodeId, Tree, TreeSnapshot};

use youth_interaction::{InteractionState, LogicalKey, Modifiers, SemanticAction};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    State {
        key: String,
        value: StateValue,
    },
    Mount,
    Activate {
        name: String,
    },
    Restart,
    Key {
        key: LogicalKey,
        modifiers: Modifiers,
    },
    ExpectText {
        name: String,
        expected: String,
    },
    ExpectCountdown {
        name: String,
    },
    ExpectFocus {
        name: Option<String>,
    },
    ExpectState {
        key: String,
        expected: Option<StateValue>,
    },
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
            Command::State { .. } => {
                if mounted {
                    return Err(diagnostic(
                        path,
                        line,
                        source_line,
                        "state may only be seeded before the initial mount",
                    ));
                }
            }
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
            Command::Activate { .. }
            | Command::Key { .. }
            | Command::ExpectText { .. }
            | Command::ExpectCountdown { .. }
            | Command::ExpectFocus { .. }
            | Command::ExpectState { .. }
            | Command::Restart => {
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
    if let Some(value) = source.strip_prefix("key ") {
        let (key, modifiers) = parse_key(path, line, source, value)?;
        return Ok(Command::Key { key, modifiers });
    }
    if let Some(arguments) = source.strip_prefix("state ") {
        let (kind, arguments) = arguments.split_once(' ').ok_or_else(|| {
            diagnostic(
                path,
                line,
                source,
                "expected: state <boolean|integer|text|bytes> <JSON-string-key> <value>",
            )
        })?;
        let (key, encoded) = parse_json_string_prefix(path, line, source, arguments)?;
        let value = parse_state_value(path, line, source, kind, encoded)?;
        return Ok(Command::State { key, value });
    }
    if let Some(arguments) = source.strip_prefix("expect state ") {
        let (kind, arguments) = arguments.split_once(' ').ok_or_else(|| {
            diagnostic(
                path,
                line,
                source,
                "expected: expect state <boolean|integer|text|missing> <JSON-string-key> [value]",
            )
        })?;
        if !matches!(kind, "boolean" | "integer" | "text" | "missing") {
            return Err(diagnostic(
                path,
                line,
                source,
                "state assertion kind must be boolean, integer, text, or missing",
            ));
        }
        let (key, encoded) = parse_json_string_prefix(path, line, source, arguments)?;
        let expected = if kind == "missing" {
            if !encoded.is_empty() {
                return Err(diagnostic(
                    path,
                    line,
                    source,
                    "expect state missing does not accept a value",
                ));
            }
            None
        } else {
            Some(parse_state_value(path, line, source, kind, encoded)?)
        };
        return Ok(Command::ExpectState { key, expected });
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
    if let Some(name) = source.strip_prefix("expect countdown ") {
        validate_name(path, line, source, name)?;
        return Ok(Command::ExpectCountdown {
            name: name.to_owned(),
        });
    }
    if let Some(name) = source.strip_prefix("expect focus ") {
        if name == "none" {
            return Ok(Command::ExpectFocus { name: None });
        }
        validate_name(path, line, source, name)?;
        return Ok(Command::ExpectFocus {
            name: Some(name.to_owned()),
        });
    }
    Err(diagnostic(
        path,
        line,
        source,
        "unknown command; expected state, mount, activate, key, restart, expect text, expect countdown, expect focus, or expect state",
    ))
}

fn parse_json_string_prefix<'a>(
    path: &Path,
    line: usize,
    source: &str,
    input: &'a str,
) -> Result<(String, &'a str), TestError> {
    let mut values = serde_json::Deserializer::from_str(input).into_iter::<String>();
    let value = values
        .next()
        .transpose()
        .map_err(|error| diagnostic(path, line, source, &format!("invalid JSON string: {error}")))?
        .ok_or_else(|| diagnostic(path, line, source, "expected a JSON string"))?;
    let raw_remainder = &input[values.byte_offset()..];
    if !raw_remainder.is_empty()
        && !raw_remainder
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        return Err(diagnostic(
            path,
            line,
            source,
            "expected whitespace after JSON string",
        ));
    }
    let remainder = raw_remainder.trim_start();
    Ok((value, remainder))
}

fn parse_state_value(
    path: &Path,
    line: usize,
    source: &str,
    kind: &str,
    encoded: &str,
) -> Result<StateValue, TestError> {
    match kind {
        "boolean" => match encoded {
            "true" => Ok(StateValue::Boolean(true)),
            "false" => Ok(StateValue::Boolean(false)),
            _ => Err(diagnostic(
                path,
                line,
                source,
                "boolean state value must be true or false",
            )),
        },
        "integer" => encoded
            .parse::<i64>()
            .map(StateValue::Integer)
            .map_err(|error| {
                diagnostic(
                    path,
                    line,
                    source,
                    &format!("invalid 64-bit integer: {error}"),
                )
            }),
        "text" | "bytes" => {
            let value: String = serde_json::from_str(encoded).map_err(|error| {
                diagnostic(path, line, source, &format!("invalid JSON string: {error}"))
            })?;
            if kind == "text" {
                Ok(StateValue::Text(value))
            } else {
                Ok(StateValue::Bytes(value.into_bytes()))
            }
        }
        _ => Err(diagnostic(
            path,
            line,
            source,
            "state kind must be boolean, integer, text, or bytes",
        )),
    }
}

fn parse_key(
    path: &Path,
    line: usize,
    source: &str,
    value: &str,
) -> Result<(LogicalKey, Modifiers), TestError> {
    let named = match value {
        "enter" => Some((LogicalKey::Enter, Modifiers::default())),
        "escape" => Some((LogicalKey::Escape, Modifiers::default())),
        "backspace" => Some((LogicalKey::Backspace, Modifiers::default())),
        "space" => Some((LogicalKey::Space, Modifiers::default())),
        "tab" => Some((LogicalKey::Tab, Modifiers::default())),
        "shift-tab" => Some((
            LogicalKey::Tab,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        )),
        "left" => Some((LogicalKey::ArrowLeft, Modifiers::default())),
        "right" => Some((LogicalKey::ArrowRight, Modifiers::default())),
        "up" => Some((LogicalKey::ArrowUp, Modifiers::default())),
        "down" => Some((LogicalKey::ArrowDown, Modifiers::default())),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(named);
    }
    let character: String = serde_json::from_str(value).map_err(|error| {
        diagnostic(
            path,
            line,
            source,
            &format!("key must be a named key or JSON string: {error}"),
        )
    })?;
    let mut characters = character.chars();
    let character = characters
        .next()
        .filter(|_| characters.next().is_none())
        .ok_or_else(|| {
            diagnostic(
                path,
                line,
                source,
                "character key must contain exactly one Unicode scalar",
            )
        })?;
    Ok((LogicalKey::Character(character), Modifiers::default()))
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
    seed_state(path, &script.commands, app_id, &state_file)?;
    let mut app = spawn(component, app_id, &state_file)?;
    let mut snapshot = None;
    let mut interaction = InteractionState::default();

    for located in script.commands {
        match &located.command {
            Command::State { .. } => {}
            Command::Mount => {
                snapshot = Some(
                    app.mount()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
                reconcile(&mut interaction, snapshot.as_ref().unwrap());
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
                reconcile(&mut interaction, snapshot.as_ref().unwrap());
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
                interaction = InteractionState::default();
                reconcile(&mut interaction, snapshot.as_ref().unwrap());
            }
            Command::Key { key, modifiers } => {
                let tree = normalized_tree(snapshot.as_ref().expect("parser requires mount"));
                let change = interaction.key(&tree, key.clone(), *modifiers, false);
                if let Some(SemanticAction::Activate(node)) = change.action {
                    app.activate(node)
                        .await
                        .map_err(|error| runtime(path, &located, error))?;
                    snapshot = Some(
                        app.snapshot()
                            .await
                            .map_err(|error| runtime(path, &located, error))?,
                    );
                    reconcile(&mut interaction, snapshot.as_ref().unwrap());
                }
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
            Command::ExpectCountdown { name } => {
                expect_countdown(
                    path,
                    &located,
                    snapshot.as_ref().expect("parser requires mount"),
                    name,
                )?;
            }
            Command::ExpectFocus { name } => {
                let expected = name.as_deref().map(named_id);
                if interaction.focused() != expected {
                    return Err(TestError::Diagnostic {
                        path: path.to_path_buf(),
                        line: located.line,
                        command: located.source.clone(),
                        message: format!(
                            "expected focus {name:?}; observed {:?}",
                            interaction.focused().map(NodeId::get)
                        ),
                    });
                }
            }
            Command::ExpectState { key, expected } => {
                expect_state(path, &located, app_id, &state_file, key, expected)?;
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

fn seed_state(
    path: &Path,
    commands: &[LocatedCommand],
    app_id: &AppId,
    state_file: &Path,
) -> Result<(), TestError> {
    let seeds = commands
        .iter()
        .take_while(|located| matches!(located.command, Command::State { .. }))
        .collect::<Vec<_>>();
    let Some(first) = seeds.first() else {
        return Ok(());
    };
    let mut store = StateStore::open_for_app(
        StateLocation::File(state_file.to_path_buf()),
        StateLimits::default(),
        app_id.clone(),
    )
    .map_err(|error| state_error(path, first, "could not open state for seeding", error))?;
    store.begin(GuestCallPhase::Mount).map_err(|error| {
        state_error(path, first, "could not begin state seed transaction", error)
    })?;
    for located in &seeds {
        let Command::State { key, value } = &located.command else {
            unreachable!("seed prefix contains only state commands");
        };
        store.set(key, value.clone()).map_err(|error| {
            state_error(
                path,
                located,
                &format!("could not seed state key {key:?}"),
                error,
            )
        })?;
    }
    let last = seeds.last().expect("nonempty seed prefix");
    store
        .commit()
        .map_err(|error| state_error(path, last, "could not commit seeded state", error))?;
    drop(store);
    Ok(())
}

fn expect_state(
    path: &Path,
    located: &LocatedCommand,
    app_id: &AppId,
    state_file: &Path,
    key: &str,
    expected: &Option<StateValue>,
) -> Result<(), TestError> {
    let mut store = StateStore::open_for_app(
        StateLocation::File(state_file.to_path_buf()),
        StateLimits::default(),
        app_id.clone(),
    )
    .map_err(|error| state_error(path, located, "could not open state for assertion", error))?;
    store
        .begin(GuestCallPhase::Resync)
        .map_err(|error| state_error(path, located, "could not begin state read", error))?;
    let observed = store
        .get(key)
        .map_err(|error| state_error(path, located, "could not read asserted state", error))?;
    store
        .rollback()
        .map_err(|error| state_error(path, located, "could not finish state read", error))?;
    if observed == *expected {
        return Ok(());
    }
    let expected_description = match expected {
        Some(value) => format!("{value:?}"),
        None => "missing".into(),
    };
    Err(TestError::Diagnostic {
        path: path.to_path_buf(),
        line: located.line,
        command: located.source.clone(),
        message: format!(
            "expected state key {key:?} to be {expected_description}; observed {observed:?}"
        ),
    })
}

fn state_error(
    path: &Path,
    command: &LocatedCommand,
    context: &str,
    error: youth_state::StateError,
) -> TestError {
    TestError::Diagnostic {
        path: path.to_path_buf(),
        line: command.line,
        command: command.source.clone(),
        message: format!("{context}: {error}"),
    }
}

fn normalized_tree(snapshot: &TreeSnapshot) -> Tree {
    Tree::from_snapshot(snapshot.clone(), &youth_tree::Limits::default())
        .expect("runtime snapshots are already validated")
}

fn reconcile(interaction: &mut InteractionState, snapshot: &TreeSnapshot) {
    interaction.reconcile(&normalized_tree(snapshot));
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
        Some(data) if data.text_value().is_some_and(|value| value == expected) => Ok(()),
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

fn expect_countdown(
    path: &Path,
    located: &LocatedCommand,
    snapshot: &TreeSnapshot,
    name: &str,
) -> Result<(), TestError> {
    let id = named_id(name);
    let node = snapshot.nodes.iter().find(|node| node.id == id);
    match node.map(|node| &node.data) {
        Some(data) if data.countdown_ref().is_some() => Ok(()),
        observed => Err(TestError::Diagnostic {
            path: path.to_path_buf(),
            line: located.line,
            command: located.source.clone(),
            message: format!(
                "expected countdown {name:?}; observed {}",
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
        Some(NodeData::Countdown { .. }) | Some(NodeData::AlignedCountdown { .. }) => {
            "a countdown node".into()
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
    fn parses_seed_and_state_assertion_values() {
        let script = parse(
            Path::new("state.youth-test"),
            r#"
state integer "count" -7
state text "message key" "hello\nworld"
state boolean "enabled" false
state bytes "payload" "héllo"
mount
expect state integer "count" -7
expect state text "message key" "hello\nworld"
expect state boolean "enabled" false
expect state missing "removed"
"#,
        )
        .unwrap();
        assert_eq!(
            script.commands[0].command,
            Command::State {
                key: "count".into(),
                value: StateValue::Integer(-7),
            }
        );
        assert_eq!(
            script.commands[3].command,
            Command::State {
                key: "payload".into(),
                value: StateValue::Bytes("héllo".as_bytes().to_vec()),
            }
        );
        assert_eq!(
            script.commands[8].command,
            Command::ExpectState {
                key: "removed".into(),
                expected: None,
            }
        );
    }

    #[test]
    fn parses_logical_keys_and_focus_assertions() {
        let script = parse(
            Path::new("keyboard.youth-test"),
            "mount\nkey tab\nexpect focus increment\nkey \"+\"\nkey enter\nexpect focus none\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::Key {
                key: LogicalKey::Tab,
                modifiers: Modifiers::default(),
            }
        );
        assert_eq!(
            script.commands[3].command,
            Command::Key {
                key: LogicalKey::Character('+'),
                modifiers: Modifiers::default(),
            }
        );
        assert!(parse(Path::new("bad.youth-test"), "mount\nkey \"ab\"\n").is_err());
    }

    #[test]
    fn parses_countdown_assertion() {
        let script = parse(
            Path::new("countdown.youth-test"),
            "mount\nexpect countdown remaining\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::ExpectCountdown {
                name: "remaining".into(),
            }
        );
    }

    #[test]
    fn rejects_countdown_assertion_before_initial_mount() {
        let error = parse(
            Path::new("bad.youth-test"),
            "expect countdown remaining\nmount\n",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("command appears before the required initial mount"),
            "{error}"
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
        parse(
            Path::new("seeded.youth-test"),
            "state integer \"count\" 3\nstate boolean \"ready\" true\nmount\n",
        )
        .unwrap();
    }

    #[test]
    fn rejects_state_after_mount_or_restart_with_seed_guidance() {
        for source in [
            "mount\nstate integer \"count\" 1\n",
            "mount\nrestart\nstate integer \"count\" 1\n",
        ] {
            let error = parse(Path::new("bad.youth-test"), source).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("state may only be seeded before the initial mount"),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_unrequested_bytes_state_assertions() {
        let error = parse(
            Path::new("bad.youth-test"),
            "mount\nexpect state bytes \"payload\" \"value\"\n",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("state assertion kind must be boolean, integer, text, or missing"),
            "{error}"
        );
    }

    #[test]
    fn multiple_seed_types_round_trip_through_the_typed_store() {
        let path = Path::new("seeded.youth-test");
        let script = parse(
            path,
            r#"
state integer "integer" 42
state text "text" "hello"
state boolean "boolean" true
state bytes "bytes" "héllo"
mount
"#,
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let state_file = directory.path().join("state.sqlite3");
        let app_id = AppId::parse("dev.youth.seed-test").unwrap();
        seed_state(path, &script.commands, &app_id, &state_file).unwrap();

        let mut store = StateStore::open_for_app(
            StateLocation::File(state_file),
            StateLimits::default(),
            app_id,
        )
        .unwrap();
        store.begin(GuestCallPhase::Resync).unwrap();
        assert_eq!(store.get("integer").unwrap(), Some(StateValue::Integer(42)));
        assert_eq!(
            store.get("text").unwrap(),
            Some(StateValue::Text("hello".into()))
        );
        assert_eq!(
            store.get("boolean").unwrap(),
            Some(StateValue::Boolean(true))
        );
        assert_eq!(
            store.get("bytes").unwrap(),
            Some(StateValue::Bytes("héllo".as_bytes().to_vec()))
        );
        store.rollback().unwrap();
    }

    #[test]
    fn oversized_seed_fails_clearly_at_seed_time() {
        let path = Path::new("oversized.youth-test");
        let command = LocatedCommand {
            line: 1,
            source: "state text \"too-big\" <oversized JSON string>".into(),
            command: Command::State {
                key: "too-big".into(),
                value: StateValue::Text("x".repeat(StateLimits::default().max_text_bytes + 1)),
            },
        };
        let directory = tempfile::tempdir().unwrap();
        let error = seed_state(
            path,
            &[command],
            &AppId::parse("dev.youth.seed-test").unwrap(),
            &directory.path().join("state.sqlite3"),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("could not seed state key \"too-big\""));
        assert!(message.contains("state value is invalid"));
    }

    #[test]
    fn locks_symbolic_id_vectors_in_the_test_runner() {
        assert_eq!(named_id("count").get(), 0xf700_b2fe_97f6_53d6);
        assert_eq!(named_id("increment").get(), 0xd9e1_c44e_444d_fb74);
        assert_eq!(named_id("café").get(), 0xcab8_7ecf_2aee_1d93);
    }
}
