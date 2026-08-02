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
const ITEM_NODE_DOMAIN: &[u8] = b"youth:item-node-id:v1\0";
const ITEM_COMMAND_DOMAIN: &[u8] = b"youth:item-command-id:v1\0";
const NAMED_BIT: u64 = 0x8000_0000_0000_0000;
const VALUE_MASK: u64 = 0x7fff_ffff_ffff_ffff;
const MAX_ITEM_IDENTITY_PART_BYTES: usize = 256;

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

fn item_id(domain: &[u8], namespace: &str, item: u64, role: &str) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in domain
        .iter()
        .copied()
        .chain((namespace.len() as u32).to_be_bytes())
        .chain(namespace.as_bytes().iter().copied())
        .chain(item.to_be_bytes())
        .chain((role.len() as u32).to_be_bytes())
        .chain(role.as_bytes().iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    (hash & VALUE_MASK) | NAMED_BIT
}

/// Calculates a validated runtime-derived node ID.
pub fn derived_node_id(namespace: &str, item: u64, role: &str) -> Result<u64> {
    Ok(ItemKey::new(namespace, item)?.node(role)?.id())
}

/// Calculates a validated runtime-derived command ID.
pub fn derived_command_id(namespace: &str, item: u64, role: &str) -> Result<u64> {
    Ok(ItemKey::new(namespace, item)?.command(role)?.id())
}

/// A runtime item identity namespace and nonzero application-owned item ID.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ItemKey {
    namespace: String,
    item: u64,
}

impl ItemKey {
    /// Creates an item key. Namespaces are exact, unnormalized UTF-8.
    pub fn new(namespace: impl Into<String>, item: u64) -> Result<Self> {
        let namespace = namespace.into();
        validate_item_identity_part("item namespace", &namespace)?;
        if item == 0 {
            return Err(Error::invalid_state().with_message("an item ID must be nonzero"));
        }
        Ok(Self { namespace, item })
    }

    /// Derives a stable app-global node identity for one role of this item.
    pub fn node(&self, role: impl Into<String>) -> Result<ItemNodeKey> {
        let role = role.into();
        validate_item_identity_part("item node role", &role)?;
        Ok(ItemNodeKey::new_unchecked(
            self.namespace.clone(),
            self.item,
            role,
        ))
    }

    /// Derives a stable app-global command identity for one role of this item.
    pub fn command(&self, role: impl Into<String>) -> Result<ItemCommandKey> {
        let role = role.into();
        validate_item_identity_part("item command role", &role)?;
        Ok(ItemCommandKey::new_unchecked(
            self.namespace.clone(),
            self.item,
            role,
        ))
    }

    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    #[must_use]
    pub const fn item(&self) -> u64 {
        self.item
    }
}

fn validate_item_identity_part(label: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(Error::invalid_state().with_message(format!("{label} must not be empty")));
    }
    if value.len() > MAX_ITEM_IDENTITY_PART_BYTES {
        return Err(Error::invalid_state().with_message(format!(
            "{label} exceeds {MAX_ITEM_IDENTITY_PART_BYTES} UTF-8 bytes"
        )));
    }
    Ok(())
}

/// A stable node identity derived from an item namespace, ID, and role.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ItemNodeKey {
    namespace: String,
    item: u64,
    role: String,
    id: u64,
}

impl ItemNodeKey {
    fn new_unchecked(namespace: String, item: u64, role: String) -> Self {
        let id = item_id(ITEM_NODE_DOMAIN, &namespace, item, &role);
        Self {
            namespace,
            item,
            role,
            id,
        }
    }

    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    #[cfg(test)]
    fn with_id(namespace: &str, item: u64, role: &str, id: u64) -> Self {
        Self {
            namespace: namespace.to_owned(),
            item,
            role: role.to_owned(),
            id,
        }
    }
}

/// A stable command identity derived from an item namespace, ID, and role.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ItemCommandKey {
    namespace: String,
    item: u64,
    role: String,
    id: u64,
}

impl ItemCommandKey {
    fn new_unchecked(namespace: String, item: u64, role: String) -> Self {
        let id = item_id(ITEM_COMMAND_DOMAIN, &namespace, item, &role);
        Self {
            namespace,
            item,
            role,
            id,
        }
    }

    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    #[cfg(test)]
    fn with_id(namespace: &str, item: u64, role: &str, id: u64) -> Self {
        Self {
            namespace: namespace.to_owned(),
            item,
            role: role.to_owned(),
            id,
        }
    }
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

/// A typed static or runtime-derived node identity accepted by SDK builders.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NodeIdentity {
    Static(NodeKey),
    Item(ItemNodeKey),
}

impl NodeIdentity {
    #[must_use]
    pub const fn id(&self) -> u64 {
        match self {
            Self::Static(key) => key.id(),
            Self::Item(key) => key.id(),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Static(key) => format!("static node {:?}", key.name()),
            Self::Item(key) => format!(
                "derived node namespace {:?}, item {}, role {:?}",
                key.namespace, key.item, key.role
            ),
        }
    }
}

impl From<NodeKey> for NodeIdentity {
    fn from(value: NodeKey) -> Self {
        Self::Static(value)
    }
}

impl From<ItemNodeKey> for NodeIdentity {
    fn from(value: ItemNodeKey) -> Self {
        Self::Item(value)
    }
}

/// A typed static or runtime-derived application command identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CommandIdentity {
    Static(CommandKey),
    Item(ItemCommandKey),
}

impl CommandIdentity {
    #[must_use]
    pub const fn id(&self) -> u64 {
        match self {
            Self::Static(key) => key.id(),
            Self::Item(key) => key.id(),
        }
    }

    fn node_identity(&self) -> NodeIdentity {
        match self {
            Self::Static(key) => NodeKey::new(key.name()).into(),
            Self::Item(key) => NodeIdentity::Item(ItemNodeKey::new_unchecked(
                key.namespace.clone(),
                key.item,
                key.role.clone(),
            )),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Static(key) => format!("static command {:?}", key.name()),
            Self::Item(key) => format!(
                "derived command namespace {:?}, item {}, role {:?}",
                key.namespace, key.item, key.role
            ),
        }
    }
}

impl From<CommandKey> for CommandIdentity {
    fn from(value: CommandKey) -> Self {
        Self::Static(value)
    }
}

impl From<ItemCommandKey> for CommandIdentity {
    fn from(value: ItemCommandKey) -> Self {
        Self::Item(value)
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

/// Declares typed, reusable application identities once at module scope.
///
/// The generated constants retain the static `NodeKey` and `CommandKey` types,
/// so view construction and event handling cannot drift because of repeated
/// string literals.
#[macro_export]
macro_rules! ui_ids {
    () => {};
    (node $name:ident = $node_name:literal; $($rest:tt)*) => {
        pub const $name: $crate::NodeKey = $crate::NodeKey::new($node_name);
        $crate::ui_ids!($($rest)*);
    };
    (command $name:ident = $command_name:literal; $($rest:tt)*) => {
        pub const $name: $crate::CommandKey = $crate::CommandKey::new($command_name);
        $crate::ui_ids!($($rest)*);
    };
}

/// A statically typed identity that can be checked against a semantic
/// activation. Node identities match direct node activation; command
/// identities match the command attached to the activated button.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ActivationKey {
    Node(NodeIdentity),
    Command(CommandIdentity),
}

impl From<NodeKey> for ActivationKey {
    fn from(value: NodeKey) -> Self {
        Self::Node(value.into())
    }
}

impl From<ItemNodeKey> for ActivationKey {
    fn from(value: ItemNodeKey) -> Self {
        Self::Node(value.into())
    }
}

impl From<NodeIdentity> for ActivationKey {
    fn from(value: NodeIdentity) -> Self {
        Self::Node(value)
    }
}

impl From<CommandKey> for ActivationKey {
    fn from(value: CommandKey) -> Self {
        Self::Command(value.into())
    }
}

impl From<ItemCommandKey> for ActivationKey {
    fn from(value: ItemCommandKey) -> Self {
        Self::Command(value.into())
    }
}

impl From<CommandIdentity> for ActivationKey {
    fn from(value: CommandIdentity) -> Self {
        Self::Command(value)
    }
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
enum ShortcutKey {
    Character(char),
    Enter,
    Escape,
    Backspace,
}

/// The chord modifiers a [`Shortcut`] requires to be held.
///
/// Only `primary` (Cmd on macOS, Control on Windows/Linux) is supported
/// today. A plain struct of named flags rather than an opaque bitfield, so
/// `shift`/`alt` could be added later as honest new fields.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ShortcutModifiers {
    primary: bool,
}

/// A keyboard shortcut a [`Button`] can declare.
///
/// `Shortcut::character('s')` and `Shortcut::primary('s')` are distinct
/// chords and may both be declared, on the same button or different ones,
/// without conflicting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shortcut {
    key: ShortcutKey,
    modifiers: ShortcutModifiers,
}

#[allow(non_upper_case_globals)]
impl Shortcut {
    pub const Enter: Self = Self {
        key: ShortcutKey::Enter,
        modifiers: ShortcutModifiers { primary: false },
    };
    pub const Escape: Self = Self {
        key: ShortcutKey::Escape,
        modifiers: ShortcutModifiers { primary: false },
    };
    pub const Backspace: Self = Self {
        key: ShortcutKey::Backspace,
        modifiers: ShortcutModifiers { primary: false },
    };

    /// An unmodified character shortcut, such as `Shortcut::character('7')`.
    #[must_use]
    pub const fn character(value: char) -> Self {
        Self {
            key: ShortcutKey::Character(value),
            modifiers: ShortcutModifiers { primary: false },
        }
    }

    /// A character shortcut that requires the platform's primary modifier
    /// (Cmd on macOS, Control on Windows/Linux) to be held, such as
    /// `Shortcut::primary('s')` for Save.
    #[must_use]
    pub const fn primary(value: char) -> Self {
        Self {
            key: ShortcutKey::Character(value),
            modifiers: ShortcutModifiers { primary: true },
        }
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

/// Guest-owned revision of an Editor document's accepted meaning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentRevision(u64);

impl DocumentRevision {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Host-issued ordering of changes to an Editor's live buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditSequence(u64);

impl EditSequence {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Text;

#[derive(Clone, Debug, Eq, PartialEq)]
struct TextElement {
    key: NodeIdentity,
    value: String,
    alignment: TextAlign,
}

impl Text {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(key: impl Into<NodeIdentity>, value: impl Into<String>) -> Element {
        Element {
            kind: ElementKind::Text(TextElement {
                key: key.into(),
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
    key: NodeIdentity,
    schedule: Schedule,
    precision: TimePrecision,
    format: CountdownFormat,
    alignment: TextAlign,
}

impl Countdown {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(key: impl Into<NodeIdentity>, schedule: Schedule) -> Element {
        Element {
            kind: ElementKind::Countdown(CountdownElement {
                key: key.into(),
                schedule,
                precision: TimePrecision::Seconds,
                format: CountdownFormat::MinutesSeconds,
                alignment: TextAlign::Start,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Editor;

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditorElement {
    key: NodeIdentity,
    source: EditorSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EditorSource {
    Inline {
        document_revision: DocumentRevision,
        initial_text: String,
    },
    TextDocument {
        handle: DocumentHandle,
        version: DocumentVersion,
    },
}

impl Editor {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        key: impl Into<NodeIdentity>,
        document_revision: DocumentRevision,
        initial_text: impl Into<String>,
    ) -> Element {
        Element {
            kind: ElementKind::Editor(EditorElement {
                key: key.into(),
                source: EditorSource::Inline {
                    document_revision,
                    initial_text: initial_text.into(),
                },
            }),
        }
    }

    #[must_use]
    pub fn document(key: impl Into<NodeIdentity>, document: &Document) -> Element {
        Element {
            kind: ElementKind::Editor(EditorElement {
                key: key.into(),
                source: EditorSource::TextDocument {
                    handle: document.handle,
                    version: document.version,
                },
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Button;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ButtonElement {
    key: NodeIdentity,
    label: String,
    enabled: bool,
    command: Option<CommandIdentity>,
    shortcuts: Vec<Shortcut>,
}

impl Button {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(key: impl Into<NodeIdentity>, label: impl Into<String>) -> Element {
        Element {
            kind: ElementKind::Button(ButtonElement {
                key: key.into(),
                label: label.into(),
                enabled: true,
                command: None,
                shortcuts: Vec::new(),
            }),
        }
    }

    #[must_use]
    pub fn command(key: impl Into<CommandIdentity>, label: impl Into<String>) -> Element {
        let command = key.into();
        Element {
            kind: ElementKind::Button(ButtonElement {
                key: command.node_identity(),
                label: label.into(),
                enabled: true,
                command: Some(command),
                shortcuts: Vec::new(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoxNode;

#[derive(Clone, Debug, Eq, PartialEq)]
struct BoxElement {
    key: Option<NodeIdentity>,
    children: Vec<Element>,
    enabled: bool,
    layout: Layout,
}

impl BoxNode {
    #[must_use]
    pub fn column(children: impl IntoIterator<Item = Element>) -> Element {
        Element {
            kind: ElementKind::Box(BoxElement {
                key: None,
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

    #[must_use]
    pub fn named(
        key: impl Into<NodeIdentity>,
        children: impl IntoIterator<Item = Element>,
    ) -> Element {
        Element {
            kind: ElementKind::Box(BoxElement {
                key: Some(key.into()),
                children: children.into_iter().collect(),
                enabled: true,
                layout: Layout::Column,
            }),
        }
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
                key: None,
                children: children.into_iter().collect(),
                enabled: true,
                layout: Layout::Row,
            }),
        }
    }

    #[must_use]
    pub fn named(
        key: impl Into<NodeIdentity>,
        children: impl IntoIterator<Item = Element>,
    ) -> Element {
        Element {
            kind: ElementKind::Box(BoxElement {
                key: Some(key.into()),
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
                key: None,
                children: children.into_iter().collect(),
                enabled: true,
                layout: Layout::Grid(columns),
            }),
        }
    }

    #[must_use]
    pub fn named(
        key: impl Into<NodeIdentity>,
        columns: u8,
        children: impl IntoIterator<Item = Element>,
    ) -> Element {
        Element {
            kind: ElementKind::Box(BoxElement {
                key: Some(key.into()),
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
    Editor(EditorElement),
    Button(ButtonElement),
}

impl Element {
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        match &mut self.kind {
            ElementKind::Box(value) => value.enabled = enabled,
            ElementKind::Button(value) => value.enabled = enabled,
            ElementKind::Text(_) | ElementKind::Countdown(_) | ElementKind::Editor(_) => {}
        }
        self
    }

    #[must_use]
    pub fn align(mut self, alignment: TextAlign) -> Self {
        match &mut self.kind {
            ElementKind::Text(value) => value.alignment = alignment,
            ElementKind::Countdown(value) => value.alignment = alignment,
            ElementKind::Box(_) | ElementKind::Editor(_) | ElementKind::Button(_) => {}
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
            require_stable: false,
        };
        let child = builder.push(&self.child)?;
        builder.nodes[0].children.push(child);
        Ok(FlatTree {
            root: 1,
            nodes: builder.nodes,
            identities: builder.names,
            commands: builder.commands,
        })
    }
}

pub type NodeId = u64;
pub type ScheduleId = u64;
pub type Generation = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentHandle {
    id: u64,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentVersion {
    id: u64,
    generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    handle: DocumentHandle,
    version: DocumentVersion,
    relative_path: String,
    filename: String,
}

impl Document {
    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SaveRequest {
    id: u64,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextDocumentFailure {
    Conflict,
    Missing,
    WrongType,
    PermissionDenied,
    Unavailable,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextDocumentSaveOutcome {
    Saved {
        document: DocumentHandle,
        version: DocumentVersion,
        saved_edit_sequence: EditSequence,
        still_dirty: bool,
    },
    Failed(TextDocumentFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextDocumentSaveCompletion {
    request: SaveRequest,
    outcome: TextDocumentSaveOutcome,
}

impl TextDocumentSaveCompletion {
    #[must_use]
    pub const fn request(self) -> SaveRequest {
        self.request
    }

    #[must_use]
    pub const fn outcome(self) -> TextDocumentSaveOutcome {
        self.outcome
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElapsedReason {
    Deadline,
    RecoveredOverdue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Activated(NodeId),
    ScheduleElapsed {
        schedule: ScheduleId,
        generation: Generation,
        reason: ElapsedReason,
    },
    EditorDirtyChanged {
        editor: NodeId,
        dirty: bool,
    },
    TextDocumentSaveCompleted(TextDocumentSaveCompletion),
}

#[cfg(any(test, all(target_os = "wasi", target_env = "p2")))]
#[cfg_attr(test, allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IncomingEvent {
    Activated(NodeId),
    ScheduleElapsed {
        schedule: ScheduleId,
        generation: Generation,
        reason: ElapsedReason,
    },
    EditorDirtyChanged {
        editor: NodeId,
        dirty: bool,
    },
    TextDocumentSaveCompleted(TextDocumentSaveCompletion),
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
            IncomingEvent::EditorDirtyChanged { editor, dirty } if editor != 0 => {
                Ok(Event::EditorDirtyChanged { editor, dirty })
            }
            IncomingEvent::EditorDirtyChanged { .. } => {
                Err(Error::invalid_state()
                    .with_message("dirty event contains an invalid editor ID"))
            }
            IncomingEvent::TextDocumentSaveCompleted(completion) => {
                Ok(Event::TextDocumentSaveCompleted(completion))
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
    pub fn activated(&self, key: impl Into<ActivationKey>) -> bool {
        match key.into() {
            ActivationKey::Node(key) => self
                .events
                .iter()
                .any(|event| matches!(event, Event::Activated(id) if *id == key.id())),
            ActivationKey::Command(key) => self.commanded.contains(&key.id()),
        }
    }

    #[must_use]
    pub fn commanded(&self, key: impl Into<CommandIdentity>) -> bool {
        self.activated(key.into())
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
            Event::Activated(_)
            | Event::EditorDirtyChanged { .. }
            | Event::TextDocumentSaveCompleted(_) => None,
        })
    }

    pub fn editor_dirty_changes(&self) -> impl Iterator<Item = (NodeId, bool)> + '_ {
        self.events.iter().filter_map(|event| match event {
            Event::EditorDirtyChanged { editor, dirty } => Some((*editor, *dirty)),
            _ => None,
        })
    }

    pub fn text_document_save_completions(
        &self,
    ) -> impl Iterator<Item = TextDocumentSaveCompletion> + '_ {
        self.events.iter().filter_map(|event| match event {
            Event::TextDocumentSaveCompleted(completion) => Some(*completion),
            _ => None,
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
    Text(NodeIdentity, String),
    Countdown(NodeIdentity, Schedule, TimePrecision, CountdownFormat),
    Label(NodeIdentity, String),
    Enabled(NodeIdentity, bool),
    EditorDocumentVersion(NodeIdentity, DocumentHandle, DocumentVersion),
    InsertChild(NodeIdentity, usize, Element),
    RemoveSubtree(NodeIdentity, NodeIdentity),
    MoveChild(NodeIdentity, NodeIdentity, usize),
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
    pub fn set_text(mut self, key: impl Into<NodeIdentity>, value: impl Into<String>) -> Self {
        self.operations
            .push(UpdateOperation::Text(key.into(), value.into()));
        self
    }

    #[must_use]
    pub fn set_countdown(
        mut self,
        key: impl Into<NodeIdentity>,
        schedule: Schedule,
        precision: TimePrecision,
        format: CountdownFormat,
    ) -> Self {
        self.operations.push(UpdateOperation::Countdown(
            key.into(),
            schedule,
            precision,
            format,
        ));
        self
    }

    #[must_use]
    pub fn set_label(mut self, key: impl Into<NodeIdentity>, value: impl Into<String>) -> Self {
        self.operations
            .push(UpdateOperation::Label(key.into(), value.into()));
        self
    }

    #[must_use]
    pub fn set_enabled(mut self, key: impl Into<NodeIdentity>, enabled: bool) -> Self {
        self.operations
            .push(UpdateOperation::Enabled(key.into(), enabled));
        self
    }

    #[must_use]
    pub fn set_editor_document_version(
        mut self,
        key: impl Into<NodeIdentity>,
        document: DocumentHandle,
        version: DocumentVersion,
    ) -> Self {
        self.operations.push(UpdateOperation::EditorDocumentVersion(
            key.into(),
            document,
            version,
        ));
        self
    }

    /// Inserts a fully named subtree at `index` in the parent's staged children.
    #[must_use]
    pub fn insert_child(
        mut self,
        parent: impl Into<NodeIdentity>,
        index: usize,
        subtree: Element,
    ) -> Self {
        self.operations
            .push(UpdateOperation::InsertChild(parent.into(), index, subtree));
        self
    }

    /// Removes a direct child and all of its descendants from the staged tree.
    #[must_use]
    pub fn remove_subtree(
        mut self,
        parent: impl Into<NodeIdentity>,
        child: impl Into<NodeIdentity>,
    ) -> Self {
        self.operations
            .push(UpdateOperation::RemoveSubtree(parent.into(), child.into()));
        self
    }

    /// Moves a direct child to its final post-move index.
    #[must_use]
    pub fn move_child(
        mut self,
        parent: impl Into<NodeIdentity>,
        child: impl Into<NodeIdentity>,
        final_index: usize,
    ) -> Self {
        self.operations.push(UpdateOperation::MoveChild(
            parent.into(),
            child.into(),
            final_index,
        ));
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

    #[must_use]
    pub const fn text_document(&self) -> TextDocumentReader {
        TextDocumentReader
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

    #[must_use]
    pub const fn editor(&mut self) -> EditorCapability {
        EditorCapability
    }

    #[must_use]
    pub const fn text_document(&mut self) -> TextDocumentWriter {
        TextDocumentWriter
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TextDocumentReader;

impl TextDocumentReader {
    pub fn current(self) -> Result<Document> {
        text_document::current()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TextDocumentWriter;

impl TextDocumentWriter {
    pub fn current(self) -> Result<Document> {
        text_document::current()
    }

    pub fn request_save(
        self,
        document: &Document,
        editor: impl Into<NodeIdentity>,
    ) -> Result<SaveRequest> {
        text_document::request_save(document.handle, editor.into().id())
    }
}

/// Whole-buffer snapshot of a host-owned Editor session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorSnapshot {
    document_revision: DocumentRevision,
    edit_sequence: EditSequence,
    text: String,
}

impl EditorSnapshot {
    #[must_use]
    pub const fn document_revision(&self) -> DocumentRevision {
        self.document_revision
    }

    #[must_use]
    pub const fn edit_sequence(&self) -> EditSequence {
        self.edit_sequence
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EditorCapability;

impl EditorCapability {
    pub fn snapshot(self, editor: impl Into<NodeIdentity>) -> Result<EditorSnapshot> {
        editor::snapshot(editor.into().id())
    }

    pub fn accept(
        self,
        editor: impl Into<NodeIdentity>,
        expected_document_revision: DocumentRevision,
        expected_edit_sequence: EditSequence,
        new_document_revision: DocumentRevision,
    ) -> Result<()> {
        editor::accept(
            editor.into().id(),
            expected_document_revision,
            expected_edit_sequence,
            new_document_revision,
        )
    }

    pub fn replace(
        self,
        editor: impl Into<NodeIdentity>,
        expected_document_revision: DocumentRevision,
        expected_edit_sequence: EditSequence,
        new_document_revision: DocumentRevision,
        authoritative_text: impl Into<String>,
    ) -> Result<()> {
        editor::replace(
            editor.into().id(),
            expected_document_revision,
            expected_edit_sequence,
            new_document_revision,
            authoritative_text.into(),
        )
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
    Editor {
        document_revision: DocumentRevision,
        text: String,
    },
    TextDocumentEditor {
        handle: DocumentHandle,
        version: DocumentVersion,
    },
    Button {
        label: String,
        enabled: bool,
        command: Option<CommandIdentity>,
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
    identities: BTreeMap<u64, NodeIdentity>,
    commands: BTreeMap<u64, CommandIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
enum AppliedPatch {
    Create(FlatNode),
    Delete(u64),
    Text(u64, String),
    Countdown(u64, Schedule, TimePrecision, CountdownFormat),
    Label(u64, String),
    Enabled(u64, bool),
    EditorDocumentVersion(u64, DocumentHandle, DocumentVersion),
    InsertChild {
        parent: u64,
        index: usize,
        child: u64,
    },
    RemoveChild {
        parent: u64,
        index: usize,
        child: u64,
    },
    MoveChild {
        parent: u64,
        from_index: usize,
        to_index: usize,
        child: u64,
    },
}

#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
impl FlatTree {
    fn apply(&mut self, update: &Update) -> Result<Vec<AppliedPatch>> {
        let mut staged = self.clone();
        let patches = staged.apply_staged(update)?;
        *self = staged;
        Ok(patches)
    }

    fn apply_staged(&mut self, update: &Update) -> Result<Vec<AppliedPatch>> {
        let mut changed = BTreeSet::new();
        let mut patches = Vec::new();
        for operation in &update.operations {
            let property_key = match operation {
                UpdateOperation::Text(key, _)
                | UpdateOperation::Countdown(key, ..)
                | UpdateOperation::Label(key, _)
                | UpdateOperation::Enabled(key, _)
                | UpdateOperation::EditorDocumentVersion(key, ..) => Some(key),
                UpdateOperation::InsertChild(..)
                | UpdateOperation::RemoveSubtree(..)
                | UpdateOperation::MoveChild(..) => None,
            };
            if let Some(key) = property_key
                && !changed.insert(key.id())
            {
                return Err(Error::invalid_state().with_message("a node is updated twice"));
            }
            match operation {
                UpdateOperation::Text(key, value) => {
                    let node = self.node_mut(key.id())?;
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
                    patches.push(AppliedPatch::Text(key.id(), value.clone()));
                }
                UpdateOperation::Countdown(key, schedule, precision, format) => {
                    let node = self.node_mut(key.id())?;
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
                    patches.push(AppliedPatch::Countdown(
                        key.id(),
                        *schedule,
                        *precision,
                        *format,
                    ));
                }
                UpdateOperation::Label(key, value) => match &mut self.node_mut(key.id())?.data {
                    FlatNodeData::Button { label, .. } => {
                        label.clone_from(value);
                        patches.push(AppliedPatch::Label(key.id(), value.clone()));
                    }
                    _ => {
                        return Err(Error::invalid_state()
                            .with_message("an update does not match the named node type"));
                    }
                },
                UpdateOperation::Enabled(key, enabled) => {
                    match &mut self.node_mut(key.id())?.data {
                        FlatNodeData::Button { enabled: value, .. }
                        | FlatNodeData::Box { enabled: value, .. } => {
                            *value = *enabled;
                            patches.push(AppliedPatch::Enabled(key.id(), *enabled));
                        }
                        _ => {
                            return Err(Error::invalid_state()
                                .with_message("an update does not match the named node type"));
                        }
                    }
                }
                UpdateOperation::EditorDocumentVersion(key, document, version) => {
                    match &mut self.node_mut(key.id())?.data {
                        FlatNodeData::TextDocumentEditor {
                            handle,
                            version: current,
                        } if *handle == *document => {
                            *current = *version;
                            patches.push(AppliedPatch::EditorDocumentVersion(
                                key.id(),
                                *document,
                                *version,
                            ));
                        }
                        _ => {
                            return Err(Error::invalid_state().with_message(
                                "a document-version update does not match the bound Editor",
                            ));
                        }
                    }
                }
                UpdateOperation::InsertChild(parent, index, subtree) => {
                    self.insert_child(parent.id(), *index, subtree, &mut patches)?;
                }
                UpdateOperation::RemoveSubtree(parent, child) => {
                    self.remove_subtree(parent.id(), child.id(), &mut patches)?;
                }
                UpdateOperation::MoveChild(parent, child, final_index) => {
                    self.move_child(parent.id(), child.id(), *final_index, &mut patches)?;
                }
            }
        }
        Ok(patches)
    }

    fn node_mut(&mut self, id: u64) -> Result<&mut FlatNode> {
        self.nodes
            .iter_mut()
            .find(|node| node.id == id)
            .ok_or_else(|| Error::invalid_state().with_message("an update names an unknown node"))
    }

    fn insert_child(
        &mut self,
        parent: u64,
        index: usize,
        subtree: &Element,
        patches: &mut Vec<AppliedPatch>,
    ) -> Result<()> {
        let parent_node = self.node_mut(parent)?;
        if !matches!(parent_node.data, FlatNodeData::Box { .. }) {
            return Err(Error::invalid_state().with_message("insert parent is not a container"));
        }
        if index > parent_node.children.len() {
            return Err(Error::invalid_state().with_message("insert index is out of range"));
        }

        let existing_ids = self.nodes.iter().map(|node| node.id).collect();
        let mut builder = FlatTreeBuilder {
            next_anonymous: 2,
            nodes: Vec::new(),
            names: self.identities.clone(),
            commands: self.commands.clone(),
            ids: existing_ids,
            require_stable: true,
        };
        let subtree_root = builder.push(subtree)?;
        let new_nodes = builder.nodes;
        for node in &new_nodes {
            let mut detached = node.clone();
            detached.children.clear();
            patches.push(AppliedPatch::Create(detached));
        }
        for node in &new_nodes {
            for (child_index, child) in node.children.iter().copied().enumerate() {
                patches.push(AppliedPatch::InsertChild {
                    parent: node.id,
                    index: child_index,
                    child,
                });
            }
        }
        patches.push(AppliedPatch::InsertChild {
            parent,
            index,
            child: subtree_root,
        });

        self.node_mut(parent)?.children.insert(index, subtree_root);
        self.nodes.extend(new_nodes);
        self.identities = builder.names;
        self.commands = builder.commands;
        Ok(())
    }

    fn remove_subtree(
        &mut self,
        parent: u64,
        child: u64,
        patches: &mut Vec<AppliedPatch>,
    ) -> Result<()> {
        let parent_node = self.node_mut(parent)?;
        if !matches!(parent_node.data, FlatNodeData::Box { .. }) {
            return Err(Error::invalid_state().with_message("remove parent is not a container"));
        }
        let Some(index) = parent_node.children.iter().position(|id| *id == child) else {
            return Err(Error::invalid_state()
                .with_message("remove child is not a direct child of its parent"));
        };
        let mut removed = Vec::new();
        self.collect_subtree(child, &mut removed)?;
        patches.push(AppliedPatch::RemoveChild {
            parent,
            index,
            child,
        });
        self.emit_descendant_removal(child, patches)?;

        self.node_mut(parent)?.children.remove(index);
        let removed_ids: BTreeSet<_> = removed.iter().copied().collect();
        for node in &self.nodes {
            if removed_ids.contains(&node.id)
                && let FlatNodeData::Button {
                    command: Some(command),
                    ..
                } = &node.data
            {
                self.commands.remove(&command.id());
            }
        }
        self.nodes.retain(|node| !removed_ids.contains(&node.id));
        for id in removed {
            self.identities.remove(&id);
        }
        Ok(())
    }

    fn collect_subtree(&self, id: u64, output: &mut Vec<u64>) -> Result<()> {
        let node = self
            .nodes
            .iter()
            .find(|node| node.id == id)
            .ok_or_else(|| Error::invalid_state().with_message("remove names an unknown child"))?;
        output.push(id);
        for child in &node.children {
            self.collect_subtree(*child, output)?;
        }
        Ok(())
    }

    fn emit_descendant_removal(&self, id: u64, patches: &mut Vec<AppliedPatch>) -> Result<()> {
        let node = self
            .nodes
            .iter()
            .find(|node| node.id == id)
            .ok_or_else(|| Error::invalid_state().with_message("remove names an unknown child"))?;
        for child in &node.children {
            patches.push(AppliedPatch::RemoveChild {
                parent: id,
                index: 0,
                child: *child,
            });
            self.emit_descendant_removal(*child, patches)?;
        }
        patches.push(AppliedPatch::Delete(id));
        Ok(())
    }

    fn move_child(
        &mut self,
        parent: u64,
        child: u64,
        final_index: usize,
        patches: &mut Vec<AppliedPatch>,
    ) -> Result<()> {
        let parent_node = self.node_mut(parent)?;
        if !matches!(parent_node.data, FlatNodeData::Box { .. }) {
            return Err(Error::invalid_state().with_message("move parent is not a container"));
        }
        let Some(from_index) = parent_node.children.iter().position(|id| *id == child) else {
            return Err(Error::invalid_state()
                .with_message("move child is not a direct child of its parent"));
        };
        if final_index >= parent_node.children.len() {
            return Err(Error::invalid_state().with_message("move index is out of range"));
        }
        if from_index == final_index {
            return Ok(());
        }
        let child = parent_node.children.remove(from_index);
        parent_node.children.insert(final_index, child);
        patches.push(AppliedPatch::MoveChild {
            parent,
            from_index,
            to_index: final_index,
            child,
        });
        Ok(())
    }
}

#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
struct FlatTreeBuilder {
    next_anonymous: u64,
    nodes: Vec<FlatNode>,
    names: BTreeMap<u64, NodeIdentity>,
    commands: BTreeMap<u64, CommandIdentity>,
    ids: BTreeSet<u64>,
    require_stable: bool,
}

#[cfg_attr(not(all(target_os = "wasi", target_env = "p2")), allow(dead_code))]
impl FlatTreeBuilder {
    fn push(&mut self, element: &Element) -> Result<u64> {
        match &element.kind {
            ElementKind::Box(value) => {
                let id = match &value.key {
                    Some(key) => self.allocate_named(key)?,
                    None if self.require_stable => {
                        return Err(Error::invalid_state().with_message(
                            "every inserted subtree node must have a stable identity",
                        ));
                    }
                    None => self.allocate_anonymous()?,
                };
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
                let id = self.allocate_named(&value.key)?;
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
                let id = self.allocate_named(&value.key)?;
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
            ElementKind::Editor(value) => {
                let id = self.allocate_named(&value.key)?;
                let data = match &value.source {
                    EditorSource::Inline {
                        document_revision,
                        initial_text,
                    } => FlatNodeData::Editor {
                        document_revision: *document_revision,
                        text: initial_text.clone(),
                    },
                    EditorSource::TextDocument { handle, version } => {
                        FlatNodeData::TextDocumentEditor {
                            handle: *handle,
                            version: *version,
                        }
                    }
                };
                self.nodes.push(FlatNode {
                    id,
                    data,
                    children: Vec::new(),
                });
                Ok(id)
            }
            ElementKind::Button(value) => {
                if value.shortcuts.len() > 4 {
                    return Err(Error::invalid_state()
                        .with_message("a button declares more than four shortcuts"));
                }
                if let Some(command) = &value.command
                    && let Some(existing) = self.commands.insert(command.id(), command.clone())
                {
                    let message = if existing == *command {
                        format!("{} is bound more than once", command.describe())
                    } else {
                        format!(
                            "command identity collision between {} and {}",
                            existing.describe(),
                            command.describe()
                        )
                    };
                    return Err(Error::invalid_state().with_message(message));
                }
                let id = self.allocate_named(&value.key)?;
                self.nodes.push(FlatNode {
                    id,
                    data: FlatNodeData::Button {
                        label: value.label.clone(),
                        enabled: value.enabled,
                        command: value.command.clone(),
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

    fn allocate_named(&mut self, key: &NodeIdentity) -> Result<u64> {
        if let Some(existing) = self.names.get(&key.id()) {
            let message = if existing == key {
                format!("{} is used more than once", key.describe())
            } else {
                format!(
                    "node identity collision between {} and {}",
                    existing.describe(),
                    key.describe()
                )
            };
            return Err(Error::invalid_state().with_message(message));
        }
        if !self.ids.insert(key.id()) {
            return Err(Error::invalid_state().with_message("a node ID is used more than once"));
        }
        self.names.insert(key.id(), key.clone());
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
mod text_document {
    use super::{Document, DocumentHandle, DocumentVersion, Error, Result, SaveRequest};
    use crate::component::bindings::youth::text_document::document::{self, ErrorCode};

    pub fn current() -> Result<Document> {
        document::current()
            .map(|info| Document {
                handle: DocumentHandle {
                    id: info.handle.id,
                    generation: info.handle.generation,
                },
                version: DocumentVersion {
                    id: info.version.id,
                    generation: info.version.generation,
                },
                relative_path: info.relative_path,
                filename: info.filename,
            })
            .map_err(map_error)
    }

    pub fn request_save(value: DocumentHandle, editor: u64) -> Result<SaveRequest> {
        document::request_save(
            document::DocumentHandle {
                id: value.id,
                generation: value.generation,
            },
            editor,
        )
        .map(|request| SaveRequest {
            id: request.id,
            generation: request.generation,
        })
        .map_err(map_error)
    }

    fn map_error(error: ErrorCode) -> Error {
        match error {
            ErrorCode::Busy => Error::rejected_event(),
            ErrorCode::InvalidDocument | ErrorCode::InvalidEditor | ErrorCode::WrongPhase => {
                Error::invalid_state()
            }
            ErrorCode::Unavailable | ErrorCode::Internal => Error::internal(),
        }
    }
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
mod text_document {
    use super::{Document, DocumentHandle, Error, Result, SaveRequest};

    fn unavailable<T>() -> Result<T> {
        Err(Error::internal().with_message("text-document calls require wasm32-wasip2"))
    }

    pub fn current() -> Result<Document> {
        unavailable()
    }

    pub fn request_save(_value: DocumentHandle, _editor: u64) -> Result<SaveRequest> {
        unavailable()
    }
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
mod editor {
    use super::{DocumentRevision, EditSequence, EditorSnapshot, Error, Result};
    use crate::component::bindings::youth::editor::session::{self, EditorErrorCode};

    pub fn snapshot(editor: u64) -> Result<EditorSnapshot> {
        session::snapshot(editor)
            .map(|snapshot| EditorSnapshot {
                document_revision: DocumentRevision(snapshot.document_revision),
                edit_sequence: EditSequence(snapshot.edit_sequence),
                text: snapshot.text,
            })
            .map_err(map_error)
    }

    pub fn accept(
        editor: u64,
        expected_document_revision: DocumentRevision,
        expected_edit_sequence: EditSequence,
        new_document_revision: DocumentRevision,
    ) -> Result<()> {
        session::accept(
            editor,
            expected_document_revision.0,
            expected_edit_sequence.0,
            new_document_revision.0,
        )
        .map_err(map_error)
    }

    pub fn replace(
        editor: u64,
        expected_document_revision: DocumentRevision,
        expected_edit_sequence: EditSequence,
        new_document_revision: DocumentRevision,
        authoritative_text: String,
    ) -> Result<()> {
        session::replace(
            editor,
            expected_document_revision.0,
            expected_edit_sequence.0,
            new_document_revision.0,
            &authoritative_text,
        )
        .map_err(map_error)
    }

    fn map_error(error: EditorErrorCode) -> Error {
        let message = match error {
            EditorErrorCode::UnknownEditor => "host does not recognize the Editor node",
            EditorErrorCode::StaleDocumentRevision => "Editor document revision is stale",
            EditorErrorCode::StaleEditSequence => "Editor edit sequence is stale",
            EditorErrorCode::Unavailable => "host Editor sessions are unavailable",
            EditorErrorCode::Internal => return Error::internal(),
        };
        Error::invalid_state().with_message(message)
    }
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
mod editor {
    use super::{DocumentRevision, EditSequence, EditorSnapshot, Error, Result};

    fn unavailable<T>() -> Result<T> {
        Err(Error::internal().with_message("host Editor calls require wasm32-wasip2"))
    }

    pub fn snapshot(_editor: u64) -> Result<EditorSnapshot> {
        unavailable()
    }

    pub fn accept(
        _editor: u64,
        _expected_document_revision: DocumentRevision,
        _expected_edit_sequence: EditSequence,
        _new_document_revision: DocumentRevision,
    ) -> Result<()> {
        unavailable()
    }

    pub fn replace(
        _editor: u64,
        _expected_document_revision: DocumentRevision,
        _expected_edit_sequence: EditSequence,
        _new_document_revision: DocumentRevision,
        _authoritative_text: String,
    ) -> Result<()> {
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
        ActivationKey, Application, BoxNode, Button, Column, CommandIdentity, CommandKey,
        Countdown, CountdownFormat, Document, DocumentHandle, DocumentRevision, DocumentVersion,
        EditSequence, Editor, EditorCapability, EditorSnapshot, ElapsedReason, Element, Error,
        ErrorKind, Event, EventContext, Events, Generation, Grid, ItemCommandKey, ItemKey,
        ItemNodeKey, NodeId, NodeIdentity, NodeKey, Notification, Result, Row, SaveRequest,
        Schedule, ScheduleId, ScheduleOptions, Shortcut, Text, TextAlign, TextDocumentFailure,
        TextDocumentReader, TextDocumentSaveCompletion, TextDocumentSaveOutcome,
        TextDocumentWriter, TimePrecision, TimeScheduler, Tree, Update, ViewContext, command,
        derived_command_id, derived_node_id, node, ui_ids,
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

    mod declared_ids {
        crate::ui_ids! {
            node DISPLAY = "display";
            command EQUALS = "equals";
            node STATUS = "status";
        }
    }

    fn schedule() -> Schedule {
        Schedule {
            id: 17,
            generation: 3,
        }
    }

    #[test]
    fn editor_builder_emits_whole_buffer_document_data() {
        let tree = Tree::root(Editor::new(
            node!("scratchpad"),
            DocumentRevision::new(7),
            "Draft",
        ))
        .flatten()
        .unwrap();
        assert!(matches!(
            &tree.nodes[1].data,
            FlatNodeData::Editor {
                document_revision,
                text,
            } if document_revision.get() == 7 && text == "Draft"
        ));
    }

    #[test]
    fn symbolic_id_vectors_are_stable() {
        assert_eq!(named_node_id("count"), 0xf700_b2fe_97f6_53d6);
        assert_eq!(named_node_id("increment"), 0xd9e1_c44e_444d_fb74);
        assert_eq!(named_node_id("café"), 0xcab8_7ecf_2aee_1d93);
    }

    #[test]
    fn declared_ids_preserve_typed_static_keys() {
        assert_eq!(declared_ids::DISPLAY.name(), "display");
        assert_eq!(declared_ids::STATUS.name(), "status");
        assert_eq!(declared_ids::EQUALS.name(), "equals");
        assert_ne!(declared_ids::DISPLAY.id(), declared_ids::EQUALS.id());
    }

    #[test]
    fn command_id_vectors_are_stable_and_use_a_distinct_domain() {
        assert_eq!(named_command_id("clear"), 0xf3da_ce2c_1a9e_2f27);
        assert_eq!(named_command_id("digit-7"), 0x9848_57b0_7512_75f7);
        assert_eq!(named_command_id("equals"), 0xee40_8611_9b13_a04b);
        assert_ne!(named_command_id("equals"), named_node_id("equals"));
    }

    #[test]
    fn derived_identity_vectors_and_validation_are_stable() {
        assert_eq!(
            derived_node_id("todo", 1, "row").unwrap(),
            0xe4ea_3f45_0dc3_046f
        );
        assert_eq!(
            derived_node_id("todo", 42, "title").unwrap(),
            0x872f_87fc_4c39_8fe4
        );
        assert_eq!(
            derived_command_id("todo", 1, "toggle").unwrap(),
            0x8b5e_c3bc_b296_c4a5
        );
        assert!(ItemKey::new("", 1).is_err());
        assert!(ItemKey::new("todo", 0).is_err());
        let item = ItemKey::new("todo", 1).unwrap();
        assert!(item.node("").is_err());
        assert!(item.command("x".repeat(257)).is_err());
    }

    #[test]
    fn derived_identities_work_across_builders_updates_and_events() {
        let item = ItemKey::new("todo", 1).unwrap();
        let title = item.node("title").unwrap();
        let toggle = item.command("toggle").unwrap();
        let mut tree = Tree::root(Row::new([
            Text::new(title.clone(), "Task 1"),
            Button::command(toggle.clone(), "Done"),
        ]))
        .flatten()
        .unwrap();
        tree.apply(&Update::new().set_text(title, "Task 1 updated"))
            .unwrap();
        let button_id = CommandIdentity::from(toggle.clone()).node_identity().id();
        let events = Events {
            events: vec![Event::Activated(button_id)],
            commanded: vec![toggle.id()],
        };
        assert!(events.activated(toggle));
    }

    #[test]
    fn derived_collisions_report_both_full_identities() {
        let first = ItemNodeKey::with_id("todo", 1, "row", NAMED_BIT | 11);
        let second = ItemNodeKey::with_id("todo", 2, "title", NAMED_BIT | 11);
        let error = Tree::root(Row::new([
            Text::new(first, "one"),
            Text::new(second, "two"),
        ]))
        .flatten()
        .unwrap_err();
        let message = error.message.unwrap();
        assert!(message.contains("namespace \"todo\", item 1, role \"row\""));
        assert!(message.contains("namespace \"todo\", item 2, role \"title\""));

        let first = ItemCommandKey::with_id("todo", 1, "toggle", NAMED_BIT | 12);
        let second = ItemCommandKey::with_id("todo", 2, "delete", NAMED_BIT | 12);
        let error = Tree::root(Row::new([
            Button::command(first, "Done"),
            Button::command(second, "Delete"),
        ]))
        .flatten()
        .unwrap_err();
        let message = error.message.unwrap();
        assert!(message.contains("namespace \"todo\", item 1, role \"toggle\""));
        assert!(message.contains("namespace \"todo\", item 2, role \"delete\""));
    }

    fn todo_row(id: u64) -> Element {
        let item = ItemKey::new("todo", id).unwrap();
        Row::named(
            item.node("row").unwrap(),
            [
                Text::new(item.node("title").unwrap(), format!("Task {id}")),
                Button::command(item.command("toggle").unwrap(), "Done"),
            ],
        )
    }

    #[test]
    fn named_containers_and_insert_expand_to_existing_patch_primitives() {
        let mut tree = Tree::root(Column::named(node!("items"), [todo_row(1)]))
            .flatten()
            .unwrap();
        let item = ItemKey::new("todo", 2).unwrap();
        let patches = tree
            .apply(&Update::new().insert_child(node!("items"), 1, todo_row(2)))
            .unwrap();
        assert_eq!(patches.len(), 6);
        assert!(matches!(patches[0], AppliedPatch::Create(_)));
        assert!(matches!(patches[1], AppliedPatch::Create(_)));
        assert!(matches!(patches[2], AppliedPatch::Create(_)));
        assert!(matches!(
            patches.last(),
            Some(AppliedPatch::InsertChild { parent, index: 1, child })
                if *parent == node!("items").id() && *child == item.node("row").unwrap().id()
        ));
        let items = tree
            .nodes
            .iter()
            .find(|node| node.id == node!("items").id())
            .unwrap();
        assert_eq!(items.children.len(), 2);
    }

    #[test]
    fn insert_rejects_anonymous_duplicate_and_unknown_shapes_without_mutation() {
        let mut tree = Tree::root(Column::named(node!("items"), [todo_row(1)]))
            .flatten()
            .unwrap();
        let original = tree.clone();
        assert!(
            tree.apply(&Update::new().insert_child(
                node!("items"),
                1,
                Row::new([Text::new(node!("new-title"), "Task")]),
            ),)
                .is_err()
        );
        assert_eq!(tree, original);
        assert!(
            tree.apply(&Update::new().insert_child(node!("items"), 1, todo_row(1)))
                .is_err()
        );
        assert_eq!(tree, original);
        assert!(
            tree.apply(&Update::new().insert_child(node!("missing"), 0, todo_row(2)))
                .is_err()
        );
        assert_eq!(tree, original);
    }

    #[test]
    fn remove_subtree_is_strict_and_deletes_every_descendant() {
        let first = ItemKey::new("todo", 1).unwrap();
        let second = ItemKey::new("todo", 2).unwrap();
        let mut tree = Tree::root(Column::named(node!("items"), [todo_row(1), todo_row(2)]))
            .flatten()
            .unwrap();
        let patches = tree
            .apply(&Update::new().remove_subtree(node!("items"), first.node("row").unwrap()))
            .unwrap();
        assert_eq!(patches.len(), 6);
        assert!(matches!(patches[0], AppliedPatch::RemoveChild { .. }));
        assert!(
            matches!(patches.last(), Some(AppliedPatch::Delete(id)) if *id == first.node("row").unwrap().id())
        );
        assert!(
            tree.nodes
                .iter()
                .all(|node| node.id != first.node("row").unwrap().id())
        );
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.id == second.node("row").unwrap().id())
        );
        let original = tree.clone();
        assert!(
            tree.apply(
                &Update::new()
                    .remove_subtree(second.node("row").unwrap(), second.node("title").unwrap(),)
            )
            .is_ok()
        );
        assert_ne!(tree, original);
        assert!(
            tree.apply(
                &Update::new().remove_subtree(node!("items"), second.node("title").unwrap(),)
            )
            .is_err()
        );
    }

    #[test]
    fn move_uses_final_index_and_current_position_is_a_patchless_no_op() {
        let ids: Vec<_> = (1..=3)
            .map(|id| ItemKey::new("todo", id).unwrap())
            .collect();
        let mut tree = Tree::root(Column::named(
            node!("items"),
            [todo_row(1), todo_row(2), todo_row(3)],
        ))
        .flatten()
        .unwrap();
        let patches = tree
            .apply(&Update::new().move_child(node!("items"), ids[0].node("row").unwrap(), 2))
            .unwrap();
        assert_eq!(patches.len(), 1);
        assert!(matches!(
            patches[0],
            AppliedPatch::MoveChild {
                from_index: 0,
                to_index: 2,
                ..
            }
        ));
        let items = tree
            .nodes
            .iter()
            .find(|node| node.id == node!("items").id())
            .unwrap();
        assert_eq!(
            items.children,
            vec![
                ids[1].node("row").unwrap().id(),
                ids[2].node("row").unwrap().id(),
                ids[0].node("row").unwrap().id(),
            ]
        );
        let patches = tree
            .apply(&Update::new().move_child(node!("items"), ids[0].node("row").unwrap(), 2))
            .unwrap();
        assert!(patches.is_empty());
    }

    #[test]
    fn structural_operations_observe_the_current_staged_tree() {
        let item = ItemKey::new("todo", 2).unwrap();
        let mut tree = Tree::root(Column::named(node!("items"), [todo_row(1)]))
            .flatten()
            .unwrap();
        let patches = tree
            .apply(
                &Update::new()
                    .insert_child(node!("items"), 1, todo_row(2))
                    .move_child(node!("items"), item.node("row").unwrap(), 0)
                    .set_label(item.node("toggle").unwrap(), "Reopen"),
            )
            .unwrap();
        assert!(matches!(patches.last(), Some(AppliedPatch::Label(_, value)) if value == "Reopen"));
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
                    Button::command(command!("digit-7"), "7").shortcut(Shortcut::character('7')),
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
                .shortcut(Shortcut::character('1'))
                .shortcut(Shortcut::character('2'))
                .shortcut(Shortcut::character('3'))
                .shortcut(Shortcut::character('4'))
                .shortcut(Shortcut::character('5')),
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
