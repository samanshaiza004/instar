//! `.youth-test` parser and real headless-runtime runner.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;
use youth_runtime::{
    ClipboardService, EditorLocalEdit, EditorLocalEditResult, RecordingClipboardService,
    RuntimeLimits, YouthAppConfig, YouthAppHandle,
};
use youth_state::{AppId, GuestCallPhase, StateLimits, StateLocation, StateStore, StateValue};
use youth_tree::{NodeData, NodeId, Tree, TreeSnapshot};

use youth_interaction::{InteractionState, LogicalKey, Modifiers, SemanticAction};
use youth_runtime::RuntimeEvent;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunOptions {
    pub verify_view_convergence: bool,
}

/// Oldest `.youth-test` format version this runner still accepts.
pub const MIN_FORMAT_VERSION: u32 = 1;
/// Newest `.youth-test` format version this runner understands. A file
/// with no `youth-test <n>` header is treated as version 1 (legacy).
///
/// Versions the *test language* grammar, independently of the Youth
/// application protocol (`youth:app@0.0.6` etc.) that the driven
/// component implements -- one test-language version can drive many
/// supported component profiles.
pub const CURRENT_FORMAT_VERSION: u32 = 1;

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

impl fmt::Display for Selector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Static(name) => write!(formatter, "{name:?}"),
            Self::Derived {
                namespace,
                item,
                role,
            } => write!(formatter, "derived {namespace:?} {item} {role:?}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    FixtureFile {
        path: String,
        bytes: Vec<u8>,
    },
    GrantDocument {
        path: String,
    },
    State {
        key: String,
        value: StateValue,
    },
    Mount,
    /// Direct guest activation: sends the activation command straight to
    /// the guest by `NodeId`, bypassing host interaction policy entirely
    /// (no present/enabled/focus/role check). Useful for testing guest
    /// command guards, and for targeting a control the host would refuse
    /// to let a real user reach (e.g. a disabled button). Written `invoke`
    /// in the DSL; `activate` parses to the same command as a
    /// backward-compatible alias.
    Activate {
        selector: Selector,
    },
    /// Semantic click: requires the target to be present, enabled, and an
    /// activatable role (a button), exactly as `youth_interaction`'s host
    /// policy would require of a real pointer click -- then activates it.
    /// Real headless hit-testing/geometry is not implemented yet, so this
    /// enforces only that semantic subset of click policy.
    Click {
        selector: Selector,
    },
    /// Real wall-clock sleep in the test process. Kept for backward
    /// compatibility: a file with no `advance time` command runs entirely
    /// against the production system clock/wake-driver, exactly as before,
    /// so `sleep` still means a genuine wait. A file that also uses
    /// `advance time` must spell a real wait as `sleep real`/`wall-sleep`
    /// instead -- see [`Command::SleepReal`].
    Sleep {
        millis: u64,
    },
    /// Advances the file's injected virtual `DeadlineClock` and
    /// `WakeDriver`, fires every schedule that becomes due, drains the
    /// resulting host work (including any newly rearmed schedule), and
    /// stops once quiescent. Present anywhere in a file switches that
    /// file's entire run to a virtual clock/wake-driver instead of the
    /// production ones (see [`Command::Sleep`]'s note).
    AdvanceTime {
        millis: u64,
    },
    /// Real wall-clock sleep, explicit and independent of the virtual
    /// clock -- for production-clock smoke evidence in a file that also
    /// uses `advance time`. Written `sleep real` or `wall-sleep` in the
    /// DSL.
    SleepReal {
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
    /// Committed text input against a host-owned Editor session --
    /// `youth_runtime::EditorLocalEdit::InsertText`, the whole string in
    /// one host-local edit (not iterated key-by-key, which would test
    /// keyboard shortcut handling rather than the editor contract).
    TypeText {
        selector: Selector,
        text: String,
    },
    /// Replaces the current selection with `text`. No separate host
    /// primitive exists for this -- it is exactly `InsertText`, which
    /// already replaces an active selection -- kept as its own DSL command
    /// only to document a test's intent.
    ReplaceSelection {
        selector: Selector,
        text: String,
    },
    /// Writes `text` to the injected host clipboard test double, then
    /// applies `EditorLocalEdit::Paste`.
    Paste {
        selector: Selector,
        text: String,
    },
    /// Sets or replaces the IME preedit text
    /// (`EditorLocalEdit::ImeSetCompose`). `start` and `update` parse to
    /// the same host call -- the session itself distinguishes a fresh
    /// composition from a continued one by whether a composition is
    /// already pending, not by a different call -- kept as two DSL verbs
    /// only to document a test's intent.
    ComposeStart {
        selector: Selector,
        text: String,
    },
    ComposeUpdate {
        selector: Selector,
        text: String,
    },
    /// Sets the preedit to `text`, then commits it as ordinary buffer
    /// content in the same step (`ImeSetCompose` followed by
    /// `ImeFinishCompose`) -- so a test can commit a composition without a
    /// separate preceding `update`.
    ComposeCommit {
        selector: Selector,
        text: String,
    },
    /// Cancels IME composition, discarding the preedit text
    /// (`EditorLocalEdit::ImeClearCompose`).
    ComposeCancel {
        selector: Selector,
    },
    /// Asserts the Editor's current live buffer text. Distinct from
    /// `expect text`, which reads the retained tree's `Editor.text` field
    /// -- accurate right after mount/resync/restart, but not kept in sync
    /// with host-local edits (`type`, `paste`, `compose`, `undo`/`redo`
    /// aren't tree patches). This reads the live session instead, via the
    /// same result every host-local edit already returns.
    ExpectEditorText {
        selector: Selector,
        expected: String,
    },
    /// Asserts the Editor's current selection as a grapheme-cluster
    /// range (not bytes, UTF-16 units, or Unicode scalar values) --
    /// `youth_editor_engine`'s `EditorLocalEditResult::{cursor,selection}`
    /// are byte offsets; converted here at the assertion layer, mirroring
    /// the byte-to-char-index conversion AccessKit support already does. A
    /// collapsed cursor at grapheme position `n` is `n..n`.
    ExpectEditorSelection {
        selector: Selector,
        start: usize,
        end: usize,
    },
    ExternalWrite {
        path: String,
        bytes: Vec<u8>,
    },
    ExpectFile {
        path: String,
        expected: Vec<u8>,
    },
    ExpectEditorDirty {
        selector: Selector,
        expected: bool,
    },
    /// Records the current lifetime guest-call count
    /// (`AppInspection::guest_call_count`) under `label`, for a later
    /// `measure expect` to diff against. Namespaced under harness
    /// observation, not the ordinary semantic-tree `expect` vocabulary --
    /// these are facts about the host's own behavior, not app semantics.
    /// Does not span a `restart`: the counter is per process instance.
    MeasureBegin {
        label: String,
    },
    /// Asserts that the lifetime guest-call count has advanced by exactly
    /// `expected` since the matching `measure begin label`. This is the
    /// only counter implemented today -- state-calls, state-writes,
    /// commits, rollbacks, host-repaints, observer-outcomes, and
    /// pending-deliveries are deferred; none of them are a cumulative
    /// counter already exposed via `YouthAppHandle::inspect`, unlike
    /// guest-turns.
    MeasureExpectGuestTurns {
        label: String,
        expected: u64,
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
    /// The declared `youth-test <n>` format version, or 1 if the file has
    /// no version header (legacy).
    pub version: u32,
    pub commands: Vec<LocatedCommand>,
}

pub fn parse(path: &Path, source: &str) -> Result<Script, TestError> {
    let mut commands = Vec::new();
    let mut mounted = false;
    let mut mount_seen = false;
    let mut grant_seen = false;
    let mut version = None;
    let mut first_content_line = true;
    for (offset, raw) in source.lines().enumerate() {
        let line = offset + 1;
        let source_line = strip_comment(raw).trim();
        if source_line.is_empty() {
            continue;
        }
        if first_content_line {
            first_content_line = false;
            if let Some(declared) = source_line.strip_prefix("youth-test ") {
                version = Some(parse_format_version(path, line, source_line, declared)?);
                continue;
            }
        }
        let command = parse_command(path, line, source_line)?;
        match command {
            Command::State { .. } | Command::FixtureFile { .. } | Command::GrantDocument { .. } => {
                if mounted {
                    return Err(diagnostic(
                        path,
                        line,
                        source_line,
                        "state may only be seeded before the initial mount",
                    ));
                }
                if matches!(command, Command::GrantDocument { .. }) {
                    if grant_seen {
                        return Err(diagnostic(
                            path,
                            line,
                            source_line,
                            "a test may grant exactly one text document",
                        ));
                    }
                    grant_seen = true;
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
            | Command::Click { .. }
            | Command::Sleep { .. }
            | Command::AdvanceTime { .. }
            | Command::SleepReal { .. }
            | Command::Key { .. }
            | Command::ExpectText { .. }
            | Command::ExpectCountdown { .. }
            | Command::ExpectFocus { .. }
            | Command::ExpectPresent { .. }
            | Command::ExpectMissing { .. }
            | Command::ExpectChildCount { .. }
            | Command::ExpectChild { .. }
            | Command::ExpectState { .. }
            | Command::TypeText { .. }
            | Command::ReplaceSelection { .. }
            | Command::Paste { .. }
            | Command::ComposeStart { .. }
            | Command::ComposeUpdate { .. }
            | Command::ComposeCommit { .. }
            | Command::ComposeCancel { .. }
            | Command::ExpectEditorText { .. }
            | Command::ExpectEditorSelection { .. }
            | Command::ExternalWrite { .. }
            | Command::ExpectFile { .. }
            | Command::ExpectEditorDirty { .. }
            | Command::MeasureBegin { .. }
            | Command::MeasureExpectGuestTurns { .. }
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
    let uses_virtual_time = commands
        .iter()
        .any(|located| matches!(located.command, Command::AdvanceTime { .. }));
    if uses_virtual_time
        && let Some(bare_sleep) = commands
            .iter()
            .find(|located| matches!(located.command, Command::Sleep { .. }))
    {
        return Err(diagnostic(
            path,
            bare_sleep.line,
            &bare_sleep.source,
            "this file also uses `advance time`, which runs against a virtual clock; `sleep` would only block the test process without advancing it, so use `sleep real`/`wall-sleep` for a genuine wall-clock wait instead",
        ));
    }
    Ok(Script {
        version: version.unwrap_or(1),
        commands,
    })
}

fn parse_format_version(
    path: &Path,
    line: usize,
    source: &str,
    declared: &str,
) -> Result<u32, TestError> {
    if declared.is_empty() || !declared.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(diagnostic(
            path,
            line,
            source,
            "expected: youth-test <format version, a decimal integer>",
        ));
    }
    let version: u32 = declared.parse().map_err(|error| {
        diagnostic(
            path,
            line,
            source,
            &format!("invalid format version: {error}"),
        )
    })?;
    if !(MIN_FORMAT_VERSION..=CURRENT_FORMAT_VERSION).contains(&version) {
        return Err(diagnostic(
            path,
            line,
            source,
            &format!(
                "unsupported .youth-test format version {version}; this runner supports {MIN_FORMAT_VERSION}..={CURRENT_FORMAT_VERSION}"
            ),
        ));
    }
    Ok(version)
}

fn parse_command(path: &Path, line: usize, source: &str) -> Result<Command, TestError> {
    for (prefix, encoding) in [
        ("fixture file ", FileEncoding::Text),
        ("fixture file-hex ", FileEncoding::Hex),
        ("fixture file-base64 ", FileEncoding::Base64),
    ] {
        if let Some(arguments) = source.strip_prefix(prefix) {
            let (file, bytes) = parse_file_arguments(path, line, source, arguments, encoding)?;
            return Ok(Command::FixtureFile { path: file, bytes });
        }
    }
    if let Some(arguments) = source.strip_prefix("grant document ") {
        let (file, remainder) = parse_json_string_prefix(path, line, source, arguments)?;
        require_empty(path, line, source, remainder)?;
        validate_fixture_path(path, line, source, &file)?;
        return Ok(Command::GrantDocument { path: file });
    }
    for (prefix, encoding) in [
        ("external write ", FileEncoding::Text),
        ("external write-hex ", FileEncoding::Hex),
        ("external write-base64 ", FileEncoding::Base64),
    ] {
        if let Some(arguments) = source.strip_prefix(prefix) {
            let (file, bytes) = parse_file_arguments(path, line, source, arguments, encoding)?;
            return Ok(Command::ExternalWrite { path: file, bytes });
        }
    }
    for (prefix, encoding) in [
        ("expect file ", FileEncoding::Text),
        ("expect file-hex ", FileEncoding::Hex),
        ("expect file-base64 ", FileEncoding::Base64),
    ] {
        if let Some(arguments) = source.strip_prefix(prefix) {
            let (file, expected) = parse_file_arguments(path, line, source, arguments, encoding)?;
            return Ok(Command::ExpectFile {
                path: file,
                expected,
            });
        }
    }
    if let Some(arguments) = source.strip_prefix("expect editor dirty ") {
        let (selector, remainder) = parse_selector_prefix(path, line, source, arguments)?;
        let expected = match remainder {
            "true" => true,
            "false" => false,
            _ => {
                return Err(diagnostic(
                    path,
                    line,
                    source,
                    "expected: expect editor dirty <selector> true|false",
                ));
            }
        };
        return Ok(Command::ExpectEditorDirty { selector, expected });
    }
    if source == "mount" {
        return Ok(Command::Mount);
    }
    if source == "restart" {
        return Ok(Command::Restart);
    }
    if let Some(name) = source
        .strip_prefix("invoke ")
        .or_else(|| source.strip_prefix("activate "))
    {
        let (selector, remainder) = parse_selector_prefix(path, line, source, name)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::Activate { selector });
    }
    if let Some(name) = source.strip_prefix("click ") {
        let (selector, remainder) = parse_selector_prefix(path, line, source, name)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::Click { selector });
    }
    if let Some(rest) = source.strip_prefix("type ") {
        let (selector, text) = parse_selector_then_text(path, line, source, rest)?;
        return Ok(Command::TypeText { selector, text });
    }
    if let Some(rest) = source.strip_prefix("replace-selection ") {
        let (selector, text) = parse_selector_then_text(path, line, source, rest)?;
        return Ok(Command::ReplaceSelection { selector, text });
    }
    if let Some(rest) = source.strip_prefix("paste ") {
        let (selector, text) = parse_selector_then_text(path, line, source, rest)?;
        return Ok(Command::Paste { selector, text });
    }
    if let Some(rest) = source.strip_prefix("compose ") {
        let (selector, remainder) = parse_selector_prefix(path, line, source, rest)?;
        if let Some(text_input) = remainder.strip_prefix("start ") {
            let (text, remainder) = parse_json_string_prefix(path, line, source, text_input)?;
            require_empty(path, line, source, remainder)?;
            return Ok(Command::ComposeStart { selector, text });
        }
        if let Some(text_input) = remainder.strip_prefix("update ") {
            let (text, remainder) = parse_json_string_prefix(path, line, source, text_input)?;
            require_empty(path, line, source, remainder)?;
            return Ok(Command::ComposeUpdate { selector, text });
        }
        if let Some(text_input) = remainder.strip_prefix("commit ") {
            let (text, remainder) = parse_json_string_prefix(path, line, source, text_input)?;
            require_empty(path, line, source, remainder)?;
            return Ok(Command::ComposeCommit { selector, text });
        }
        if remainder == "cancel" {
            return Ok(Command::ComposeCancel { selector });
        }
        return Err(diagnostic(
            path,
            line,
            source,
            "expected: compose <selector> start|update|commit <JSON-string>, or compose <selector> cancel",
        ));
    }
    if let Some(rest) = source.strip_prefix("advance time ") {
        let millis = parse_millis_with_unit_suffix(path, line, source, rest)?;
        return Ok(Command::AdvanceTime { millis });
    }
    if let Some(rest) = source
        .strip_prefix("sleep real ")
        .or_else(|| source.strip_prefix("wall-sleep "))
    {
        let millis = parse_millis_with_unit_suffix(path, line, source, rest)?;
        return Ok(Command::SleepReal { millis });
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
                "expected: state <boolean|integer|text|bytes|utf8-bytes|bytes-hex|bytes-base64> <JSON-string-key> <value>",
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
    if let Some(arguments) = source.strip_prefix("expect editor text ") {
        let (selector, encoded) = parse_selector_prefix(path, line, source, arguments)?;
        if encoded.is_empty() {
            return Err(diagnostic(
                path,
                line,
                source,
                "expected: expect editor text <selector> <JSON-string>",
            ));
        }
        let expected: String = serde_json::from_str(encoded).map_err(|error| {
            diagnostic(path, line, source, &format!("invalid JSON string: {error}"))
        })?;
        return Ok(Command::ExpectEditorText { selector, expected });
    }
    if let Some(arguments) = source.strip_prefix("expect editor selection ") {
        let (selector, remainder) = parse_selector_prefix(path, line, source, arguments)?;
        let (start, end, remainder) = parse_grapheme_range(path, line, source, remainder)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::ExpectEditorSelection {
            selector,
            start,
            end,
        });
    }
    if let Some(rest) = source.strip_prefix("measure begin ") {
        let (label, remainder) = parse_json_string_prefix(path, line, source, rest)?;
        require_empty(path, line, source, remainder)?;
        return Ok(Command::MeasureBegin { label });
    }
    if let Some(rest) = source.strip_prefix("measure expect ") {
        let (label, remainder) = parse_json_string_prefix(path, line, source, rest)?;
        let counter_error = || {
            diagnostic(
                path,
                line,
                source,
                "expected: measure expect <JSON-string-label> guest-turns <count> (the only measure counter implemented today)",
            )
        };
        let remainder = remainder
            .strip_prefix("guest-turns ")
            .ok_or_else(counter_error)?;
        if remainder.is_empty() || !remainder.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(counter_error());
        }
        let expected: u64 = remainder
            .parse()
            .map_err(|error| diagnostic(path, line, source, &format!("invalid count: {error}")))?;
        return Ok(Command::MeasureExpectGuestTurns { label, expected });
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
        "unknown command; expected state, mount, invoke, activate, click, sleep, advance, key, type, replace-selection, paste, compose, measure, restart, or an expect assertion",
    ))
}

#[derive(Clone, Copy)]
enum FileEncoding {
    Text,
    Hex,
    Base64,
}

fn parse_file_arguments(
    path: &Path,
    line: usize,
    source: &str,
    arguments: &str,
    encoding: FileEncoding,
) -> Result<(String, Vec<u8>), TestError> {
    let (file, remainder) = parse_json_string_prefix(path, line, source, arguments)?;
    validate_fixture_path(path, line, source, &file)?;
    let (encoded, remainder) = parse_json_string_prefix(path, line, source, remainder)?;
    require_empty(path, line, source, remainder)?;
    let bytes = match encoding {
        FileEncoding::Text => encoded.into_bytes(),
        FileEncoding::Hex => {
            decode_hex(&encoded).map_err(|message| diagnostic(path, line, source, &message))?
        }
        FileEncoding::Base64 => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(encoded.as_bytes())
                .map_err(|error| {
                    diagnostic(
                        path,
                        line,
                        source,
                        &format!("invalid base64 bytes: {error}"),
                    )
                })?;
            if base64::engine::general_purpose::STANDARD.encode(&decoded) != encoded {
                return Err(diagnostic(
                    path,
                    line,
                    source,
                    "base64 bytes must use canonical padded RFC 4648 encoding",
                ));
            }
            decoded
        }
    };
    Ok((file, bytes))
}

fn validate_fixture_path(
    test_path: &Path,
    line: usize,
    source: &str,
    value: &str,
) -> Result<(), TestError> {
    let candidate = Path::new(value);
    if value.is_empty()
        || candidate.is_absolute()
        || candidate.components().any(|component| {
            matches!(
                component,
                std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
                    | std::path::Component::CurDir
                    | std::path::Component::ParentDir
            )
        })
    {
        return Err(diagnostic(
            test_path,
            line,
            source,
            "file paths must be nonempty relative paths without `.` or `..` components",
        ));
    }
    Ok(())
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
    // The canonical form: a quoted exact name, which can hold whitespace,
    // `#`, and any other UTF-8 the bare-identifier shorthand below cannot
    // safely delimit -- Youth's own node-name identity model is not
    // restricted to single tokens, so the DSL must not accidentally narrow
    // what it can select.
    if input.starts_with('"') {
        let (name, remainder) = parse_json_string_prefix(path, line, source, input)?;
        if name.is_empty() {
            return Err(diagnostic(
                path,
                line,
                source,
                "node name must not be empty",
            ));
        }
        return Ok((Selector::Static(name), remainder));
    }
    // The bare-identifier shorthand: convenient for the common case of an
    // ASCII-identifier-shaped name with no whitespace.
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let name = &input[..end];
    validate_name(path, line, source, name)?;
    Ok((Selector::Static(name.to_owned()), input[end..].trim_start()))
}

/// The shared `<selector> <JSON-string>` shape behind `type`,
/// `replace-selection`, and `paste`.
fn parse_selector_then_text(
    path: &Path,
    line: usize,
    source: &str,
    input: &str,
) -> Result<(Selector, String), TestError> {
    let (selector, remainder) = parse_selector_prefix(path, line, source, input)?;
    let (text, remainder) = parse_json_string_prefix(path, line, source, remainder)?;
    require_empty(path, line, source, remainder)?;
    Ok((selector, text))
}

/// Parses `graphemes <start>..<end>`, the explicit position unit
/// `expect editor selection` requires.
fn parse_grapheme_range<'a>(
    path: &Path,
    line: usize,
    source: &str,
    input: &'a str,
) -> Result<(usize, usize, &'a str), TestError> {
    let invalid = || {
        diagnostic(
            path,
            line,
            source,
            "expected: graphemes <start>..<end> (unsigned decimal grapheme-cluster indices)",
        )
    };
    let input = input.strip_prefix("graphemes ").ok_or_else(invalid)?;
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let range_text = &input[..end];
    let (start_text, end_text) = range_text.split_once("..").ok_or_else(invalid)?;
    let start: usize = start_text.parse().map_err(|_| invalid())?;
    let end_value: usize = end_text.parse().map_err(|_| invalid())?;
    if end_value < start {
        return Err(diagnostic(
            path,
            line,
            source,
            "grapheme range end must not be before its start",
        ));
    }
    Ok((start, end_value, input[end..].trim_start()))
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

/// Parses a duration written as a decimal integer immediately followed by
/// `ms` (e.g. `100ms`), with no separating whitespace and no other unit --
/// the exact spelling `advance time` and `sleep real` use.
fn parse_millis_with_unit_suffix(
    path: &Path,
    line: usize,
    source: &str,
    input: &str,
) -> Result<u64, TestError> {
    let invalid = || {
        diagnostic(
            path,
            line,
            source,
            "duration must be a non-negative decimal integer immediately followed by ms, e.g. 100ms",
        )
    };
    let digits = input.strip_suffix("ms").ok_or_else(invalid)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    digits
        .parse()
        .map_err(|error| diagnostic(path, line, source, &format!("invalid duration: {error}")))
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
        "text" => {
            let value: String = serde_json::from_str(encoded).map_err(|error| {
                diagnostic(path, line, source, &format!("invalid JSON string: {error}"))
            })?;
            Ok(StateValue::Text(value))
        }
        // `bytes` is kept as a compatibility alias of `utf8-bytes`: despite
        // the name, it can only represent well-formed UTF-8 text encoded as
        // bytes, not arbitrary binary or invalid UTF-8. `bytes-hex` and
        // `bytes-base64` below can represent every value the typed state
        // API's `StateValue::Bytes(Vec<u8>)` actually supports.
        "bytes" | "utf8-bytes" => {
            let value: String = serde_json::from_str(encoded).map_err(|error| {
                diagnostic(path, line, source, &format!("invalid JSON string: {error}"))
            })?;
            Ok(StateValue::Bytes(value.into_bytes()))
        }
        "bytes-hex" => {
            let value: String = serde_json::from_str(encoded).map_err(|error| {
                diagnostic(path, line, source, &format!("invalid JSON string: {error}"))
            })?;
            let bytes = decode_hex(&value).map_err(|error| {
                diagnostic(
                    path,
                    line,
                    source,
                    &format!("invalid hex-encoded bytes: {error}"),
                )
            })?;
            Ok(StateValue::Bytes(bytes))
        }
        "bytes-base64" => {
            let value: String = serde_json::from_str(encoded).map_err(|error| {
                diagnostic(path, line, source, &format!("invalid JSON string: {error}"))
            })?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(value.as_bytes())
                .map_err(|error| {
                    diagnostic(
                        path,
                        line,
                        source,
                        &format!("invalid base64-encoded bytes: {error}"),
                    )
                })?;
            Ok(StateValue::Bytes(bytes))
        }
        _ => Err(diagnostic(
            path,
            line,
            source,
            "state kind must be boolean, integer, text, bytes (legacy alias of utf8-bytes), utf8-bytes, bytes-hex, or bytes-base64",
        )),
    }
}

/// Decodes ASCII hex digits into bytes, able to represent every byte
/// sequence including invalid UTF-8 -- unlike `bytes`/`utf8-bytes`, which
/// can only represent well-formed UTF-8 text.
fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.is_ascii() {
        return Err("hex-encoded bytes must be ASCII".into());
    }
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err("hex-encoded bytes must have an even number of digits".into());
    }
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char)
                .to_digit(16)
                .ok_or_else(|| format!("invalid hex digit {:?}", pair[0] as char))?;
            let low = (pair[1] as char)
                .to_digit(16)
                .ok_or_else(|| format!("invalid hex digit {:?}", pair[1] as char))?;
            Ok(((high << 4) | low) as u8)
        })
        .collect()
}

fn parse_key(
    path: &Path,
    line: usize,
    source: &str,
    value: &str,
) -> Result<(LogicalKey, Modifiers), TestError> {
    let shift_tab = value.starts_with("shift-tab");
    let (key, modifier_source) = if value.starts_with('"') {
        let (character, remainder) = parse_json_string_prefix(path, line, source, value)?;
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
        (LogicalKey::Character(character), remainder)
    } else if let Some((key, modifiers)) = value.split_once(' ') {
        (named_key(path, line, source, key)?, modifiers)
    } else {
        (named_key(path, line, source, value)?, "")
    };
    let mut modifiers = Modifiers {
        shift: shift_tab,
        ..Modifiers::default()
    };
    match modifier_source {
        "" => {}
        "+primary" => modifiers.control = true,
        _ => {
            return Err(diagnostic(
                path,
                line,
                source,
                "key modifiers must be exactly `+primary`",
            ));
        }
    }
    Ok((key, modifiers))
}

fn named_key(path: &Path, line: usize, source: &str, value: &str) -> Result<LogicalKey, TestError> {
    match value {
        "enter" => Ok(LogicalKey::Enter),
        "escape" => Ok(LogicalKey::Escape),
        "backspace" => Ok(LogicalKey::Backspace),
        "space" => Ok(LogicalKey::Space),
        "tab" | "shift-tab" => Ok(LogicalKey::Tab),
        "left" => Ok(LogicalKey::ArrowLeft),
        "right" => Ok(LogicalKey::ArrowRight),
        "up" => Ok(LogicalKey::ArrowUp),
        "down" => Ok(LogicalKey::ArrowDown),
        _ => Err(diagnostic(
            path,
            line,
            source,
            "key must be a supported named key or JSON string",
        )),
    }
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
    let workspace = tempfile::tempdir().map_err(|source| TestError::Io {
        path: std::env::temp_dir(),
        source,
    })?;
    seed_workspace(path, &script.commands, workspace.path())?;
    let granted_document = script
        .commands
        .iter()
        .find_map(|located| match &located.command {
            Command::GrantDocument { path } => Some(path.clone()),
            _ => None,
        });
    let uses_virtual_time = script
        .commands
        .iter()
        .any(|located| matches!(located.command, Command::AdvanceTime { .. }));
    let virtual_time = uses_virtual_time.then(VirtualTime::new);
    // Every run gets an isolated, deterministic clipboard test double --
    // headless tests must never read or write the real developer's OS
    // clipboard -- and it's what `paste` writes through.
    let clipboard = RecordingClipboardService::default();
    let mut app = spawn(
        component,
        app_id,
        &state_file,
        virtual_time.as_ref(),
        &clipboard,
        workspace.path(),
        granted_document.as_deref(),
    )?;
    let mut events = app.subscribe();
    let mut snapshot = None;
    // The live Editor session state as of the most recent host-local edit
    // against each node, for `expect editor text`/`expect editor
    // selection` -- host-local edits touch only the live Editor registry,
    // never the retained tree, so this is the only place that state is
    // observable from the runner.
    let mut editor_state: HashMap<NodeId, EditorLocalEditResult> = HashMap::new();
    // The lifetime `guest_call_count` observed at each active `measure
    // begin <label>`, for `measure expect <label> guest-turns <n>` to diff
    // against. Does not survive a `restart` -- the counter is per process
    // instance -- so it is cleared there, same as `editor_state`.
    let mut measure_baselines: HashMap<String, u64> = HashMap::new();
    let mut interaction = InteractionState::default();

    for located in script.commands {
        match &located.command {
            Command::State { .. } | Command::FixtureFile { .. } | Command::GrantDocument { .. } => {
            }
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
                let receipt = app
                    .activate(node)
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                if receipt.external_effect_writes > 0 {
                    await_save_completion(path, &located, &mut events).await?;
                }
                snapshot = Some(
                    app.snapshot()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
                reconcile(&mut interaction, snapshot.as_ref().unwrap());
            }
            Command::Click { selector } => {
                let tree = normalized_tree(snapshot.as_ref().expect("parser requires mount"));
                let node = selector.node_id();
                check_click_policy(path, &located, &tree, selector, node)?;
                interaction.focus_pointer_target(&tree, node);
                let receipt = app
                    .activate(node)
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                if receipt.external_effect_writes > 0 {
                    await_save_completion(path, &located, &mut events).await?;
                }
                snapshot = Some(
                    app.snapshot()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
                reconcile(&mut interaction, snapshot.as_ref().unwrap());
            }
            Command::TypeText { selector, text } => {
                let node = selector.node_id();
                let result = app
                    .edit_editor_locally(node, EditorLocalEdit::InsertText(text.clone()))
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                editor_state.insert(node, result);
                snapshot = Some(
                    app.snapshot()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
            }
            Command::ReplaceSelection { selector, text } => {
                let node = selector.node_id();
                let result = app
                    .edit_editor_locally(node, EditorLocalEdit::InsertText(text.clone()))
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                editor_state.insert(node, result);
            }
            Command::Paste { selector, text } => {
                let node = selector.node_id();
                clipboard.write_text(text).map_err(|error| {
                    assertion_error(
                        path,
                        &located,
                        format!("could not write the test clipboard: {error}"),
                    )
                })?;
                let result = app
                    .edit_editor_locally(node, EditorLocalEdit::Paste)
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                editor_state.insert(node, result);
            }
            Command::ComposeStart { selector, text }
            | Command::ComposeUpdate { selector, text } => {
                let node = selector.node_id();
                let result = app
                    .edit_editor_locally(
                        node,
                        EditorLocalEdit::ImeSetCompose {
                            text: text.clone(),
                            cursor: None,
                        },
                    )
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                editor_state.insert(node, result);
            }
            Command::ComposeCommit { selector, text } => {
                let node = selector.node_id();
                app.edit_editor_locally(
                    node,
                    EditorLocalEdit::ImeSetCompose {
                        text: text.clone(),
                        cursor: None,
                    },
                )
                .await
                .map_err(|error| runtime(path, &located, error))?;
                let result = app
                    .edit_editor_locally(node, EditorLocalEdit::ImeFinishCompose)
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                editor_state.insert(node, result);
            }
            Command::ComposeCancel { selector } => {
                let node = selector.node_id();
                let result = app
                    .edit_editor_locally(node, EditorLocalEdit::ImeClearCompose)
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                editor_state.insert(node, result);
            }
            Command::ExpectEditorText { selector, expected } => {
                let node = selector.node_id();
                // Local-edit results only carry metadata (content_changed,
                // changed_range, cursor/selection) since edits stopped
                // returning the whole buffer; the live text itself is
                // fetched fresh, on demand, exactly like a real app would
                // via `context.editor().snapshot()`.
                let observed = Some(
                    app.editor_snapshot(node)
                        .await
                        .map_err(|error| runtime(path, &located, error))?
                        .text,
                );
                if observed.as_deref() != Some(expected.as_str()) {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected editor text {selector} to equal {expected:?}; observed {}",
                            observed.as_deref().map_or_else(
                                || "no live Editor session or semantic node".to_owned(),
                                |value| format!("{value:?}")
                            )
                        ),
                    ));
                }
            }
            Command::ExpectEditorSelection {
                selector,
                start,
                end,
            } => {
                let node = selector.node_id();
                let Some((cursor, selection)) = editor_state
                    .get(&node)
                    .map(|result| (result.cursor, result.selection.clone()))
                else {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected editor selection for {selector}; observed no live Editor session (no host-local edit against it yet)"
                        ),
                    ));
                };
                let byte_range = selection.unwrap_or(cursor..cursor);
                let text = app
                    .editor_snapshot(node)
                    .await
                    .map_err(|error| runtime(path, &located, error))?
                    .text;
                let observed_start = byte_to_grapheme_index(&text, byte_range.start);
                let observed_end = byte_to_grapheme_index(&text, byte_range.end);
                if (observed_start, observed_end) != (*start, *end) {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected editor selection {selector} to be graphemes {start}..{end}; observed graphemes {observed_start}..{observed_end}"
                        ),
                    ));
                }
            }
            Command::ExternalWrite { path: file, bytes } => {
                let destination = workspace.path().join(file);
                fs::write(&destination, bytes).map_err(|source| TestError::Io {
                    path: destination,
                    source,
                })?;
            }
            Command::ExpectFile {
                path: file,
                expected,
            } => {
                let destination = workspace.path().join(file);
                let observed = fs::read(&destination).map_err(|source| TestError::Io {
                    path: destination.clone(),
                    source,
                })?;
                if &observed != expected {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected file {file:?} to contain {} bytes; observed {} bytes",
                            expected.len(),
                            observed.len()
                        ),
                    ));
                }
            }
            Command::ExpectEditorDirty { selector, expected } => {
                let observed = app
                    .editor_snapshot(selector.node_id())
                    .await
                    .map_err(|error| runtime(path, &located, error))?
                    .locally_dirty;
                if observed != *expected {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected editor dirty {selector} to be {expected}; observed {observed}"
                        ),
                    ));
                }
            }
            Command::MeasureBegin { label } => {
                let inspection = app
                    .inspect()
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                measure_baselines.insert(label.clone(), inspection.guest_call_count);
            }
            Command::MeasureExpectGuestTurns { label, expected } => {
                let Some(baseline) = measure_baselines.get(label) else {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected a measurement named {label:?}; observed no matching `measure begin` (or it was cleared by an intervening `restart`)"
                        ),
                    ));
                };
                let inspection = app
                    .inspect()
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                let Some(observed) = inspection.guest_call_count.checked_sub(*baseline) else {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "measurement {label:?}'s guest-call count went backwards since `measure begin`; a `measure` span cannot cross a `restart`"
                        ),
                    ));
                };
                if observed != *expected {
                    return Err(assertion_error(
                        path,
                        &located,
                        format!(
                            "expected measurement {label:?} to record {expected} guest turns; observed {observed}"
                        ),
                    ));
                }
            }
            Command::Sleep { millis } | Command::SleepReal { millis } => {
                tokio::time::sleep(std::time::Duration::from_millis(*millis)).await;
            }
            Command::AdvanceTime { millis } => {
                let virtual_time = virtual_time
                    .as_ref()
                    .expect("parser requires advance time to run in a virtual-time file");
                let duration = std::time::Duration::from_millis(*millis);
                virtual_time.clock.advance(duration);
                virtual_time.wake.advance(duration);
                let mut iterations = 0_u32;
                loop {
                    let due = virtual_time.wake.due();
                    if due.is_empty() {
                        break;
                    }
                    for token in due {
                        // `due()` just reported this token as armed and
                        // overdue, and only this loop mutates the driver,
                        // so `fire` returning `false` here would mean the
                        // seams disagree with themselves.
                        assert!(
                            virtual_time.wake.fire(&token),
                            "a token `due()` just reported was not fired"
                        );
                    }
                    // Barrier: the mailbox is a strict FIFO processed by one
                    // dedicated worker thread, so this round-trip guarantees
                    // every wake fired above -- and any host work it
                    // triggered, including a newly rearmed schedule -- is
                    // fully applied before the next `due()` check.
                    snapshot = Some(
                        app.snapshot()
                            .await
                            .map_err(|error| runtime(path, &located, error))?,
                    );
                    iterations += 1;
                    if iterations > 10_000 {
                        return Err(assertion_error(
                            path,
                            &located,
                            "advance time did not settle: a schedule kept rearming after 10,000 drain iterations".into(),
                        ));
                    }
                }
                reconcile(
                    &mut interaction,
                    snapshot.as_ref().expect("parser requires mount"),
                );
            }
            Command::Restart => {
                app.stop()
                    .await
                    .map_err(|error| runtime(path, &located, error))?;
                app = spawn(
                    component,
                    app_id,
                    &state_file,
                    virtual_time.as_ref(),
                    &clipboard,
                    workspace.path(),
                    granted_document.as_deref(),
                )?;
                events = app.subscribe();
                snapshot = Some(
                    app.mount()
                        .await
                        .map_err(|error| runtime(path, &located, error))?,
                );
                interaction = InteractionState::default();
                // A restart drops the whole runtime, including every live
                // Editor session; any cached local-edit result now
                // describes a session that no longer exists.
                editor_state.clear();
                // The new process instance's guest_call_count starts back
                // at zero; a baseline from before the restart is no longer
                // meaningful.
                measure_baselines.clear();
                reconcile(&mut interaction, snapshot.as_ref().unwrap());
                if options.verify_view_convergence {
                    verify_view_convergence(path, &located, &app).await?;
                }
            }
            Command::Key { key, modifiers } => {
                let tree = normalized_tree(snapshot.as_ref().expect("parser requires mount"));
                let change = interaction.key(&tree, key.clone(), *modifiers, false);
                if let Some(SemanticAction::Activate(node)) = change.action {
                    let receipt = app
                        .activate(node)
                        .await
                        .map_err(|error| runtime(path, &located, error))?;
                    if receipt.external_effect_writes > 0 {
                        await_save_completion(path, &located, &mut events).await?;
                    }
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
                    let expected = selector
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "none".to_owned());
                    return Err(TestError::Diagnostic {
                        path: path.to_path_buf(),
                        line: located.line,
                        command: located.source.clone(),
                        message: format!(
                            "expected focus {expected}; observed {:?}",
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
                        format!("expected {selector} to be present; observed no semantic node"),
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
                            "expected {selector} to be missing; observed {}",
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
        .take_while(|located| !matches!(located.command, Command::Mount))
        .filter(|located| matches!(located.command, Command::State { .. }))
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

fn seed_workspace(
    test_path: &Path,
    commands: &[LocatedCommand],
    workspace: &Path,
) -> Result<(), TestError> {
    for located in commands.iter().take_while(|located| {
        matches!(
            located.command,
            Command::State { .. } | Command::FixtureFile { .. } | Command::GrantDocument { .. }
        )
    }) {
        let Command::FixtureFile { path, bytes } = &located.command else {
            continue;
        };
        let destination = workspace.join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| TestError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&destination, bytes).map_err(|source| TestError::Io {
            path: destination,
            source,
        })?;
    }
    let _ = test_path;
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

async fn await_save_completion(
    path: &Path,
    located: &LocatedCommand,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
) -> Result<(), TestError> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::TurnCommitted(turn))
                    if matches!(
                        turn.origin,
                        youth_runtime::TurnOrigin::TextDocumentSaveCompleted { .. }
                    ) =>
                {
                    return Ok(());
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    return Err(assertion_error(
                        path,
                        located,
                        format!("runtime observer lagged by {count} events while awaiting save"),
                    ));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(assertion_error(
                        path,
                        located,
                        "runtime stopped before save completion".into(),
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| assertion_error(path, located, "timed out awaiting save completion".into()))?
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

/// The injected virtual `DeadlineClock` and `WakeDriver` for a file that
/// uses `advance time`. Held for the file's entire run, including across
/// `restart`, since state (and thus how much virtual time has already
/// elapsed) is meant to persist across a restart exactly like it would
/// against the production clock.
struct VirtualTime {
    clock: std::sync::Arc<youth_state::VirtualDeadlineClock>,
    wake: std::sync::Arc<youth_state::VirtualWakeDriver>,
}

impl VirtualTime {
    /// Seeds the virtual deadline clock with the real current time, purely
    /// so absolute epoch-millisecond values stay realistic (useful for
    /// debugging output); nothing depends on this exact starting value.
    fn new() -> Self {
        Self {
            clock: std::sync::Arc::new(youth_state::VirtualDeadlineClock::new(real_epoch_millis())),
            wake: std::sync::Arc::new(youth_state::VirtualWakeDriver::default()),
        }
    }
}

fn real_epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// Converts a byte offset into `text` to a grapheme-cluster index -- the
/// count of extended grapheme cluster boundaries (UAX #29) strictly before
/// it. `.youth-test`'s editor assertions are written in grapheme clusters,
/// not bytes, UTF-16 units, or Unicode scalar values, so this is the
/// conversion at the assertion layer that
/// `EditorLocalEditResult::{cursor,selection}` (byte offsets) need before
/// they can be compared against a DSL-level grapheme position. `byte_offset`
/// is assumed to already land on a grapheme boundary, true of every offset
/// the editor engine itself produces.
fn byte_to_grapheme_index(text: &str, byte_offset: usize) -> usize {
    text.grapheme_indices(true)
        .take_while(|(index, _)| *index < byte_offset)
        .count()
}

fn spawn(
    component: &Path,
    app_id: &AppId,
    state_file: &Path,
    virtual_time: Option<&VirtualTime>,
    clipboard: &RecordingClipboardService,
    workspace_root: &Path,
    granted_document: Option<&str>,
) -> Result<YouthAppHandle, TestError> {
    let mut limits = RuntimeLimits::default();
    if let Some(virtual_time) = virtual_time {
        limits.time.deadline_clock = virtual_time.clock.clone();
        limits.time.wake_driver = virtual_time.wake.clone();
    }
    limits.time.clipboard_service = std::sync::Arc::new(clipboard.clone());
    YouthAppHandle::spawn(YouthAppConfig {
        component_path: component.to_path_buf(),
        app_id: app_id.clone(),
        state: StateLocation::File(state_file.to_path_buf()),
        limits,
        workspace: granted_document
            .map(|document| youth_runtime::WorkspaceGrant::text_document(workspace_root, document)),
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

/// The semantic subset of real click policy: present, an activatable role
/// (a button), and enabled (accounting for ancestor `enabled` state, same
/// as `youth_interaction::InteractionState`'s focus/shortcut discovery).
/// Real headless hit-testing/geometry is not implemented yet.
fn check_click_policy(
    path: &Path,
    located: &LocatedCommand,
    tree: &Tree,
    selector: &Selector,
    node: NodeId,
) -> Result<(), TestError> {
    let Some(entry) = tree.node(node) else {
        return Err(assertion_error(
            path,
            located,
            format!("click target {selector} is not present in the semantic tree"),
        ));
    };
    if !entry.data.is_button() {
        return Err(assertion_error(
            path,
            located,
            format!(
                "click target {selector} has no activatable role; observed {}",
                describe(Some(&entry.data))
            ),
        ));
    }
    let probe = InteractionState::default();
    if !probe
        .snapshot(tree)
        .enabled_actions
        .contains(&SemanticAction::Activate(node))
    {
        return Err(assertion_error(
            path,
            located,
            format!("click target {selector} is present but disabled"),
        ));
    }
    Ok(())
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
                "expected text {selector} to equal {expected:?}; observed {}",
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
                "expected countdown {selector}; observed {}",
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
        Some(NodeData::Editor {
            document_revision,
            text,
        }) => {
            format!("editor(document_revision={document_revision}, text={text:?})")
        }
        Some(NodeData::TextDocumentEditor {
            document_id,
            document_generation,
            version_id,
            version_generation,
        }) => format!(
            "text-document editor(document={document_id}:{document_generation}, version={version_id}:{version_generation})"
        ),
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
    fn missing_header_defaults_to_format_version_one() {
        let script = parse(Path::new("legacy.youth-test"), "mount\n").unwrap();
        assert_eq!(script.version, 1);
    }

    #[test]
    fn explicit_header_is_parsed_and_excluded_from_commands() {
        let script = parse(Path::new("versioned.youth-test"), "youth-test 1\nmount\n").unwrap();
        assert_eq!(script.version, 1);
        assert_eq!(script.commands.len(), 1);
        assert_eq!(script.commands[0].command, Command::Mount);
    }

    #[test]
    fn header_may_follow_leading_comments_and_blank_lines() {
        let script = parse(
            Path::new("versioned.youth-test"),
            "# leading comment\n\nyouth-test 1\nmount\n",
        )
        .unwrap();
        assert_eq!(script.version, 1);
        assert_eq!(script.commands.len(), 1);
    }

    #[test]
    fn rejects_a_format_version_newer_than_this_runner_supports() {
        let error = parse(Path::new("future.youth-test"), "youth-test 2\nmount\n").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported .youth-test format version 2"),
            "{error}"
        );
    }

    #[test]
    fn rejects_a_malformed_format_version() {
        for source in ["youth-test\nmount\n", "youth-test abc\nmount\n"] {
            assert!(parse(Path::new("bad.youth-test"), source).is_err());
        }
    }

    #[test]
    fn header_is_only_recognized_as_the_first_content_line() {
        let error = parse(
            Path::new("bad.youth-test"),
            "mount\nyouth-test 1\nrestart\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown command"), "{error}");
    }

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
    fn invoke_and_activate_parse_to_the_same_command() {
        let script = parse(Path::new("invoke.youth-test"), "mount\ninvoke increment\n").unwrap();
        let alias = parse(
            Path::new("activate.youth-test"),
            "mount\nactivate increment\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::Activate {
                selector: Selector::Static("increment".into())
            }
        );
        assert_eq!(script.commands[1].command, alias.commands[1].command);
    }

    #[test]
    fn click_parses_to_its_own_command() {
        let script = parse(Path::new("click.youth-test"), "mount\nclick increment\n").unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::Click {
                selector: Selector::Static("increment".into())
            }
        );
    }

    #[test]
    fn click_policy_rejects_absent_disabled_and_non_button_targets() {
        let id = |value| NodeId::new(value).unwrap();
        let tree = Tree::from_snapshot(
            TreeSnapshot {
                revision: 0,
                root: id(1),
                nodes: vec![
                    youth_tree::Node {
                        id: id(1),
                        data: NodeData::Root,
                        grow: 0,
                        children: vec![id(2), id(3), id(4)],
                    },
                    youth_tree::Node {
                        id: id(2),
                        data: NodeData::Button {
                            label: "Go".into(),
                            enabled: true,
                        },
                        grow: 0,
                        children: vec![],
                    },
                    youth_tree::Node {
                        id: id(3),
                        data: NodeData::Button {
                            label: "Disabled".into(),
                            enabled: false,
                        },
                        grow: 0,
                        children: vec![],
                    },
                    youth_tree::Node {
                        id: id(4),
                        data: NodeData::Text {
                            value: "not a button".into(),
                        },
                        grow: 0,
                        children: vec![],
                    },
                ],
            },
            &youth_tree::Limits::default(),
        )
        .unwrap();
        let located = LocatedCommand {
            line: 1,
            source: "click go".into(),
            command: Command::Click {
                selector: Selector::Static("go".into()),
            },
        };
        let path = Path::new("click.youth-test");

        check_click_policy(path, &located, &tree, &Selector::Static("go".into()), id(2))
            .expect("enabled button target is clickable");

        let disabled = check_click_policy(
            path,
            &located,
            &tree,
            &Selector::Static("disabled".into()),
            id(3),
        )
        .unwrap_err();
        assert!(
            disabled.to_string().contains("present but disabled"),
            "{disabled}"
        );

        let non_button = check_click_policy(
            path,
            &located,
            &tree,
            &Selector::Static("label".into()),
            id(4),
        )
        .unwrap_err();
        assert!(
            non_button.to_string().contains("no activatable role"),
            "{non_button}"
        );

        let absent = check_click_policy(
            path,
            &located,
            &tree,
            &Selector::Static("missing".into()),
            id(99),
        )
        .unwrap_err();
        assert!(absent.to_string().contains("is not present"), "{absent}");
    }

    #[test]
    fn quoted_selectors_accept_whitespace_hashes_and_non_ascii() {
        let script = parse(
            Path::new("quoted.youth-test"),
            "mount\nexpect present \"sidebar/current note\"\nexpect present \"\u{6587}\u{66f8}/\u{73fe}\u{5728} # not a comment\"\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::ExpectPresent {
                selector: Selector::Static("sidebar/current note".into())
            }
        );
        assert_eq!(
            script.commands[2].command,
            Command::ExpectPresent {
                selector: Selector::Static(
                    "\u{6587}\u{66f8}/\u{73fe}\u{5728} # not a comment".into()
                )
            }
        );
    }

    #[test]
    fn quoted_selector_named_none_is_not_the_no_focus_sentinel() {
        let script = parse(
            Path::new("quoted-focus.youth-test"),
            "mount\nexpect focus \"none\"\nexpect focus none\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::ExpectFocus {
                selector: Some(Selector::Static("none".into()))
            }
        );
        assert_eq!(
            script.commands[2].command,
            Command::ExpectFocus { selector: None }
        );
    }

    #[test]
    fn parses_extended_bytes_seed_encodings() {
        let script = parse(
            Path::new("bytes.youth-test"),
            r#"
state utf8-bytes "text" "héllo"
state bytes-hex "binary" "00ff7f80"
state bytes-base64 "based" "AP9/gA=="
mount
"#,
        )
        .unwrap();
        assert_eq!(
            script.commands[0].command,
            Command::State {
                key: "text".into(),
                value: StateValue::Bytes("héllo".as_bytes().to_vec()),
            }
        );
        assert_eq!(
            script.commands[1].command,
            Command::State {
                key: "binary".into(),
                value: StateValue::Bytes(vec![0x00, 0xff, 0x7f, 0x80]),
            }
        );
        assert_eq!(
            script.commands[2].command,
            Command::State {
                key: "based".into(),
                value: StateValue::Bytes(vec![0x00, 0xff, 0x7f, 0x80]),
            }
        );
    }

    #[test]
    fn rejects_malformed_hex_and_base64_seeds() {
        for source in [
            "state bytes-hex \"k\" \"0ff\"\nmount\n",
            "state bytes-hex \"k\" \"zz\"\nmount\n",
            "state bytes-base64 \"k\" \"not base64!!\"\nmount\n",
        ] {
            assert!(parse(Path::new("bad.youth-test"), source).is_err());
        }
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
    fn parses_type_replace_selection_and_paste() {
        let script = parse(
            Path::new("editor.youth-test"),
            "mount\ntype document \"Hello, 世界\"\nreplace-selection document \"Hi\"\npaste document \"clipped\"\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::TypeText {
                selector: Selector::Static("document".into()),
                text: "Hello, 世界".into(),
            }
        );
        assert_eq!(
            script.commands[2].command,
            Command::ReplaceSelection {
                selector: Selector::Static("document".into()),
                text: "Hi".into(),
            }
        );
        assert_eq!(
            script.commands[3].command,
            Command::Paste {
                selector: Selector::Static("document".into()),
                text: "clipped".into(),
            }
        );
    }

    #[test]
    fn parses_the_compose_family() {
        let script = parse(
            Path::new("compose.youth-test"),
            "mount\ncompose document start \"n\"\ncompose document update \"\u{f1}\"\ncompose document commit \"\u{f1}\"\ncompose document cancel\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::ComposeStart {
                selector: Selector::Static("document".into()),
                text: "n".into(),
            }
        );
        assert_eq!(
            script.commands[2].command,
            Command::ComposeUpdate {
                selector: Selector::Static("document".into()),
                text: "\u{f1}".into(),
            }
        );
        assert_eq!(
            script.commands[3].command,
            Command::ComposeCommit {
                selector: Selector::Static("document".into()),
                text: "\u{f1}".into(),
            }
        );
        assert_eq!(
            script.commands[4].command,
            Command::ComposeCancel {
                selector: Selector::Static("document".into()),
            }
        );
    }

    #[test]
    fn rejects_a_malformed_compose_verb() {
        let error = parse(
            Path::new("bad.youth-test"),
            "mount\ncompose document nonsense\n",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected: compose <selector> start|update|commit"),
            "{error}"
        );
    }

    #[test]
    fn parses_expect_editor_text_and_selection() {
        let script = parse(
            Path::new("editor-expect.youth-test"),
            "mount\nexpect editor text document \"Hi, 世界\"\nexpect editor selection document graphemes 2..2\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::ExpectEditorText {
                selector: Selector::Static("document".into()),
                expected: "Hi, 世界".into(),
            }
        );
        assert_eq!(
            script.commands[2].command,
            Command::ExpectEditorSelection {
                selector: Selector::Static("document".into()),
                start: 2,
                end: 2,
            }
        );
    }

    #[test]
    fn rejects_a_malformed_grapheme_range() {
        for source in [
            "mount\nexpect editor selection document 2..2\n",
            "mount\nexpect editor selection document graphemes 2\n",
            "mount\nexpect editor selection document graphemes two..three\n",
            "mount\nexpect editor selection document graphemes 5..2\n",
        ] {
            assert!(parse(Path::new("bad.youth-test"), source).is_err());
        }
    }

    #[test]
    fn parses_measure_begin_and_expect_guest_turns() {
        let script = parse(
            Path::new("measure.youth-test"),
            "mount\nmeasure begin \"typing\"\nmeasure expect \"typing\" guest-turns 0\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::MeasureBegin {
                label: "typing".into()
            }
        );
        assert_eq!(
            script.commands[2].command,
            Command::MeasureExpectGuestTurns {
                label: "typing".into(),
                expected: 0,
            }
        );
    }

    #[test]
    fn rejects_a_measure_expect_naming_an_unsupported_counter() {
        let error = parse(
            Path::new("bad.youth-test"),
            "mount\nmeasure expect \"typing\" state-writes 0\n",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("the only measure counter implemented today"),
            "{error}"
        );
    }

    #[test]
    fn byte_to_grapheme_index_counts_extended_grapheme_clusters_not_bytes_or_scalars() {
        // "Hi, 世界" -- "世" and "界" are each one grapheme cluster but
        // three UTF-8 bytes; a naive byte-offset comparison would be wrong.
        let text = "Hi, 世界";
        assert_eq!(byte_to_grapheme_index(text, 0), 0);
        assert_eq!(byte_to_grapheme_index(text, 4), 4); // just after "Hi, "
        assert_eq!(byte_to_grapheme_index(text, 7), 5); // just after "世" (3 bytes)
        assert_eq!(byte_to_grapheme_index(text, 10), 6); // end of string
    }

    #[test]
    fn parses_advance_time_and_sleep_real_and_wall_sleep() {
        let script = parse(
            Path::new("advance.youth-test"),
            "mount\nadvance time 100ms\nsleep real 150ms\nwall-sleep 25ms\n",
        )
        .unwrap();
        assert_eq!(
            script.commands[1].command,
            Command::AdvanceTime { millis: 100 }
        );
        assert_eq!(
            script.commands[2].command,
            Command::SleepReal { millis: 150 }
        );
        assert_eq!(
            script.commands[3].command,
            Command::SleepReal { millis: 25 }
        );
    }

    #[test]
    fn rejects_advance_time_and_sleep_durations_missing_the_ms_suffix() {
        for source in [
            "mount\nadvance time 100\n",
            "mount\nsleep real 100\n",
            "mount\nadvance time 100s\n",
            "mount\nadvance time ms\n",
        ] {
            let error = parse(Path::new("bad.youth-test"), source).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("immediately followed by ms, e.g. 100ms"),
                "{error}"
            );
        }
    }

    #[test]
    fn rejects_bare_sleep_mixed_with_advance_time_in_the_same_file() {
        let error = parse(
            Path::new("bad.youth-test"),
            "mount\nadvance time 100ms\nsleep 50\n",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("also uses `advance time`"),
            "{error}"
        );
        // Order does not matter -- the bare `sleep` still runs against the
        // production clock either way.
        let error = parse(
            Path::new("bad.youth-test"),
            "mount\nsleep 50\nadvance time 100ms\n",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("also uses `advance time`"),
            "{error}"
        );
        // `sleep real`/`wall-sleep` remain fine alongside `advance time`.
        parse(
            Path::new("ok.youth-test"),
            "mount\nadvance time 100ms\nsleep real 50ms\nwall-sleep 50ms\n",
        )
        .unwrap();
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
                    grow: 0,
                    children: vec![id(2)],
                },
                youth_tree::Node {
                    id: id(2),
                    data: NodeData::Text {
                        value: "old".into(),
                    },
                    grow: 0,
                    children: vec![],
                },
                youth_tree::Node {
                    id: id(3),
                    data: NodeData::Text {
                        value: "missing".into(),
                    },
                    grow: 0,
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
                    grow: 0,
                    children: vec![id(2)],
                },
                youth_tree::Node {
                    id: id(2),
                    data: NodeData::Text {
                        value: "new".into(),
                    },
                    grow: 0,
                    children: vec![],
                },
                youth_tree::Node {
                    id: id(4),
                    data: NodeData::Text {
                        value: "extra".into(),
                    },
                    grow: 0,
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
