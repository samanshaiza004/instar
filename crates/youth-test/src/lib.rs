//! `.youth-test` parser and real headless-runtime runner.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;
use youth_runtime::{RuntimeLimits, YouthAppConfig, YouthAppHandle};
use youth_state::{AppId, GuestCallPhase, StateLimits, StateLocation, StateStore, StateValue};
use youth_tree::{NodeData, NodeId, Tree, TreeSnapshot};

use youth_interaction::{InteractionState, LogicalKey, Modifiers, SemanticAction};
use youth_runtime::RuntimeEvent;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunOptions {
    pub verify_view_convergence: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Selector {
    Static(String),
    Derived {
        namespace: String,
        item: u64,
        role: String,
    },
}

impl Selector {
    fn node_id(&self) -> NodeId {
        let value = match self {
            Self::Static(name) => youth_sdk::named_node_id(name),
            Self::Derived {
                namespace,
                item,
                role,
            } => youth_sdk::derived_node_id(namespace, *item, role)
                .expect("parsed derived selectors are valid"),
        };
        NodeId::new(value).expect("symbolic IDs are nonzero")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    State {
        key: String,
        value: StateValue,
    },
    Mount,
    Activate {
        selector: Selector,
    },
    Sleep {
        millis: u64,
    },
    Restart,
    Key {
        key: LogicalKey,
        modifiers: Modifiers,
    },
    ExpectText {
        selector: Selector,
        expected: String,
    },
    ExpectCountdown {
        selector: Selector,
    },
    ExpectFocus {
        selector: Option<Selector>,
    },
    ExpectPresent {
        selector: Selector,
    },
    ExpectMissing {
        selector: Selector,
    },
    ExpectChildCount {
        parent: Selector,
        expected: usize,
    },
    ExpectChild {
        parent: Selector,
        index: usize,
        child: Selector,
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
            | Command::Sleep { .. }
            | Command::Key { .. }
            | Command::ExpectText { .. }
            | Command::ExpectCountdown { .. }
            | Command::ExpectFocus { .. }
            | Command::ExpectPresent { .. }
            | Command::ExpectMissing { .. }
            | Command::ExpectChildCount { .. }
            | Command::ExpectChild { .. }
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
        let (selector, remainder) = parse_selector_prefix(path, line, source, name)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::Activate { selector });
    }
    if let Some(digits) = source.strip_prefix("sleep ") {
        let millis = if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            digits.parse::<u64>().map_err(|error| {
                diagnostic(
                    path,
                    line,
                    source,
                    &format!("invalid sleep duration in milliseconds: {error}"),
                )
            })?
        } else {
            return Err(diagnostic(
                path,
                line,
                source,
                "sleep duration must be a non-negative decimal integer in milliseconds",
            ));
        };
        return Ok(Command::Sleep { millis });
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
        let (selector, encoded) = parse_selector_prefix(path, line, source, arguments)?;
        if encoded.is_empty() {
            return Err(diagnostic(
                path,
                line,
                source,
                "expected: expect text <selector> <JSON-string>",
            ));
        }
        let expected: String = serde_json::from_str(encoded).map_err(|error| {
            diagnostic(path, line, source, &format!("invalid JSON string: {error}"))
        })?;
        return Ok(Command::ExpectText { selector, expected });
    }
    if let Some(name) = source.strip_prefix("expect countdown ") {
        let (selector, remainder) = parse_selector_prefix(path, line, source, name)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::ExpectCountdown { selector });
    }
    if let Some(name) = source.strip_prefix("expect focus ") {
        if name == "none" {
            return Ok(Command::ExpectFocus { selector: None });
        }
        let (selector, remainder) = parse_selector_prefix(path, line, source, name)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::ExpectFocus {
            selector: Some(selector),
        });
    }
    if let Some(value) = source.strip_prefix("expect present ") {
        let (selector, remainder) = parse_selector_prefix(path, line, source, value)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::ExpectPresent { selector });
    }
    if let Some(value) = source.strip_prefix("expect missing ") {
        let (selector, remainder) = parse_selector_prefix(path, line, source, value)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::ExpectMissing { selector });
    }
    if let Some(value) = source.strip_prefix("expect child-count ") {
        let (parent, remainder) = parse_selector_prefix(path, line, source, value)?;
        let expected = parse_usize(path, line, source, remainder, "child count")?;
        return Ok(Command::ExpectChildCount { parent, expected });
    }
    if let Some(value) = source.strip_prefix("expect child ") {
        let (parent, remainder) = parse_selector_prefix(path, line, source, value)?;
        let (index, remainder) = parse_usize_prefix(path, line, source, remainder, "child index")?;
        let (child, remainder) = parse_selector_prefix(path, line, source, remainder)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::ExpectChild {
            parent,
            index,
            child,
        });
    }
    Err(diagnostic(
        path,
        line,
        source,
        "unknown command; expected state, mount, activate, sleep, key, restart, or an expect assertion",
    ))
}

fn parse_selector_prefix<'a>(
    path: &Path,
    line: usize,
    source: &str,
    input: &'a str,
) -> Result<(Selector, &'a str), TestError> {
    if let Some(input) = input.strip_prefix("derived ") {
        let (namespace, input) = parse_json_string_prefix(path, line, source, input)?;
        let (item, input) = parse_u64_prefix(path, line, source, input, "derived item ID")?;
        if item == 0 {
            return Err(diagnostic(
                path,
                line,
                source,
                "derived item ID must be nonzero",
            ));
        }
        let (role, remainder) = parse_json_string_prefix(path, line, source, input)?;
        youth_sdk::derived_node_id(&namespace, item, &role).map_err(|error| {
            diagnostic(
                path,
                line,
                source,
                &format!("invalid derived selector: {error}"),
            )
        })?;
        return Ok((
            Selector::Derived {
                namespace,
                item,
                role,
            },
            remainder,
        ));
    }
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let name = &input[..end];
    validate_name(path, line, source, name)?;
    Ok((Selector::Static(name.to_owned()), input[end..].trim_start()))
}

fn parse_u64_prefix<'a>(
    path: &Path,
    line: usize,
    source: &str,
    input: &'a str,
    label: &str,
) -> Result<(u64, &'a str), TestError> {
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let value = &input[..end];
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(diagnostic(
            path,
            line,
            source,
            &format!("{label} must be an unsigned decimal integer"),
        ));
    }
    let parsed = value
        .parse()
        .map_err(|error| diagnostic(path, line, source, &format!("invalid {label}: {error}")))?;
    Ok((parsed, input[end..].trim_start()))
}

fn parse_usize_prefix<'a>(
    path: &Path,
    line: usize,
    source: &str,
    input: &'a str,
    label: &str,
) -> Result<(usize, &'a str), TestError> {
    let (value, remainder) = parse_u64_prefix(path, line, source, input, label)?;
    let value = usize::try_from(value)
        .map_err(|_| diagnostic(path, line, source, &format!("{label} is too large")))?;
    Ok((value, remainder))
}

fn parse_usize(
    path: &Path,
    line: usize,
    source: &str,
    input: &str,
    label: &str,
) -> Result<usize, TestError> {
    let (value, remainder) = parse_usize_prefix(path, line, source, input, label)?;
    require_empty(path, line, source, remainder)?;
    Ok(value)
}

fn require_empty(path: &Path, line: usize, source: &str, remainder: &str) -> Result<(), TestError> {
    if remainder.is_empty() {
        Ok(())
    } else {
        Err(diagnostic(path, line, source, "unexpected trailing input"))
    }
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
    run_directory_with_options(tests_directory, component, app_id, RunOptions::default()).await
}

pub async fn run_directory_with_options(
    tests_directory: &Path,
    component: &Path,
    app_id: &AppId,
    options: RunOptions,
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
        run_file_with_options(path, component, app_id, options).await?;
    }
    Ok(files.len())
}

pub async fn run_file(path: &Path, component: &Path, app_id: &AppId) -> Result<(), TestError> {
    run_file_with_options(path, component, app_id, RunOptions::default()).await
}

pub async fn run_file_with_options(
    path: &Path,
    component: &Path,
    app_id: &AppId,
    options: RunOptions,
) -> Result<(), TestError> {
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
    let mut events = app.subscribe();
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
                if options.verify_view_convergence {
                    verify_view_convergence(path, &located, &app).await?;
                }
            }
            Command::Activate { selector } => {
                let node = selector.node_id();
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
            Command::Sleep { millis } => {
                tokio::time::sleep(std::time::Duration::from_millis(*millis)).await;
            }
            Command::Restart => {
                app.stop()
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                app = spawn(component, app_id, &state_file)?;
                events = app.subscribe();
                snapshot = Some(
                    app.mount()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
                interaction = InteractionState::default();
                reconcile(&mut interaction, snapshot.as_ref().unwrap());
                if options.verify_view_convergence {
                    verify_view_convergence(path, &located, &app).await?;
                }
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
            Command::ExpectText { selector, expected } => {
                expect_text(
                    path,
                    &located,
                    snapshot.as_ref().expect("parser requires mount"),
                    selector,
                    expected,
                )?;
            }
            Command::ExpectCountdown { selector } => {
                expect_countdown(
                    path,
                    &located,
                    snapshot.as_ref().expect("parser requires mount"),
                    selector,
                )?;
            }
            Command::ExpectFocus { selector } => {
                let expected = selector.as_ref().map(Selector::node_id);
                if interaction.focused() != expected {
                    return Err(TestError::Diagnostic {
                        path: path.to_path_buf(),
                        line: located.line,
                        command: located.source.clone(),
                        message: format!(
                            "expected focus {selector:?}; observed {:?}",
                            interaction.focused().map(NodeId::get)
                        ),
                    });
                }
            }
            Command::ExpectPresent { selector } => {
                let snapshot = snapshot.as_ref().expect("parser requires mount");
                let observed = snapshot
                    .nodes
                    .iter()
                    .find(|node| node.id == selector.node_id());
                if observed.is_none() {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!("expected {selector:?} to be present; observed no semantic node"),
                    ));
                }
            }
            Command::ExpectMissing { selector } => {
                let snapshot = snapshot.as_ref().expect("parser requires mount");
                let observed = snapshot
                    .nodes
                    .iter()
                    .find(|node| node.id == selector.node_id());
                if let Some(node) = observed {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected {selector:?} to be missing; observed {}",
                            describe(Some(&node.data))
                        ),
                    ));
                }
            }
            Command::ExpectChildCount { parent, expected } => {
                let snapshot = snapshot.as_ref().expect("parser requires mount");
                let observed = snapshot
                    .nodes
                    .iter()
                    .find(|node| node.id == parent.node_id());
                match observed {
                    Some(node) if node.children.len() == *expected => {}
                    Some(node) => {
                        return Err(assertion_error(
                            path,
                            &located,
                            format!(
                                "expected {parent:?} to have {expected} children; observed {}",
                                node.children.len()
                            ),
                        ));
                    }
                    None => {
                        return Err(assertion_error(
                            path,
                            &located,
                            format!(
                                "expected {parent:?} to have {expected} children; observed no semantic node"
                            ),
                        ));
                    }
                }
            }
            Command::ExpectChild {
                parent,
                index,
                child,
            } => {
                let snapshot = snapshot.as_ref().expect("parser requires mount");
                let observed = snapshot
                    .nodes
                    .iter()
                    .find(|node| node.id == parent.node_id());
                let observed_child = observed.and_then(|node| node.children.get(*index)).copied();
                let expected_child = child.node_id();
                if observed_child != Some(expected_child) {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected child {index} of {parent:?} to be {child:?}; observed {:?}",
                            observed_child.map(NodeId::get)
                        ),
                    ));
                }
            }
            Command::ExpectState { key, expected } => {
                expect_state(path, &located, app_id, &state_file, key, expected)?;
            }
        }
        if options.verify_view_convergence {
            verify_committed_events(path, &located, &app, &mut events).await?;
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

async fn verify_committed_events(
    path: &Path,
    located: &LocatedCommand,
    app: &YouthAppHandle,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> Result<(), TestError> {
    loop {
        match events.try_recv() {
            Ok(RuntimeEvent::TurnCommitted(_)) => {
                verify_view_convergence(path, located, app).await?;
            }
            Ok(RuntimeEvent::Faulted(_) | RuntimeEvent::SnapshotReplaced(_)) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return Ok(()),
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                return Err(assertion_error(
                    path,
                    located,
                    "runtime observer stream closed during convergence verification".into(),
                ));
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
                return Err(assertion_error(
                    path,
                    located,
                    format!(
                        "runtime observer lagged by {count} events during convergence verification"
                    ),
                ));
            }
        }
    }
}

async fn verify_view_convergence(
    path: &Path,
    located: &LocatedCommand,
    app: &YouthAppHandle,
) -> Result<(), TestError> {
    let verification = app
        .verify_view()
        .await
        .map_err(|error| runtime(path, located, error))?;
    compare_guest_semantics(&verification.retained, &verification.reconstructed).map_err(
        |message| assertion_error(path, located, format!("view convergence failed: {message}")),
    )
}

fn compare_guest_semantics(
    retained: &TreeSnapshot,
    reconstructed: &TreeSnapshot,
) -> Result<(), String> {
    let retained = retained
        .nodes
        .iter()
        .filter(|node| !matches!(&node.data, NodeData::Root))
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let reconstructed = reconstructed
        .nodes
        .iter()
        .filter(|node| !matches!(&node.data, NodeData::Root))
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let missing = retained
        .keys()
        .filter(|id| !reconstructed.contains_key(id))
        .map(|id| id.get())
        .collect::<Vec<_>>();
    let extra = reconstructed
        .keys()
        .filter(|id| !retained.contains_key(id))
        .map(|id| id.get())
        .collect::<Vec<_>>();
    let changed = retained
        .iter()
        .filter_map(|(id, retained)| {
            reconstructed
                .get(id)
                .filter(|reconstructed| {
                    retained.data != reconstructed.data
                        || retained.children != reconstructed.children
                })
                .map(|_| id.get())
        })
        .collect::<Vec<_>>();
    if missing.is_empty() && extra.is_empty() && changed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing nodes {missing:?}; extra nodes {extra:?}; changed nodes {changed:?}"
        ))
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

#[cfg(test)]
fn named_id(name: &str) -> NodeId {
    Selector::Static(name.to_owned()).node_id()
}

fn expect_text(
    path: &Path,
    located: &LocatedCommand,
    snapshot: &TreeSnapshot,
    selector: &Selector,
    expected: &str,
) -> Result<(), TestError> {
    let id = selector.node_id();
    let node = snapshot.nodes.iter().find(|node| node.id == id);
    match node.map(|node| &node.data) {
        Some(data) if data.text_value().is_some_and(|value| value == expected) => Ok(()),
        observed => Err(TestError::Diagnostic {
            path: path.to_path_buf(),
            line: located.line,
            command: located.source.clone(),
            message: format!(
                "expected text {selector:?} to equal {expected:?}; observed {}",
                describe(observed)
            ),
        }),
    }
}

fn expect_countdown(
    path: &Path,
    located: &LocatedCommand,
    snapshot: &TreeSnapshot,
    selector: &Selector,
) -> Result<(), TestError> {
    let id = selector.node_id();
    let node = snapshot.nodes.iter().find(|node| node.id == id);
    match node.map(|node| &node.data) {
        Some(data) if data.countdown_ref().is_some() => Ok(()),
        observed => Err(TestError::Diagnostic {
            path: path.to_path_buf(),
            line: located.line,
            command: located.source.clone(),
            message: format!(
                "expected countdown {selector:?}; observed {}",
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
        message: format!("{:?}: {error}", error.category()),
    }
}

fn assertion_error(path: &Path, command: &LocatedCommand, message: String) -> TestError {
    TestError::Diagnostic {
        path: path.to_path_buf(),
        line: command.line,
        command: command.source.clone(),
        message,
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
                selector: Selector::Static("count".into()),
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
                selector: Selector::Static("remaining".into()),
            }
        );
    }

    #[test]
    fn parses_derived_selectors_and_structural_assertions() {
        let script = parse(
            Path::new("todo.youth-test"),
            r#"mount
activate derived "todo" 1 "toggle"
expect text derived "todo" 1 "title" "Task 1"
expect present derived "todo" 1 "row"
expect missing derived "todo" 2 "row"
expect child-count items 5
expect child items 0 derived "todo" 1 "row"
expect focus derived "todo" 1 "toggle"
"#,
        )
        .unwrap();
        let derived = Selector::Derived {
            namespace: "todo".into(),
            item: 1,
            role: "toggle".into(),
        };
        assert_eq!(
            script.commands[1].command,
            Command::Activate {
                selector: derived.clone()
            }
        );
        assert_eq!(
            script.commands[7].command,
            Command::ExpectFocus {
                selector: Some(derived)
            }
        );
        assert!(
            parse(
                Path::new("bad.youth-test"),
                "mount\nexpect present derived \"todo\" 0 \"row\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn parses_sleep_and_rejects_invalid_durations() {
        let script = parse(Path::new("sleep.youth-test"), "mount\nsleep 150\n").unwrap();
        assert_eq!(script.commands[1].command, Command::Sleep { millis: 150 });

        for source in ["mount\nsleep abc\n", "mount\nsleep -5\n"] {
            let error = parse(Path::new("bad.youth-test"), source).unwrap_err();
            assert!(
                error.to_string().contains(
                    "sleep duration must be a non-negative decimal integer in milliseconds"
                ),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_sleep_before_initial_mount() {
        let error = parse(Path::new("bad.youth-test"), "sleep 1\nmount\n").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("command appears before the required initial mount"),
            "{error}"
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
        assert_eq!(
            youth_sdk::derived_node_id("todo", 1, "row").unwrap(),
            0xe4ea_3f45_0dc3_046f
        );
        assert_eq!(
            youth_sdk::derived_node_id("todo", 42, "title").unwrap(),
            0x872f_87fc_4c39_8fe4
        );
        assert_eq!(
            youth_sdk::derived_command_id("todo", 1, "toggle").unwrap(),
            0x8b5e_c3bc_b296_c4a5
        );
    }

    #[test]
    fn convergence_diagnostics_separate_missing_extra_and_changed_nodes() {
        let id = |value| NodeId::new(value).unwrap();
        let retained = TreeSnapshot {
            revision: 1,
            root: id(1),
            nodes: vec![
                youth_tree::Node {
                    id: id(1),
                    data: NodeData::Root,
                    children: vec![id(2)],
                },
                youth_tree::Node {
                    id: id(2),
                    data: NodeData::Text {
                        value: "old".into(),
                    },
                    children: vec![],
                },
                youth_tree::Node {
                    id: id(3),
                    data: NodeData::Text {
                        value: "missing".into(),
                    },
                    children: vec![],
                },
            ],
        };
        let reconstructed = TreeSnapshot {
            revision: 1,
            root: id(1),
            nodes: vec![
                youth_tree::Node {
                    id: id(1),
                    data: NodeData::Root,
                    children: vec![id(2)],
                },
                youth_tree::Node {
                    id: id(2),
                    data: NodeData::Text {
                        value: "new".into(),
                    },
                    children: vec![],
                },
                youth_tree::Node {
                    id: id(4),
                    data: NodeData::Text {
                        value: "extra".into(),
                    },
                    children: vec![],
                },
            ],
        };
        assert_eq!(
            compare_guest_semantics(&retained, &reconstructed).unwrap_err(),
            "missing nodes [3]; extra nodes [4]; changed nodes [2]"
        );
    }
}
