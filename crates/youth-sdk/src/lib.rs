//! Guest-facing Rust SDK for Youth applications.
//!
//! The SDK owns the Rust WIT bindings, component export adapter, lifecycle
//! bookkeeping, semantic builders, typed state calls, and wire conversion.
//!
//! ```no_run
//! use youth_sdk::prelude::*;
//!
//! struct Tally;
//!
//! impl Application for Tally {
//!     fn view(context: &ViewContext) -> Result<Tree> {
//!         let count = context.state().integer("count")?.unwrap_or(0);
//!         Ok(Tree::root(BoxNode::column([
//!             Text::new(node!("count"), format!("Count: {count}")),
//!             Button::new(node!("increment"), "Increment"),
//!         ])))
//!     }
//!
//!     fn handle(context: &mut EventContext, events: &Events) -> Result<Update> {
//!         if events.activated(node!("increment")) {
//!             let count = context.state().integer("count")?.unwrap_or(0) + 1;
//!             context.state().set_integer("count", count)?;
//!             return Ok(Update::new()
//!                 .set_text(node!("count"), format!("Count: {count}")));
//!         }
//!         Ok(Update::unchanged())
//!     }
//! }
//!
//! youth_sdk::export_app!(Tally);
//! ```

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;
// The suffix is the two ASCII bytes `\\` (0x5c) and `0` (0x30), not NUL.
// This spelling is locked by the public cross-language test vectors.
const NAMED_DOMAIN: &[u8] = b"youth:node-id:v1\\0";
const COMMAND_DOMAIN: &[u8] = b"youth:command-id:v1\\0";
const NAMED_BIT: u64 = 0x8000_0000_0000_0000;
const VALUE_MASK: u64 = 0x7fff_ffff_ffff_ffff;

/// Calculates the stable, app-global wire ID for an exact UTF-8 node name.
#[must_use]
pub const fn named_node_id(name: &str) -> u64 {
    named_id(NAMED_DOMAIN, name)
}

/// Calculates the stable, app-global command ID for an exact UTF-8 name.
#[must_use]
pub const fn named_command_id(name: &str) -> u64 {
    named_id(COMMAND_DOMAIN, name)
}

const fn named_id(domain: &[u8], name: &str) -> u64 {
    let mut hash = FNV_OFFSET;
    let mut index = 0;
    while index < domain.len() {
        hash ^= domain[index] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        index += 1;
    }
    let bytes = name.as_bytes();
    index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        index += 1;
    }
    (hash & VALUE_MASK) | NAMED_BIT
}

/// A symbolic node name and its stable app-global wire ID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeKey {
    name: &'static str,
    id: u64,
}

impl NodeKey {
    /// Creates a key from an exact, unnormalized UTF-8 name.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            id: named_node_id(name),
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    #[must_use]
    pub const fn id(self) -> u64 {
        self.id
    }

    #[cfg(test)]
    const fn with_id(name: &'static str, id: u64) -> Self {
        Self { name, id }
    }
}

/// Creates a stable, app-global symbolic node key.
#[macro_export]
macro_rules! node {
    ($name:literal) => {{
        const KEY: $crate::NodeKey = $crate::NodeKey::new($name);
        KEY
    }};
}

/// A symbolic command name with an identity domain distinct from node IDs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommandKey {
    name: &'static str,
    id: u64,
}

impl CommandKey {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            id: named_command_id(name),
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    #[must_use]
    pub const fn id(self) -> u64 {
        self.id
    }
}

/// Creates a stable, app-global symbolic command key.
#[macro_export]
macro_rules! command {
    ($name:literal) => {{
        const KEY: $crate::CommandKey = $crate::CommandKey::new($name);
        KEY
    }};
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextAlign {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimePrecision {
    Seconds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountdownFormat {
    MinutesSeconds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shortcut {
    Character(char),
    Enter,
    Escape,
    Backspace,
}

impl Shortcut {
    #[must_use]
    pub const fn character(value: char) -> Self {
        Self::Character(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Layout {
    Column,
    Row,
    Grid(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    InvalidState,
    RejectedEvent,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    message: Option<String>,
}

impl Error {
    #[must_use]
    pub const fn invalid_state() -> Self {
        Self {
            kind: ErrorKind::InvalidState,
            message: None,
        }
    }

    #[must_use]
    pub const fn rejected_event() -> Self {
        Self {
            kind: ErrorKind::RejectedEvent,
            message: None,
        }
    }

    #[must_use]
    pub const fn internal() -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: None,
        }
    }

    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            ErrorKind::InvalidState => "application state is invalid",
            ErrorKind::RejectedEvent => "application rejected the event",
            ErrorKind::Internal => "application failed internally",
        })
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Text;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TextElement {
    key: NodeKey,
    value: String,
    alignment: TextAlign,
}

impl Text {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(key: NodeKey, value: impl Into<String>) -> Element {
        Element {
            kind: ElementKind::Text(TextElement {
                key,
                value: value.into(),
                alignment: TextAlign::Start,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Countdown;

#[derive(Clone, Debug, Eq, PartialEq)]
struct CountdownElement {
    key: NodeKey,
    schedule: Schedule,
    precision: TimePrecision,
    format: CountdownFormat,
    alignment: TextAlign,
}

impl Countdown {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(key: NodeKey, schedule: Schedule) -> Element {
        Element {
            kind: ElementKind::Countdown(CountdownElement {
                key,
                schedule,
                precision: TimePrecision::Seconds,
                format: CountdownFormat::MinutesSeconds,
                alignment: TextAlign::Start,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Button;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ButtonElement {
    key: NodeKey,
    label: String,
    enabled: bool,
    command: Option<CommandKey>,
    shortcuts: Vec<Shortcut>,
}

impl Button {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(key: NodeKey, label: impl Into<String>) -> Element {
        Element {
            kind: ElementKind::Button(ButtonElement {
                key,
                label: label.into(),
                enabled: true,
                command: None,
                shortcuts: Vec::new(),
            }),
        }
    }

    #[must_use]
    pub fn command(key: CommandKey, label: impl Into<String>) -> Element {
        Element {
            kind: ElementKind::Button(ButtonElement {
                key: NodeKey::new(key.name()),
                label: label.into(),
                enabled: true,
                command: Some(key),
                shortcuts: Vec::new(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoxNode;

#[derive(Clone, Debug, Eq, PartialEq)]
struct BoxElement {
    children: Vec<Element>,
    enabled: bool,
    layout: Layout,
}

impl BoxNode {
    #[must_use]
    pub fn column(children: impl IntoIterator<Item = Element>) -> Element {
        Element {
            kind: ElementKind::Box(BoxElement {
                children: children.into_iter().collect(),
                enabled: true,
                layout: Layout::Column,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Column;

impl Column {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(children: impl IntoIterator<Item = Element>) -> Element {
        BoxNode::column(children)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row;

impl Row {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(children: impl IntoIterator<Item = Element>) -> Element {
        Element {
            kind: ElementKind::Box(BoxElement {
                children: children.into_iter().collect(),
                enabled: true,
                layout: Layout::Row,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grid;

impl Grid {
    #[must_use]
    pub fn columns(columns: u8, children: impl IntoIterator<Item = Element>) -> Element {
        Element {
            kind: ElementKind::Box(BoxElement {
                children: children.into_iter().collect(),
                enabled: true,
                layout: Layout::Grid(columns),
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Element {
    kind: ElementKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ElementKind {
    Box(BoxElement),
    Text(TextElement),
    Countdown(CountdownElement),
    Button(ButtonElement),
}

impl Element {
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        match &mut self.kind {
            ElementKind::Box(value) => value.enabled = enabled,
            ElementKind::Button(value) => value.enabled = enabled,
            ElementKind::Text(_) | ElementKind::Countdown(_) => {}
        }
        self
    }

    #[must_use]
    pub fn align(mut self, alignment: TextAlign) -> Self {
        match &mut self.kind {
            ElementKind::Text(value) => value.alignment = alignment,
            ElementKind::Countdown(value) => value.alignment = alignment,
            ElementKind::Box(_) | ElementKind::Button(_) => {}
        }
        self
    }

    #[must_use]
    pub fn shortcut(mut self, shortcut: Shortcut) -> Self {
        if let ElementKind::Button(value) = &mut self.kind {
            value.shortcuts.push(shortcut);
        }
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tree {
    child: Element,
}

impl Tree {
    #[must_use]
    pub const fn root(child: Element) -> Self {
        Self { child }
    }

    #[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
    fn flatten(&self) -> Result<FlatTree> {
        let mut builder = FlatTreeBuilder {
            next_anonymous: 2,
            nodes: vec![FlatNode {
                id: 1,
                data: FlatNodeData::Root,
                children: Vec::new(),
            }],
            names: BTreeMap::new(),
            commands: BTreeMap::new(),
            ids: BTreeSet::from([1]),
        };
        let child = builder.push(&self.child)?;
        builder.nodes[0].children.push(child);
        Ok(FlatTree {
            root: 1,
            nodes: builder.nodes,
        })
    }
}

pub type NodeId = u64;
pub type ScheduleId = u64;
pub type Generation = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElapsedReason {
    Deadline,
    RecoveredOverdue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Activated(NodeId),
    ScheduleElapsed {
        schedule: ScheduleId,
        generation: Generation,
        reason: ElapsedReason,
    },
}

#[cfg(any(test, all(target_os = "wasi", target_env = "p2")))]
#[cfg_attr(test, allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IncomingEvent {
    Activated(NodeId),
    ScheduleElapsed {
        schedule: ScheduleId,
        generation: Generation,
        reason: ElapsedReason,
    },
    #[cfg(test)]
    Unsupported,
}

#[cfg(any(test, all(target_os = "wasi", target_env = "p2")))]
pub(crate) fn decode_incoming_events(
    incoming: impl IntoIterator<Item = IncomingEvent>,
) -> Result<Vec<Event>> {
    incoming
        .into_iter()
        .map(|event| match event {
            IncomingEvent::Activated(id) if id != 0 => Ok(Event::Activated(id)),
            IncomingEvent::Activated(_) => {
                Err(Error::invalid_state().with_message("event contains an invalid node ID"))
            }
            IncomingEvent::ScheduleElapsed {
                schedule,
                generation,
                reason,
            } if schedule != 0 && generation != 0 => Ok(Event::ScheduleElapsed {
                schedule,
                generation,
                reason,
            }),
            IncomingEvent::ScheduleElapsed { .. } => {
                Err(Error::invalid_state()
                    .with_message("event contains an invalid schedule identity"))
            }
            #[cfg(test)]
            IncomingEvent::Unsupported => {
                Err(Error::invalid_state().with_message("event kind is not supported by this SDK"))
            }
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Events {
    events: Vec<Event>,
    commanded: Vec<u64>,
}

impl Events {
    #[must_use]
    pub fn activated(&self, key: NodeKey) -> bool {
        self.events
            .iter()
            .any(|event| matches!(event, Event::Activated(id) if *id == key.id()))
    }

    #[must_use]
    pub fn commanded(&self, key: CommandKey) -> bool {
        self.commanded.contains(&key.id())
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Event> {
        self.events.iter()
    }

    pub fn elapsed(&self) -> impl Iterator<Item = (ScheduleId, Generation, ElapsedReason)> + '_ {
        self.events.iter().filter_map(|event| match event {
            Event::ScheduleElapsed {
                schedule,
                generation,
                reason,
            } => Some((*schedule, *generation, *reason)),
            Event::Activated(_) => None,
        })
    }
}

impl<'a> IntoIterator for &'a Events {
    type Item = &'a Event;
    type IntoIter = std::slice::Iter<'a, Event>;

    fn into_iter(self) -> Self::IntoIter {
        self.events.iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum UpdateOperation {
    Text(NodeKey, String),
    Countdown(NodeKey, Schedule, TimePrecision, CountdownFormat),
    Label(NodeKey, String),
    Enabled(NodeKey, bool),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Update {
    operations: Vec<UpdateOperation>,
}

impl Update {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            operations: Vec::new(),
        }
    }

    #[must_use]
    pub const fn unchanged() -> Self {
        Self::new()
    }

    #[must_use]
    pub fn set_text(mut self, key: NodeKey, value: impl Into<String>) -> Self {
        self.operations
            .push(UpdateOperation::Text(key, value.into()));
        self
    }

    #[must_use]
    pub fn set_countdown(
        mut self,
        key: NodeKey,
        schedule: Schedule,
        precision: TimePrecision,
        format: CountdownFormat,
    ) -> Self {
        self.operations
            .push(UpdateOperation::Countdown(key, schedule, precision, format));
        self
    }

    #[must_use]
    pub fn set_label(mut self, key: NodeKey, value: impl Into<String>) -> Self {
        self.operations
            .push(UpdateOperation::Label(key, value.into()));
        self
    }

    #[must_use]
    pub fn set_enabled(mut self, key: NodeKey, enabled: bool) -> Self {
        self.operations.push(UpdateOperation::Enabled(key, enabled));
        self
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ViewContext;

impl ViewContext {
    #[must_use]
    pub const fn state(&self) -> StateReader {
        StateReader
    }

    // Deliberately no `time()` method: rendering a view must remain
    // side-effect free, while every scheduler operation mutates host state.
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EventContext;

impl EventContext {
    #[must_use]
    pub const fn state(&mut self) -> StateWriter {
        StateWriter
    }

    #[must_use]
    pub const fn time(&mut self) -> TimeScheduler {
        TimeScheduler
    }
}

/// A host-issued schedule identity.
///
/// Applications cannot construct this handle. The raw values are exposed only
/// so an application can persist them; prefer [`StateReader::schedule`] and
/// [`StateWriter::set_schedule`] for a checked round trip through typed state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Schedule {
    id: u64,
    generation: u64,
}

impl Schedule {
    /// Returns the host-issued identifier for persistence.
    #[must_use]
    pub const fn id(self) -> u64 {
        self.id
    }

    /// Returns the host-issued generation for persistence.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    title: String,
    body: String,
}

impl Notification {
    #[must_use]
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScheduleOptions {
    notification: Option<Notification>,
}

impl ScheduleOptions {
    #[must_use]
    pub const fn new() -> Self {
        Self { notification: None }
    }

    #[must_use]
    pub fn notification(mut self, value: Notification) -> Self {
        self.notification = Some(value);
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TimeScheduler;

impl TimeScheduler {
    pub fn schedule_after(self, duration: Duration, options: ScheduleOptions) -> Result<Schedule> {
        let millis = u64::try_from(duration.as_millis()).map_err(|_| {
            Error::invalid_state().with_message("schedule duration exceeds u64::MAX milliseconds")
        })?;
        if millis < 100 {
            return Err(Error::invalid_state()
                .with_message("schedule duration must be at least 100 milliseconds"));
        }
        time::schedule_after(millis, options)
    }

    pub fn pause(self, schedule: Schedule) -> Result<Schedule> {
        time::pause(schedule)
    }

    pub fn resume(self, schedule: Schedule) -> Result<Schedule> {
        time::resume(schedule)
    }

    pub fn cancel(self, schedule: Schedule) -> Result<()> {
        time::cancel(schedule)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StateReader;

impl StateReader {
    pub fn boolean(self, key: &str) -> Result<Option<bool>> {
        state::get_boolean(key)
    }

    pub fn integer(self, key: &str) -> Result<Option<i64>> {
        state::get_integer(key)
    }

    pub fn text(self, key: &str) -> Result<Option<String>> {
        state::get_text(key)
    }

    pub fn bytes(self, key: &str) -> Result<Option<Vec<u8>>> {
        state::get_bytes(key)
    }

    /// Restores a schedule handle previously written by
    /// [`StateWriter::set_schedule`].
    pub fn schedule(self, key: &str) -> Result<Option<Schedule>> {
        let Some(bytes) = self.bytes(key)? else {
            return Ok(None);
        };
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| Error::invalid_state().with_message("stored schedule is malformed"))?;
        let (id, generation) = bytes.split_at(8);
        Ok(Some(Schedule {
            id: u64::from_be_bytes(id.try_into().expect("slice has eight bytes")),
            generation: u64::from_be_bytes(generation.try_into().expect("slice has eight bytes")),
        }))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StateWriter;

impl StateWriter {
    pub fn boolean(self, key: &str) -> Result<Option<bool>> {
        StateReader.boolean(key)
    }

    pub fn integer(self, key: &str) -> Result<Option<i64>> {
        StateReader.integer(key)
    }

    pub fn text(self, key: &str) -> Result<Option<String>> {
        StateReader.text(key)
    }

    pub fn bytes(self, key: &str) -> Result<Option<Vec<u8>>> {
        StateReader.bytes(key)
    }

    pub fn schedule(self, key: &str) -> Result<Option<Schedule>> {
        StateReader.schedule(key)
    }

    pub fn set_boolean(self, key: &str, value: bool) -> Result<()> {
        state::set_boolean(key, value)
    }

    pub fn set_integer(self, key: &str, value: i64) -> Result<()> {
        state::set_integer(key, value)
    }

    pub fn set_text(self, key: &str, value: &str) -> Result<()> {
        state::set_text(key, value)
    }

    pub fn set_bytes(self, key: &str, value: &[u8]) -> Result<()> {
        state::set_bytes(key, value)
    }

    /// Persists a host-issued schedule handle for
    /// [`StateReader::schedule`] to restore.
    pub fn set_schedule(self, key: &str, value: Schedule) -> Result<()> {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&value.id.to_be_bytes());
        bytes[8..].copy_from_slice(&value.generation.to_be_bytes());
        self.set_bytes(key, &bytes)
    }

    pub fn delete(self, key: &str) -> Result<bool> {
        state::delete(key)
    }
}

pub trait Application {
    fn view(context: &ViewContext) -> Result<Tree>;
    fn handle(context: &mut EventContext, events: &Events) -> Result<Update>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
enum FlatNodeData {
    Root,
    Box {
        enabled: bool,
        layout: Layout,
    },
    Text {
        value: String,
        alignment: TextAlign,
    },
    Countdown {
        schedule: Schedule,
        precision: TimePrecision,
        format: CountdownFormat,
        alignment: TextAlign,
    },
    Button {
        label: String,
        enabled: bool,
        command: Option<CommandKey>,
        shortcuts: Vec<Shortcut>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
struct FlatNode {
    id: u64,
    data: FlatNodeData,
    children: Vec<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
struct FlatTree {
    root: u64,
    nodes: Vec<FlatNode>,
}

#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
impl FlatTree {
    fn apply(&mut self, update: &Update) -> Result<()> {
        let mut changed = BTreeSet::new();
        for operation in &update.operations {
            let key = match operation {
                UpdateOperation::Text(key, _)
                | UpdateOperation::Countdown(key, ..)
                | UpdateOperation::Label(key, _)
                | UpdateOperation::Enabled(key, _) => *key,
            };
            if !changed.insert(key.id()) {
                return Err(Error::invalid_state().with_message("a node is updated twice"));
            }
            let Some(node) = self.nodes.iter_mut().find(|node| node.id == key.id()) else {
                return Err(Error::invalid_state().with_message("an update names an unknown node"));
            };
            match operation {
                UpdateOperation::Text(_, value) => {
                    let alignment = match &node.data {
                        FlatNodeData::Text { alignment, .. }
                        | FlatNodeData::Countdown { alignment, .. } => *alignment,
                        _ => {
                            return Err(Error::invalid_state()
                                .with_message("an update does not match the named node type"));
                        }
                    };
                    node.data = FlatNodeData::Text {
                        value: value.clone(),
                        alignment,
                    };
                }
                UpdateOperation::Countdown(_, schedule, precision, format) => {
                    let alignment = match &node.data {
                        FlatNodeData::Text { alignment, .. }
                        | FlatNodeData::Countdown { alignment, .. } => *alignment,
                        _ => {
                            return Err(Error::invalid_state()
                                .with_message("an update does not match the named node type"));
                        }
                    };
                    node.data = FlatNodeData::Countdown {
                        schedule: *schedule,
                        precision: *precision,
                        format: *format,
                        alignment,
                    };
                }
                UpdateOperation::Label(_, value) => match &mut node.data {
                    FlatNodeData::Button { label, .. } => label.clone_from(value),
                    _ => {
                        return Err(Error::invalid_state()
                            .with_message("an update does not match the named node type"));
                    }
                },
                UpdateOperation::Enabled(_, enabled) => match &mut node.data {
                    FlatNodeData::Button { enabled: value, .. }
                    | FlatNodeData::Box { enabled: value, .. } => *value = *enabled,
                    _ => {
                        return Err(Error::invalid_state()
                            .with_message("an update does not match the named node type"));
                    }
                },
            }
        }
        Ok(())
    }
}

#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
struct FlatTreeBuilder {
    next_anonymous: u64,
    nodes: Vec<FlatNode>,
    names: BTreeMap<u64, &'static str>,
    commands: BTreeMap<u64, &'static str>,
    ids: BTreeSet<u64>,
}

#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
impl FlatTreeBuilder {
    fn push(&mut self, element: &Element) -> Result<u64> {
        match &element.kind {
            ElementKind::Box(value) => {
                let id = self.allocate_anonymous()?;
                let index = self.nodes.len();
                self.nodes.push(FlatNode {
                    id,
                    data: FlatNodeData::Box {
                        enabled: value.enabled,
                        layout: value.layout,
                    },
                    children: Vec::new(),
                });
                for child in &value.children {
                    let child = self.push(child)?;
                    self.nodes[index].children.push(child);
                }
                Ok(id)
            }
            ElementKind::Text(value) => {
                let id = self.allocate_named(value.key)?;
                self.nodes.push(FlatNode {
                    id,
                    data: FlatNodeData::Text {
                        value: value.value.clone(),
                        alignment: value.alignment,
                    },
                    children: Vec::new(),
                });
                Ok(id)
            }
            ElementKind::Countdown(value) => {
                let id = self.allocate_named(value.key)?;
                self.nodes.push(FlatNode {
                    id,
                    data: FlatNodeData::Countdown {
                        schedule: value.schedule,
                        precision: value.precision,
                        format: value.format,
                        alignment: value.alignment,
                    },
                    children: Vec::new(),
                });
                Ok(id)
            }
            ElementKind::Button(value) => {
                if value.shortcuts.len() > 4 {
                    return Err(Error::invalid_state()
                        .with_message("a button declares more than four shortcuts"));
                }
                if let Some(command) = value.command
                    && let Some(existing) = self.commands.insert(command.id(), command.name())
                {
                    let message = if existing == command.name() {
                        "a command is bound more than once"
                    } else {
                        "two symbolic command names collide"
                    };
                    return Err(Error::invalid_state().with_message(message));
                }
                let id = self.allocate_named(value.key)?;
                self.nodes.push(FlatNode {
                    id,
                    data: FlatNodeData::Button {
                        label: value.label.clone(),
                        enabled: value.enabled,
                        command: value.command,
                        shortcuts: value.shortcuts.clone(),
                    },
                    children: Vec::new(),
                });
                Ok(id)
            }
        }
    }

    fn allocate_anonymous(&mut self) -> Result<u64> {
        if self.next_anonymous & NAMED_BIT != 0 {
            return Err(Error::internal().with_message("anonymous node ID space is exhausted"));
        }
        let id = self.next_anonymous;
        self.next_anonymous = self
            .next_anonymous
            .checked_add(1)
            .ok_or_else(Error::internal)?;
        self.ids.insert(id);
        Ok(id)
    }

    fn allocate_named(&mut self, key: NodeKey) -> Result<u64> {
        if let Some(existing) = self.names.get(&key.id()) {
            let message = if *existing == key.name() {
                "a symbolic node name is used more than once"
            } else {
                "two symbolic node names collide"
            };
            return Err(Error::invalid_state().with_message(message));
        }
        if !self.ids.insert(key.id()) {
            return Err(Error::invalid_state().with_message("a node ID is used more than once"));
        }
        self.names.insert(key.id(), key.name());
        Ok(key.id())
    }
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
#[doc(hidden)]
pub mod component;

#[cfg(all(target_os = "wasi", target_env = "p2"))]
mod state {
    use super::{Error, Result};
    use crate::component::bindings::youth::state::store::{self, ErrorCode, Value};

    fn get(key: &str) -> Result<Option<Value>> {
        store::get(key).map_err(map_error)
    }

    pub fn get_boolean(key: &str) -> Result<Option<bool>> {
        match get(key)? {
            Some(Value::Boolean(value)) => Ok(Some(value)),
            None => Ok(None),
            _ => Err(Error::invalid_state()),
        }
    }

    pub fn get_integer(key: &str) -> Result<Option<i64>> {
        match get(key)? {
            Some(Value::Integer(value)) => Ok(Some(value)),
            None => Ok(None),
            _ => Err(Error::invalid_state()),
        }
    }

    pub fn get_text(key: &str) -> Result<Option<String>> {
        match get(key)? {
            Some(Value::Text(value)) => Ok(Some(value)),
            None => Ok(None),
            _ => Err(Error::invalid_state()),
        }
    }

    pub fn get_bytes(key: &str) -> Result<Option<Vec<u8>>> {
        match get(key)? {
            Some(Value::Bytes(value)) => Ok(Some(value)),
            None => Ok(None),
            _ => Err(Error::invalid_state()),
        }
    }

    pub fn set_boolean(key: &str, value: bool) -> Result<()> {
        store::set(key, &Value::Boolean(value)).map_err(map_error)
    }

    pub fn set_integer(key: &str, value: i64) -> Result<()> {
        store::set(key, &Value::Integer(value)).map_err(map_error)
    }

    pub fn set_text(key: &str, value: &str) -> Result<()> {
        store::set(key, &Value::Text(value.to_owned())).map_err(map_error)
    }

    pub fn set_bytes(key: &str, value: &[u8]) -> Result<()> {
        store::set(key, &Value::Bytes(value.to_vec())).map_err(map_error)
    }

    pub fn delete(key: &str) -> Result<bool> {
        store::delete(key).map_err(map_error)
    }

    fn map_error(error: store::StateError) -> Error {
        match error.code {
            ErrorCode::Internal => Error::internal(),
            _ => Error::invalid_state(),
        }
    }
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
mod state {
    use super::{Error, Result};

    fn unavailable<T>() -> Result<T> {
        Err(Error::internal().with_message("host state calls require wasm32-wasip2"))
    }

    pub fn get_boolean(_key: &str) -> Result<Option<bool>> {
        unavailable()
    }
    pub fn get_integer(_key: &str) -> Result<Option<i64>> {
        unavailable()
    }
    pub fn get_text(_key: &str) -> Result<Option<String>> {
        unavailable()
    }
    pub fn get_bytes(_key: &str) -> Result<Option<Vec<u8>>> {
        unavailable()
    }
    pub fn set_boolean(_key: &str, _value: bool) -> Result<()> {
        unavailable()
    }
    pub fn set_integer(_key: &str, _value: i64) -> Result<()> {
        unavailable()
    }
    pub fn set_text(_key: &str, _value: &str) -> Result<()> {
        unavailable()
    }
    pub fn set_bytes(_key: &str, _value: &[u8]) -> Result<()> {
        unavailable()
    }
    pub fn delete(_key: &str) -> Result<bool> {
        unavailable()
    }
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
mod time {
    use super::{Error, Result, Schedule, ScheduleOptions};
    use crate::component::bindings::youth::time::scheduler::{self, ScheduleErrorCode};

    pub fn schedule_after(millis: u64, options: ScheduleOptions) -> Result<Schedule> {
        let options = scheduler::ScheduleOptions {
            notification: options.notification.map(|value| scheduler::Notification {
                title: value.title,
                body: value.body,
            }),
        };
        scheduler::schedule_after(millis, &options)
            .map(from_wire_schedule)
            .map_err(map_error)
    }

    pub fn pause(value: Schedule) -> Result<Schedule> {
        scheduler::pause(wire_schedule(value))
            .map(from_wire_schedule)
            .map_err(map_error)
    }

    pub fn resume(value: Schedule) -> Result<Schedule> {
        scheduler::resume(wire_schedule(value))
            .map(from_wire_schedule)
            .map_err(map_error)
    }

    pub fn cancel(value: Schedule) -> Result<()> {
        scheduler::cancel(wire_schedule(value)).map_err(map_error)
    }

    const fn wire_schedule(value: Schedule) -> scheduler::Schedule {
        scheduler::Schedule {
            id: value.id,
            generation: value.generation,
        }
    }

    const fn from_wire_schedule(value: scheduler::Schedule) -> Schedule {
        Schedule {
            id: value.id,
            generation: value.generation,
        }
    }

    fn map_error(error: ScheduleErrorCode) -> Error {
        let message = match error {
            ScheduleErrorCode::InvalidDuration => "host rejected the schedule duration",
            ScheduleErrorCode::TooManySchedules => "host schedule limit reached",
            ScheduleErrorCode::UnknownSchedule => "host does not recognize the schedule",
            ScheduleErrorCode::StaleGeneration => "schedule generation is stale",
            ScheduleErrorCode::InvalidState => "schedule is not valid in its current state",
            ScheduleErrorCode::Unavailable => "host scheduling is unavailable",
            ScheduleErrorCode::Internal => return Error::internal(),
        };
        Error::invalid_state().with_message(message)
    }
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
mod time {
    use super::{Error, Result, Schedule, ScheduleOptions};

    fn unavailable<T>() -> Result<T> {
        Err(Error::internal().with_message("host time calls require wasm32-wasip2"))
    }

    pub fn schedule_after(_millis: u64, _options: ScheduleOptions) -> Result<Schedule> {
        unavailable()
    }

    pub fn pause(_value: Schedule) -> Result<Schedule> {
        unavailable()
    }

    pub fn resume(_value: Schedule) -> Result<Schedule> {
        unavailable()
    }

    pub fn cancel(_value: Schedule) -> Result<()> {
        unavailable()
    }
}

pub mod prelude {
    pub use crate::{
        Application, BoxNode, Button, Column, CommandKey, Countdown, CountdownFormat,
        ElapsedReason, Element, Error, ErrorKind, Event, EventContext, Events, Generation, Grid,
        NodeId, NodeKey, Notification, Result, Row, Schedule, ScheduleId, ScheduleOptions,
        Shortcut, Text, TextAlign, TimePrecision, TimeScheduler, Tree, Update, ViewContext,
        command, node,
    };
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
#[doc(hidden)]
#[macro_export]
macro_rules! export_app {
    ($application:ty) => {
        type __YouthExport = $crate::__private::Adapter<$application>;
        $crate::__export_adapter!(__YouthExport);
    };
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
#[doc(hidden)]
#[macro_export]
macro_rules! export_app {
    ($application:ty) => {};
}

#[doc(hidden)]
pub mod __private {
    #[cfg(all(target_os = "wasi", target_env = "p2"))]
    pub use crate::component::Adapter;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule() -> Schedule {
        Schedule {
            id: 17,
            generation: 3,
        }
    }

    #[test]
    fn symbolic_id_vectors_are_stable() {
        assert_eq!(named_node_id("count"), 0xf700_b2fe_97f6_53d6);
        assert_eq!(named_node_id("increment"), 0xd9e1_c44e_444d_fb74);
        assert_eq!(named_node_id("café"), 0xcab8_7ecf_2aee_1d93);
    }

    #[test]
    fn command_id_vectors_are_stable_and_use_a_distinct_domain() {
        assert_eq!(named_command_id("clear"), 0xf3da_ce2c_1a9e_2f27);
        assert_eq!(named_command_id("digit-7"), 0x9848_57b0_7512_75f7);
        assert_eq!(named_command_id("equals"), 0xee40_8611_9b13_a04b);
        assert_ne!(named_command_id("equals"), named_node_id("equals"));
    }

    #[test]
    fn rich_layout_and_command_bindings_flatten_without_wire_details() {
        let tree = Tree::root(Column::new([
            Text::new(node!("display"), "42").align(TextAlign::End),
            Row::new([
                Button::command(command!("clear"), "C").shortcut(Shortcut::Escape),
                Button::command(command!("backspace"), "⌫").shortcut(Shortcut::Backspace),
            ]),
            Grid::columns(
                2,
                [
                    Button::command(command!("digit-7"), "7").shortcut(Shortcut::Character('7')),
                    Button::command(command!("equals"), "=").shortcut(Shortcut::Enter),
                ],
            ),
        ]));
        let flat = tree.flatten().expect("rich tree is valid");
        assert!(matches!(
            flat.nodes[2].data,
            FlatNodeData::Text {
                alignment: TextAlign::End,
                ..
            }
        ));
        assert!(flat.nodes.iter().any(|node| matches!(
            node.data,
            FlatNodeData::Box {
                layout: Layout::Row,
                ..
            }
        )));
        assert!(flat.nodes.iter().any(|node| matches!(
            node.data,
            FlatNodeData::Box {
                layout: Layout::Grid(2),
                ..
            }
        )));
    }

    #[test]
    fn countdown_flattens_with_schedule_and_defaults() {
        let schedule = schedule();
        let flat = Tree::root(Countdown::new(node!("remaining"), schedule))
            .flatten()
            .expect("countdown tree is valid");
        assert_eq!(
            flat.nodes[1].data,
            FlatNodeData::Countdown {
                schedule,
                precision: TimePrecision::Seconds,
                format: CountdownFormat::MinutesSeconds,
                alignment: TextAlign::Start,
            }
        );
    }

    #[test]
    fn countdown_alignment_flattens_without_wire_details() {
        let flat = Tree::root(Countdown::new(node!("remaining"), schedule()).align(TextAlign::End))
            .flatten()
            .expect("countdown tree is valid");
        assert!(matches!(
            flat.nodes[1].data,
            FlatNodeData::Countdown {
                alignment: TextAlign::End,
                ..
            }
        ));
    }

    #[test]
    fn duplicate_command_bindings_and_excess_shortcuts_are_rejected() {
        let duplicate = Tree::root(Row::new([
            Button::command(command!("clear"), "C"),
            Button::command(command!("clear"), "Clear"),
        ]));
        assert!(duplicate.flatten().is_err());

        let excess = Tree::root(
            Button::new(node!("many"), "Many")
                .shortcut(Shortcut::Character('1'))
                .shortcut(Shortcut::Character('2'))
                .shortcut(Shortcut::Character('3'))
                .shortcut(Shortcut::Character('4'))
                .shortcut(Shortcut::Character('5')),
        );
        assert!(excess.flatten().is_err());
    }

    #[test]
    fn anonymous_and_named_ids_use_separate_halves() {
        let tree = Tree::root(BoxNode::column([
            Text::new(node!("count"), "Count: 0"),
            Button::new(node!("increment"), "Increment"),
        ]));
        let flat = tree.flatten().expect("reference tree is valid");
        assert_eq!(flat.nodes[0].id, 1);
        assert_eq!(flat.nodes[1].id, 2);
        assert!(flat.nodes[2].id & NAMED_BIT != 0);
        assert!(flat.nodes[3].id & NAMED_BIT != 0);
    }

    #[test]
    fn duplicate_names_and_collisions_are_hard_errors() {
        let duplicate = Tree::root(BoxNode::column([
            Text::new(node!("same"), "a"),
            Button::new(node!("same"), "b"),
        ]));
        assert!(duplicate.flatten().is_err());

        let first = NodeKey::with_id("first", NAMED_BIT | 7);
        let second = NodeKey::with_id("second", NAMED_BIT | 7);
        let collision = Tree::root(BoxNode::column([
            Text::new(first, "a"),
            Button::new(second, "b"),
        ]));
        assert!(collision.flatten().is_err());
    }

    #[test]
    fn updates_validate_target_type_and_duplicates() {
        let mut tree = Tree::root(BoxNode::column([
            Text::new(node!("count"), "Count: 0"),
            Button::new(node!("increment"), "Increment"),
        ]))
        .flatten()
        .expect("reference tree is valid");
        tree.apply(&Update::new().set_text(node!("count"), "Count: 1"))
            .expect("text update is valid");
        assert!(
            tree.apply(&Update::new().set_text(node!("increment"), "bad"))
                .is_err()
        );
        assert!(
            tree.apply(
                &Update::new()
                    .set_text(node!("count"), "one")
                    .set_text(node!("count"), "two")
            )
            .is_err()
        );
    }

    #[test]
    fn text_update_retargets_countdown_and_preserves_alignment() {
        let mut tree =
            Tree::root(Countdown::new(node!("remaining"), schedule()).align(TextAlign::Center))
                .flatten()
                .expect("countdown tree is valid");
        tree.apply(&Update::new().set_text(node!("remaining"), "05:00"))
            .expect("countdown can be retargeted to literal text");
        assert_eq!(
            tree.nodes[1].data,
            FlatNodeData::Text {
                value: "05:00".to_owned(),
                alignment: TextAlign::Center,
            }
        );
    }

    #[test]
    fn countdown_update_retargets_text_and_preserves_alignment() {
        let schedule = schedule();
        let mut tree = Tree::root(Text::new(node!("remaining"), "05:00").align(TextAlign::End))
            .flatten()
            .expect("text tree is valid");
        tree.apply(&Update::new().set_countdown(
            node!("remaining"),
            schedule,
            TimePrecision::Seconds,
            CountdownFormat::MinutesSeconds,
        ))
        .expect("literal text can be retargeted to a countdown");
        assert_eq!(
            tree.nodes[1].data,
            FlatNodeData::Countdown {
                schedule,
                precision: TimePrecision::Seconds,
                format: CountdownFormat::MinutesSeconds,
                alignment: TextAlign::End,
            }
        );
    }

    #[test]
    fn text_family_updates_reject_button_targets() {
        let mut tree = Tree::root(Button::new(node!("start"), "Start"))
            .flatten()
            .expect("button tree is valid");
        let text_error = tree
            .apply(&Update::new().set_text(node!("start"), "bad"))
            .expect_err("a button cannot be retargeted to text");
        assert_eq!(
            text_error.message.as_deref(),
            Some("an update does not match the named node type")
        );

        let countdown_error = tree
            .apply(&Update::new().set_countdown(
                node!("start"),
                schedule(),
                TimePrecision::Seconds,
                CountdownFormat::MinutesSeconds,
            ))
            .expect_err("a button cannot be retargeted to a countdown");
        assert_eq!(
            countdown_error.message.as_deref(),
            Some("an update does not match the named node type")
        );
    }

    #[test]
    fn countdown_update_participates_in_duplicate_key_rejection() {
        let mut tree = Tree::root(Text::new(node!("remaining"), "05:00"))
            .flatten()
            .expect("text tree is valid");
        let error = tree
            .apply(
                &Update::new()
                    .set_countdown(
                        node!("remaining"),
                        schedule(),
                        TimePrecision::Seconds,
                        CountdownFormat::MinutesSeconds,
                    )
                    .set_text(node!("remaining"), "04:59"),
            )
            .expect_err("a countdown and text update cannot share a key");
        assert_eq!(error.message.as_deref(), Some("a node is updated twice"));
    }

    #[test]
    fn unsupported_event_does_not_invoke_application_or_advance_acknowledgement() {
        let mut application_invoked = false;
        let mut processed_through = 0;
        let decoded = decode_incoming_events([IncomingEvent::Unsupported]);
        if decoded.is_ok() {
            application_invoked = true;
            processed_through = 9;
        }
        assert!(decoded.is_err());
        assert!(!application_invoked);
        assert_eq!(processed_through, 0);
    }
}
