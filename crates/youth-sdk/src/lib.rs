//! Guest-facing Rust SDK for Youth applications.
//!
//! The SDK owns the Rust WIT bindings, component export adapter, lifecycle
//! bookkeeping, semantic builders, typed state calls, and wire conversion.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;
// The suffix is the two ASCII bytes `\\` (0x5c) and `0` (0x30), not NUL.
// This spelling is locked by the public cross-language test vectors.
const NAMED_DOMAIN: &[u8] = b"youth:node-id:v1\\0";
const NAMED_BIT: u64 = 0x8000_0000_0000_0000;
const VALUE_MASK: u64 = 0x7fff_ffff_ffff_ffff;

/// Calculates the stable, app-global wire ID for an exact UTF-8 node name.
#[must_use]
pub const fn named_node_id(name: &str) -> u64 {
    let mut hash = FNV_OFFSET;
    let mut index = 0;
    while index < NAMED_DOMAIN.len() {
        hash ^= NAMED_DOMAIN[index] as u64;
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
}

impl Text {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(key: NodeKey, value: impl Into<String>) -> Element {
        Element {
            kind: ElementKind::Text(TextElement {
                key,
                value: value.into(),
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
}

impl BoxNode {
    #[must_use]
    pub fn column(children: impl IntoIterator<Item = Element>) -> Element {
        Element {
            kind: ElementKind::Box(BoxElement {
                children: children.into_iter().collect(),
                enabled: true,
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
    Button(ButtonElement),
}

impl Element {
    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        match &mut self.kind {
            ElementKind::Box(value) => value.enabled = enabled,
            ElementKind::Button(value) => value.enabled = enabled,
            ElementKind::Text(_) => {}
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Events {
    activated: Vec<u64>,
}

impl Events {
    #[must_use]
    pub fn activated(&self, key: NodeKey) -> bool {
        self.activated.contains(&key.id())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum UpdateOperation {
    Text(NodeKey, String),
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
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EventContext;

impl EventContext {
    #[must_use]
    pub const fn state(&mut self) -> StateWriter {
        StateWriter
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
    Box { enabled: bool },
    Text { value: String },
    Button { label: String, enabled: bool },
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
                | UpdateOperation::Label(key, _)
                | UpdateOperation::Enabled(key, _) => *key,
            };
            if !changed.insert(key.id()) {
                return Err(Error::invalid_state().with_message("a node is updated twice"));
            }
            let Some(node) = self.nodes.iter_mut().find(|node| node.id == key.id()) else {
                return Err(Error::invalid_state().with_message("an update names an unknown node"));
            };
            match (operation, &mut node.data) {
                (UpdateOperation::Text(_, value), FlatNodeData::Text { value: current }) => {
                    current.clone_from(value);
                }
                (UpdateOperation::Label(_, value), FlatNodeData::Button { label, .. }) => {
                    label.clone_from(value);
                }
                (
                    UpdateOperation::Enabled(_, enabled),
                    FlatNodeData::Button { enabled: value, .. },
                )
                | (UpdateOperation::Enabled(_, enabled), FlatNodeData::Box { enabled: value }) => {
                    *value = *enabled;
                }
                _ => {
                    return Err(Error::invalid_state()
                        .with_message("an update does not match the named node type"));
                }
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
                    },
                    children: Vec::new(),
                });
                Ok(id)
            }
            ElementKind::Button(value) => {
                let id = self.allocate_named(value.key)?;
                self.nodes.push(FlatNode {
                    id,
                    data: FlatNodeData::Button {
                        label: value.label.clone(),
                        enabled: value.enabled,
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

pub mod prelude {
    pub use crate::{
        Application, BoxNode, Button, Error, ErrorKind, EventContext, Events, NodeKey, Result,
        Text, Tree, Update, ViewContext, node,
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

    #[test]
    fn symbolic_id_vectors_are_stable() {
        assert_eq!(named_node_id("count"), 0xf700_b2fe_97f6_53d6);
        assert_eq!(named_node_id("increment"), 0xd9e1_c44e_444d_fb74);
        assert_eq!(named_node_id("café"), 0xcab8_7ecf_2aee_1d93);
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
}
