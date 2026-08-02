//! Pure retained semantic-tree engine.
//!
//! This crate must not depend on Wasmtime, WASI, Tokio, rendering
//! libraries, or platform APIs. The tree correctness core is testable
//! and reusable without executing Wasm.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU64;

use serde::Serialize;
use thiserror::Error;

/// A non-zero identity for a node in a semantic tree.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct NodeId(NonZeroU64);

impl NodeId {
    /// Creates an ID, returning `None` for the reserved value zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric value of this ID.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The semantic content and behavior of a node.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoxLayout {
    Column,
    Row,
    Grid { columns: u8 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlignment {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ScheduleRef {
    pub id: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimePrecision {
    Seconds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CountdownFormat {
    MinutesSeconds,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutKey {
    Character(String),
    Enter,
    Escape,
    Backspace,
}

/// The chord modifiers a [`Shortcut`] requires to be held.
///
/// Only `primary` (Cmd on macOS, Control on Windows/Linux) is supported
/// today. This is a plain struct of named flags rather than an opaque
/// bitfield so that `shift`/`alt` can be added later as honest new fields
/// without another restructuring.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ShortcutModifiers {
    pub primary: bool,
}

impl ShortcutModifiers {
    #[must_use]
    pub const fn empty() -> Self {
        Self { primary: false }
    }

    #[must_use]
    pub const fn primary() -> Self {
        Self { primary: true }
    }
}

/// A logical key paired with the modifiers required to trigger it.
///
/// `Character("s")` and `Primary+Character("s")` are distinct chords and may
/// coexist on the same tree without conflicting.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct Shortcut {
    pub key: ShortcutKey,
    pub modifiers: ShortcutModifiers,
}

impl Shortcut {
    #[must_use]
    pub const fn new(key: ShortcutKey, modifiers: ShortcutModifiers) -> Self {
        Self { key, modifiers }
    }

    /// A shortcut with no modifiers held -- the only chord shape protocols
    /// 0.0.2 through 0.0.6 can express.
    #[must_use]
    pub const fn plain(key: ShortcutKey) -> Self {
        Self {
            key,
            modifiers: ShortcutModifiers::empty(),
        }
    }
}

/// The semantic content and behavior of a normalized node.
///
/// The compact DP0 variants remain the normalized representation for their
/// default DP1 attributes. Protocol adapters terminate at this type; layout,
/// interaction, and rendering never branch on a component protocol version.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeData {
    Root,
    Box {
        enabled: bool,
    },
    Row {
        enabled: bool,
    },
    Grid {
        enabled: bool,
        columns: u8,
    },
    Text {
        value: String,
    },
    AlignedText {
        value: String,
        alignment: TextAlignment,
    },
    Editor {
        document_revision: u64,
        text: String,
    },
    Countdown {
        schedule: ScheduleRef,
        precision: TimePrecision,
        format: CountdownFormat,
    },
    AlignedCountdown {
        schedule: ScheduleRef,
        precision: TimePrecision,
        format: CountdownFormat,
        alignment: TextAlignment,
    },
    Button {
        label: String,
        enabled: bool,
    },
    ShortcutButton {
        label: String,
        enabled: bool,
        shortcuts: Vec<Shortcut>,
    },
}

impl NodeData {
    #[must_use]
    pub const fn box_layout(&self) -> Option<BoxLayout> {
        match self {
            Self::Box { .. } => Some(BoxLayout::Column),
            Self::Row { .. } => Some(BoxLayout::Row),
            Self::Grid { columns, .. } => Some(BoxLayout::Grid { columns: *columns }),
            _ => None,
        }
    }

    #[must_use]
    pub const fn text_alignment(&self) -> Option<TextAlignment> {
        match self {
            Self::Text { .. } | Self::Editor { .. } | Self::Countdown { .. } => {
                Some(TextAlignment::Start)
            }
            Self::AlignedText { alignment, .. } | Self::AlignedCountdown { alignment, .. } => {
                Some(*alignment)
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn text_value(&self) -> Option<&str> {
        match self {
            Self::Text { value } | Self::AlignedText { value, .. } => Some(value),
            Self::Editor { text, .. } => Some(text),
            _ => None,
        }
    }

    #[must_use]
    pub const fn countdown_ref(&self) -> Option<(ScheduleRef, TimePrecision, CountdownFormat)> {
        match self {
            Self::Countdown {
                schedule,
                precision,
                format,
            }
            | Self::AlignedCountdown {
                schedule,
                precision,
                format,
                ..
            } => Some((*schedule, *precision, *format)),
            _ => None,
        }
    }

    #[must_use]
    pub fn button_label(&self) -> Option<&str> {
        match self {
            Self::Button { label, .. } | Self::ShortcutButton { label, .. } => Some(label),
            _ => None,
        }
    }

    #[must_use]
    pub fn shortcuts(&self) -> &[Shortcut] {
        match self {
            Self::ShortcutButton { shortcuts, .. } => shortcuts,
            _ => &[],
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        match self {
            Self::Box { enabled }
            | Self::Row { enabled }
            | Self::Grid { enabled, .. }
            | Self::Button { enabled, .. }
            | Self::ShortcutButton { enabled, .. } => *enabled,
            Self::Root
            | Self::Text { .. }
            | Self::AlignedText { .. }
            | Self::Editor { .. }
            | Self::Countdown { .. }
            | Self::AlignedCountdown { .. } => true,
        }
    }

    #[must_use]
    pub const fn is_button(&self) -> bool {
        matches!(self, Self::Button { .. } | Self::ShortcutButton { .. })
    }

    #[must_use]
    pub const fn is_focusable(&self) -> bool {
        self.is_button() || matches!(self, Self::Editor { .. })
    }
}

/// A semantic node and its ordered child IDs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Node {
    pub id: NodeId,
    pub data: NodeData,
    pub children: Vec<NodeId>,
}

/// The unvalidated exchange representation of a tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TreeSnapshot {
    pub revision: u64,
    pub root: NodeId,
    pub nodes: Vec<Node>,
}

/// Resource limits applied while validating a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_nodes: usize,
    pub max_depth: usize,
    pub max_children: usize,
    pub max_text_len: usize,
    /// Maximum UTF-8 byte length of one host-owned Editor buffer.
    ///
    /// Editor text is transported as a whole-buffer value at the protocol
    /// boundary, so it has a separate (larger) budget than ordinary labels.
    pub max_editor_text_len: usize,
    pub max_label_len: usize,
    pub max_patches: usize,
    pub max_grid_columns: u8,
    pub max_shortcuts_per_button: usize,
    pub max_shortcuts_per_tree: usize,
    pub max_shortcut_character_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: 10_000,
            max_depth: 64,
            max_children: 4_096,
            max_text_len: 64 * 1024,
            max_editor_text_len: 1024 * 1024,
            max_label_len: 4 * 1024,
            max_patches: 10_000,
            max_grid_columns: 16,
            max_shortcuts_per_button: 4,
            max_shortcuts_per_tree: 256,
            max_shortcut_character_bytes: 4,
        }
    }
}

/// A single ordered mutation to a retained semantic tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Patch {
    Create {
        node: Node,
    },
    Delete {
        id: NodeId,
    },
    SetText {
        id: NodeId,
        value: String,
    },
    SetCountdown {
        id: NodeId,
        schedule: ScheduleRef,
        precision: TimePrecision,
        format: CountdownFormat,
    },
    SetLabel {
        id: NodeId,
        value: String,
    },
    SetEnabled {
        id: NodeId,
        value: bool,
    },
    InsertChild {
        parent: NodeId,
        index: u32,
        child: NodeId,
    },
    RemoveChild {
        parent: NodeId,
        index: u32,
        expected_child: NodeId,
    },
    MoveChild {
        parent: NodeId,
        from_index: u32,
        to_index: u32,
        expected_child: NodeId,
    },
}

/// An atomic, revision-checked sequence of patches.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PatchBatch {
    pub base_revision: u64,
    pub next_revision: u64,
    pub patches: Vec<Patch>,
}

/// Why an atomic patch batch was rejected.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PatchError {
    #[error("patch base revision {actual} does not match live revision {expected}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("invalid revision transition from {base} to {next} for {patch_count} patches")]
    InvalidRevisionTransition {
        base: u64,
        next: u64,
        patch_count: usize,
    },
    #[error("batch has {actual} patches, exceeding the limit of {max}")]
    TooManyPatches { actual: usize, max: usize },
    #[error("patch {patch_index} creates already-existing node {id}")]
    DuplicateCreate { patch_index: usize, id: NodeId },
    #[error("patch {patch_index} creates node {id} with attached children")]
    CreateNotDetached { patch_index: usize, id: NodeId },
    #[error("patch {patch_index} attaches node {child} more than once")]
    AttachedTwice { patch_index: usize, child: NodeId },
    #[error("patch {patch_index} deletes attached node {id}")]
    DeleteAttached { patch_index: usize, id: NodeId },
    #[error("patch {patch_index} deletes non-leaf node {id}")]
    DeleteNonLeaf { patch_index: usize, id: NodeId },
    #[error("patch {patch_index} refers to unknown node {id}")]
    UnknownNode { patch_index: usize, id: NodeId },
    #[error("patch {patch_index} refers to unknown parent {parent}")]
    UnknownParent { patch_index: usize, parent: NodeId },
    #[error("patch {patch_index} expected child {expected} but found {actual}")]
    WrongExpectedChild {
        patch_index: usize,
        expected: NodeId,
        actual: NodeId,
    },
    #[error("patch {patch_index} index {index} is out of range for length {len}")]
    IndexOutOfRange {
        patch_index: usize,
        index: u32,
        len: usize,
    },
    #[error("patch {patch_index} applies an operation incompatible with node {id}")]
    WrongNodeKind { patch_index: usize, id: NodeId },
    #[error("node {id}, created by patch {patch_index}, is unreachable after the batch")]
    UnreachableAfterBatch { patch_index: usize, id: NodeId },
    #[error("staged tree validation failed: {0}")]
    Validation(#[from] ValidationError),
}

/// Why an unvalidated snapshot was rejected.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ValidationError {
    #[error("declared root {root} is not present")]
    MissingRoot { root: NodeId },
    #[error("node ID {id} occurs more than once")]
    DuplicateNodeId { id: NodeId },
    #[error("declared root {root} does not contain root data")]
    RootWrongData { root: NodeId },
    #[error("non-root node {id} contains root data")]
    NonRootRootData { id: NodeId },
    #[error("root {root} appears as a child of {parent}")]
    RootHasParent { root: NodeId, parent: NodeId },
    #[error("child {child} has parents {first_parent} and {second_parent}")]
    MultipleParents {
        child: NodeId,
        first_parent: NodeId,
        second_parent: NodeId,
    },
    #[error("child {child} occurs more than once under parent {parent}")]
    DuplicateChild { parent: NodeId, child: NodeId },
    #[error("parent {parent} refers to unknown child {child}")]
    UnknownChild { parent: NodeId, child: NodeId },
    #[error("cycle contains node {node}")]
    Cycle { node: NodeId },
    #[error("node {node} is unreachable from root")]
    Orphan { node: NodeId },
    #[error("leaf node {node} has children")]
    LeafWithChildren { node: NodeId },
    #[error("snapshot has {actual} nodes, exceeding the limit of {max}")]
    TooManyNodes { actual: usize, max: usize },
    #[error("node {node} is at depth {depth}, exceeding the limit of {max}")]
    TooDeep {
        node: NodeId,
        depth: usize,
        max: usize,
    },
    #[error("node {node} has {actual} children, exceeding the limit of {max}")]
    TooManyChildren {
        node: NodeId,
        actual: usize,
        max: usize,
    },
    #[error("text node {node} has {actual} bytes, exceeding the limit of {max}")]
    TextTooLong {
        node: NodeId,
        actual: usize,
        max: usize,
    },
    #[error("button node {node} has {actual} label bytes, exceeding the limit of {max}")]
    LabelTooLong {
        node: NodeId,
        actual: usize,
        max: usize,
    },
    #[error("grid node {node} declares {columns} columns; expected 1 through {max}")]
    InvalidGridColumns { node: NodeId, columns: u8, max: u8 },
    #[error("button node {node} declares {actual} shortcuts, exceeding the limit of {max}")]
    TooManyButtonShortcuts {
        node: NodeId,
        actual: usize,
        max: usize,
    },
    #[error("tree declares {actual} shortcuts, exceeding the limit of {max}")]
    TooManyTreeShortcuts { actual: usize, max: usize },
    #[error("button node {node} has an invalid logical character shortcut")]
    InvalidShortcutCharacter { node: NodeId },
    #[error("logical shortcut {shortcut:?} is declared by nodes {first} and {second}")]
    DuplicateShortcut {
        shortcut: Shortcut,
        first: NodeId,
        second: NodeId,
    },
}

/// A validated retained semantic tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tree {
    revision: u64,
    root: NodeId,
    nodes: BTreeMap<NodeId, Node>,
    parents: BTreeMap<NodeId, NodeId>,
    depth: usize,
}

impl Tree {
    /// Validates and retains an exchange snapshot.
    pub fn from_snapshot(snapshot: TreeSnapshot, limits: &Limits) -> Result<Self, ValidationError> {
        if snapshot.nodes.len() > limits.max_nodes {
            return Err(ValidationError::TooManyNodes {
                actual: snapshot.nodes.len(),
                max: limits.max_nodes,
            });
        }

        let mut nodes = BTreeMap::new();
        for node in snapshot.nodes {
            let id = node.id;
            if nodes.insert(id, node).is_some() {
                return Err(ValidationError::DuplicateNodeId { id });
            }
        }

        let root_node = nodes
            .get(&snapshot.root)
            .ok_or(ValidationError::MissingRoot {
                root: snapshot.root,
            })?;
        if root_node.data != NodeData::Root {
            return Err(ValidationError::RootWrongData {
                root: snapshot.root,
            });
        }

        let mut shortcuts = BTreeMap::<Shortcut, NodeId>::new();
        let mut shortcut_count = 0_usize;
        for (&id, node) in &nodes {
            if id != snapshot.root && node.data == NodeData::Root {
                return Err(ValidationError::NonRootRootData { id });
            }
            if node.children.len() > limits.max_children {
                return Err(ValidationError::TooManyChildren {
                    node: id,
                    actual: node.children.len(),
                    max: limits.max_children,
                });
            }
            match &node.data {
                NodeData::Text { value } | NodeData::AlignedText { value, .. } => {
                    if !node.children.is_empty() {
                        return Err(ValidationError::LeafWithChildren { node: id });
                    }
                    if value.len() > limits.max_text_len {
                        return Err(ValidationError::TextTooLong {
                            node: id,
                            actual: value.len(),
                            max: limits.max_text_len,
                        });
                    }
                }
                NodeData::Editor { text, .. } => {
                    if !node.children.is_empty() {
                        return Err(ValidationError::LeafWithChildren { node: id });
                    }
                    if text.len() > limits.max_editor_text_len {
                        return Err(ValidationError::TextTooLong {
                            node: id,
                            actual: text.len(),
                            max: limits.max_editor_text_len,
                        });
                    }
                }
                NodeData::Countdown { .. } | NodeData::AlignedCountdown { .. } => {
                    if !node.children.is_empty() {
                        return Err(ValidationError::LeafWithChildren { node: id });
                    }
                }
                NodeData::Button { label, .. } | NodeData::ShortcutButton { label, .. } => {
                    if !node.children.is_empty() {
                        return Err(ValidationError::LeafWithChildren { node: id });
                    }
                    if label.len() > limits.max_label_len {
                        return Err(ValidationError::LabelTooLong {
                            node: id,
                            actual: label.len(),
                            max: limits.max_label_len,
                        });
                    }
                }
                NodeData::Root | NodeData::Box { .. } | NodeData::Row { .. } => {}
                NodeData::Grid { columns, .. } => {
                    if *columns == 0 || *columns > limits.max_grid_columns {
                        return Err(ValidationError::InvalidGridColumns {
                            node: id,
                            columns: *columns,
                            max: limits.max_grid_columns,
                        });
                    }
                }
            }
            if node.data.shortcuts().len() > limits.max_shortcuts_per_button {
                return Err(ValidationError::TooManyButtonShortcuts {
                    node: id,
                    actual: node.data.shortcuts().len(),
                    max: limits.max_shortcuts_per_button,
                });
            }
            for shortcut in node.data.shortcuts() {
                shortcut_count = shortcut_count.saturating_add(1);
                if let ShortcutKey::Character(value) = &shortcut.key
                    && (value.len() > limits.max_shortcut_character_bytes
                        || value.chars().count() != 1)
                {
                    return Err(ValidationError::InvalidShortcutCharacter { node: id });
                }
                if let Some(first) = shortcuts.insert(shortcut.clone(), id) {
                    return Err(ValidationError::DuplicateShortcut {
                        shortcut: shortcut.clone(),
                        first,
                        second: id,
                    });
                }
            }
        }
        if shortcut_count > limits.max_shortcuts_per_tree {
            return Err(ValidationError::TooManyTreeShortcuts {
                actual: shortcut_count,
                max: limits.max_shortcuts_per_tree,
            });
        }

        let mut parents = BTreeMap::new();
        for (&parent, node) in &nodes {
            let mut children = BTreeSet::new();
            for &child in &node.children {
                if !children.insert(child) {
                    return Err(ValidationError::DuplicateChild { parent, child });
                }
                if !nodes.contains_key(&child) {
                    return Err(ValidationError::UnknownChild { parent, child });
                }
                if child == snapshot.root {
                    return Err(ValidationError::RootHasParent {
                        root: snapshot.root,
                        parent,
                    });
                }
                if let Some(first_parent) = parents.insert(child, parent) {
                    return Err(ValidationError::MultipleParents {
                        child,
                        first_parent,
                        second_parent: parent,
                    });
                }
            }
        }

        detect_cycle(&nodes)?;

        let mut reached = BTreeSet::new();
        let mut stack = vec![(snapshot.root, 1_usize)];
        let mut depth = 0;
        while let Some((id, node_depth)) = stack.pop() {
            reached.insert(id);
            if node_depth > limits.max_depth {
                return Err(ValidationError::TooDeep {
                    node: id,
                    depth: node_depth,
                    max: limits.max_depth,
                });
            }
            depth = depth.max(node_depth);
            let node = &nodes[&id];
            for &child in node.children.iter().rev() {
                stack.push((child, node_depth + 1));
            }
        }
        if let Some(&node) = nodes.keys().find(|id| !reached.contains(id)) {
            return Err(ValidationError::Orphan { node });
        }

        Ok(Self {
            revision: snapshot.revision,
            root: snapshot.root,
            nodes,
            parents,
            depth,
        })
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    #[must_use]
    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains_key(&id)
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    /// Applies an ordered patch batch atomically.
    ///
    /// Every mutation is made to a clone. The live tree is replaced only after
    /// all patches and full-tree validation succeed.
    pub fn apply(&mut self, batch: PatchBatch, limits: &Limits) -> Result<(), PatchError> {
        if batch.base_revision != self.revision {
            return Err(PatchError::RevisionMismatch {
                expected: self.revision,
                actual: batch.base_revision,
            });
        }
        if batch.patches.len() > limits.max_patches {
            return Err(PatchError::TooManyPatches {
                actual: batch.patches.len(),
                max: limits.max_patches,
            });
        }

        let valid_transition = if batch.patches.is_empty() {
            batch.next_revision == batch.base_revision
        } else {
            batch
                .base_revision
                .checked_add(1)
                .is_some_and(|next| batch.next_revision == next)
        };
        if !valid_transition {
            return Err(PatchError::InvalidRevisionTransition {
                base: batch.base_revision,
                next: batch.next_revision,
                patch_count: batch.patches.len(),
            });
        }

        let mut staged = self.clone();
        let mut created = BTreeMap::<NodeId, usize>::new();
        for (patch_index, patch) in batch.patches.into_iter().enumerate() {
            staged.apply_one(patch, patch_index, &mut created)?;
        }
        for (&id, &patch_index) in &created {
            if id != staged.root && !staged.parents.contains_key(&id) {
                return Err(PatchError::UnreachableAfterBatch { patch_index, id });
            }
        }

        let validated = Self::from_snapshot(
            TreeSnapshot {
                revision: batch.next_revision,
                root: staged.root,
                nodes: staged.nodes.into_values().collect(),
            },
            limits,
        )?;
        *self = validated;
        Ok(())
    }

    fn apply_one(
        &mut self,
        patch: Patch,
        patch_index: usize,
        created: &mut BTreeMap<NodeId, usize>,
    ) -> Result<(), PatchError> {
        match patch {
            Patch::Create { node } => {
                let id = node.id;
                if self.nodes.contains_key(&id) {
                    return Err(PatchError::DuplicateCreate { patch_index, id });
                }
                if !node.children.is_empty() {
                    return Err(PatchError::CreateNotDetached { patch_index, id });
                }
                self.nodes.insert(id, node);
                created.insert(id, patch_index);
            }
            Patch::Delete { id } => {
                let node = self
                    .nodes
                    .get(&id)
                    .ok_or(PatchError::UnknownNode { patch_index, id })?;
                if self.parents.contains_key(&id) {
                    return Err(PatchError::DeleteAttached { patch_index, id });
                }
                if !node.children.is_empty() {
                    return Err(PatchError::DeleteNonLeaf { patch_index, id });
                }
                self.nodes.remove(&id);
                created.remove(&id);
            }
            Patch::SetText { id, value } => {
                let node = self
                    .nodes
                    .get_mut(&id)
                    .ok_or(PatchError::UnknownNode { patch_index, id })?;
                node.data = match &node.data {
                    NodeData::Text { .. } | NodeData::Countdown { .. } => NodeData::Text { value },
                    NodeData::AlignedText { alignment, .. }
                    | NodeData::AlignedCountdown { alignment, .. } => NodeData::AlignedText {
                        value,
                        alignment: *alignment,
                    },
                    _ => return Err(PatchError::WrongNodeKind { patch_index, id }),
                };
            }
            Patch::SetCountdown {
                id,
                schedule,
                precision,
                format,
            } => {
                let node = self
                    .nodes
                    .get_mut(&id)
                    .ok_or(PatchError::UnknownNode { patch_index, id })?;
                node.data = match &node.data {
                    NodeData::Text { .. } | NodeData::Countdown { .. } => NodeData::Countdown {
                        schedule,
                        precision,
                        format,
                    },
                    NodeData::AlignedText { alignment, .. }
                    | NodeData::AlignedCountdown { alignment, .. } => NodeData::AlignedCountdown {
                        schedule,
                        precision,
                        format,
                        alignment: *alignment,
                    },
                    _ => return Err(PatchError::WrongNodeKind { patch_index, id }),
                };
            }
            Patch::SetLabel { id, value } => {
                let node = self
                    .nodes
                    .get_mut(&id)
                    .ok_or(PatchError::UnknownNode { patch_index, id })?;
                match &mut node.data {
                    NodeData::Button { label, .. } | NodeData::ShortcutButton { label, .. } => {
                        *label = value
                    }
                    _ => return Err(PatchError::WrongNodeKind { patch_index, id }),
                }
            }
            Patch::SetEnabled { id, value } => {
                let node = self
                    .nodes
                    .get_mut(&id)
                    .ok_or(PatchError::UnknownNode { patch_index, id })?;
                match &mut node.data {
                    NodeData::Box { enabled }
                    | NodeData::Row { enabled }
                    | NodeData::Grid { enabled, .. }
                    | NodeData::Button { enabled, .. }
                    | NodeData::ShortcutButton { enabled, .. } => {
                        *enabled = value;
                    }
                    NodeData::Root
                    | NodeData::Text { .. }
                    | NodeData::AlignedText { .. }
                    | NodeData::Editor { .. }
                    | NodeData::Countdown { .. }
                    | NodeData::AlignedCountdown { .. } => {
                        return Err(PatchError::WrongNodeKind { patch_index, id });
                    }
                }
            }
            Patch::InsertChild {
                parent,
                index,
                child,
            } => {
                let len = self
                    .nodes
                    .get(&parent)
                    .ok_or(PatchError::UnknownParent {
                        patch_index,
                        parent,
                    })?
                    .children
                    .len();
                if !self.nodes.contains_key(&child) {
                    return Err(PatchError::UnknownNode {
                        patch_index,
                        id: child,
                    });
                }
                if self.parents.contains_key(&child) {
                    return Err(PatchError::AttachedTwice { patch_index, child });
                }
                let index_usize = insertion_index(index, len, patch_index)?;
                let parent_node = self
                    .nodes
                    .get_mut(&parent)
                    .ok_or(PatchError::UnknownParent {
                        patch_index,
                        parent,
                    })?;
                parent_node.children.insert(index_usize, child);
                self.parents.insert(child, parent);
            }
            Patch::RemoveChild {
                parent,
                index,
                expected_child,
            } => {
                let parent_node = self
                    .nodes
                    .get_mut(&parent)
                    .ok_or(PatchError::UnknownParent {
                        patch_index,
                        parent,
                    })?;
                let index_usize = existing_index(index, parent_node.children.len(), patch_index)?;
                let actual = parent_node.children.get(index_usize).copied().ok_or(
                    PatchError::IndexOutOfRange {
                        patch_index,
                        index,
                        len: parent_node.children.len(),
                    },
                )?;
                if actual != expected_child {
                    return Err(PatchError::WrongExpectedChild {
                        patch_index,
                        expected: expected_child,
                        actual,
                    });
                }
                parent_node.children.remove(index_usize);
                self.parents.remove(&actual);
            }
            Patch::MoveChild {
                parent,
                from_index,
                to_index,
                expected_child,
            } => {
                let parent_node = self
                    .nodes
                    .get_mut(&parent)
                    .ok_or(PatchError::UnknownParent {
                        patch_index,
                        parent,
                    })?;
                let len = parent_node.children.len();
                let from = existing_index(from_index, len, patch_index)?;
                let to = existing_index(to_index, len, patch_index)?;
                let actual =
                    parent_node
                        .children
                        .get(from)
                        .copied()
                        .ok_or(PatchError::IndexOutOfRange {
                            patch_index,
                            index: from_index,
                            len,
                        })?;
                if actual != expected_child {
                    return Err(PatchError::WrongExpectedChild {
                        patch_index,
                        expected: expected_child,
                        actual,
                    });
                }
                let child = parent_node.children.remove(from);
                parent_node.children.insert(to, child);
            }
        }
        Ok(())
    }

    /// Converts the retained tree to its deterministic exchange representation.
    #[must_use]
    pub fn to_snapshot(&self) -> TreeSnapshot {
        debug_assert_eq!(self.parents.len(), self.nodes.len().saturating_sub(1));
        TreeSnapshot {
            revision: self.revision,
            root: self.root,
            nodes: self.nodes.values().cloned().collect(),
        }
    }

    /// Produces the platform-independent, human-readable fixture format.
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut output = String::new();
        let mut stack = vec![(self.root, String::new(), None)];

        while let Some((id, prefix, connector)) = stack.pop() {
            output.push_str(&prefix);
            if let Some(is_last) = connector {
                output.push_str(if is_last { "└── " } else { "├── " });
            }
            append_node_description(&mut output, &self.nodes[&id]);
            output.push('\n');

            let mut child_prefix = prefix;
            if let Some(is_last) = connector {
                child_prefix.push_str(if is_last { "    " } else { "│   " });
            }
            let children = &self.nodes[&id].children;
            for (index, &child) in children.iter().enumerate().rev() {
                stack.push((
                    child,
                    child_prefix.clone(),
                    Some(index + 1 == children.len()),
                ));
            }
        }
        output
    }
}

fn existing_index(index: u32, len: usize, patch_index: usize) -> Result<usize, PatchError> {
    let converted = usize::try_from(index).map_err(|_| PatchError::IndexOutOfRange {
        patch_index,
        index,
        len,
    })?;
    if converted >= len {
        return Err(PatchError::IndexOutOfRange {
            patch_index,
            index,
            len,
        });
    }
    Ok(converted)
}

fn insertion_index(index: u32, len: usize, patch_index: usize) -> Result<usize, PatchError> {
    let converted = usize::try_from(index).map_err(|_| PatchError::IndexOutOfRange {
        patch_index,
        index,
        len,
    })?;
    if converted > len {
        return Err(PatchError::IndexOutOfRange {
            patch_index,
            index,
            len,
        });
    }
    Ok(converted)
}

fn detect_cycle(nodes: &BTreeMap<NodeId, Node>) -> Result<(), ValidationError> {
    // 0 = unseen, 1 = active in the current DFS, 2 = completely visited.
    let mut colors = BTreeMap::<NodeId, u8>::new();
    for &start in nodes.keys() {
        if colors.get(&start).copied().unwrap_or(0) != 0 {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((id, exiting)) = stack.pop() {
            if exiting {
                colors.insert(id, 2);
                continue;
            }
            match colors.get(&id).copied().unwrap_or(0) {
                1 => return Err(ValidationError::Cycle { node: id }),
                2 => continue,
                _ => {}
            }
            colors.insert(id, 1);
            stack.push((id, true));
            for &child in nodes[&id].children.iter().rev() {
                if colors.get(&child).copied().unwrap_or(0) == 1 {
                    return Err(ValidationError::Cycle { node: child });
                }
                if colors.get(&child).copied().unwrap_or(0) == 0 {
                    stack.push((child, false));
                }
            }
        }
    }
    Ok(())
}

fn append_node_description(output: &mut String, node: &Node) {
    match &node.data {
        NodeData::Root => output.push_str("root"),
        NodeData::Box { enabled } => {
            output.push_str("box");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            if !enabled {
                output.push_str(" disabled");
            }
            return;
        }
        NodeData::Row { enabled } => {
            output.push_str("row");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            if !enabled {
                output.push_str(" disabled");
            }
            return;
        }
        NodeData::Grid { enabled, columns } => {
            output.push_str("grid");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            output.push_str(" columns=");
            output.push_str(&columns.to_string());
            if !enabled {
                output.push_str(" disabled");
            }
            return;
        }
        NodeData::Text { value } | NodeData::AlignedText { value, .. } => {
            output.push_str("text");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            output.push_str(" \"");
            output.extend(value.escape_debug());
            output.push('"');
            if let NodeData::AlignedText { alignment, .. } = &node.data {
                output.push_str(" align=");
                output.push_str(match alignment {
                    TextAlignment::Start => "start",
                    TextAlignment::Center => "center",
                    TextAlignment::End => "end",
                });
            }
            return;
        }
        NodeData::Editor {
            document_revision,
            text,
        } => {
            output.push_str("editor");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            output.push_str(" document-revision=");
            output.push_str(&document_revision.to_string());
            output.push_str(" \"");
            output.extend(text.escape_debug());
            output.push('"');
            return;
        }
        NodeData::Countdown {
            schedule,
            precision,
            format,
        }
        | NodeData::AlignedCountdown {
            schedule,
            precision,
            format,
            ..
        } => {
            output.push_str("countdown");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            output.push_str(" schedule={id=");
            output.push_str(&schedule.id.to_string());
            output.push_str(",generation=");
            output.push_str(&schedule.generation.to_string());
            output.push_str("} precision=");
            output.push_str(match precision {
                TimePrecision::Seconds => "seconds",
            });
            output.push_str(" format=");
            output.push_str(match format {
                CountdownFormat::MinutesSeconds => "minutes-seconds",
            });
            if let NodeData::AlignedCountdown { alignment, .. } = &node.data {
                output.push_str(" align=");
                output.push_str(match alignment {
                    TextAlignment::Start => "start",
                    TextAlignment::Center => "center",
                    TextAlignment::End => "end",
                });
            }
            return;
        }
        NodeData::Button { label, enabled } | NodeData::ShortcutButton { label, enabled, .. } => {
            output.push_str("button");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            output.push_str(" \"");
            output.extend(label.escape_debug());
            output.push('"');
            if !enabled {
                output.push_str(" disabled");
            }
            if !node.data.shortcuts().is_empty() {
                output.push_str(" shortcuts=");
                output.push_str(&format!("{:?}", node.data.shortcuts()));
            }
            return;
        }
    }
    output.push_str(" #");
    output.push_str(&node.id.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u64) -> NodeId {
        NodeId::new(value).unwrap()
    }

    fn node(value: u64, data: NodeData, children: &[u64]) -> Node {
        Node {
            id: id(value),
            data,
            children: children.iter().copied().map(id).collect(),
        }
    }

    fn snapshot(root: u64, nodes: Vec<Node>) -> TreeSnapshot {
        TreeSnapshot {
            revision: 7,
            root: id(root),
            nodes,
        }
    }

    fn validate(snapshot: TreeSnapshot) -> Result<Tree, ValidationError> {
        Tree::from_snapshot(snapshot, &Limits::default())
    }

    #[test]
    fn node_id_zero_is_invalid() {
        assert_eq!(NodeId::new(0), None);
        assert_eq!(NodeId::new(1).map(NodeId::get), Some(1));
    }

    #[test]
    fn valid_minimal_root() {
        let tree = validate(snapshot(1, vec![node(1, NodeData::Root, &[])])).unwrap();
        assert_eq!(tree.revision(), 7);
        assert_eq!(tree.root(), id(1));
        assert_eq!(tree.node_count(), 1);
        assert_eq!(tree.depth(), 1);
        assert!(tree.contains(id(1)));
    }

    #[test]
    fn valid_root_with_box() {
        let tree = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(2, NodeData::Box { enabled: true }, &[]),
            ],
        ))
        .unwrap();
        assert_eq!(tree.depth(), 2);
    }

    #[test]
    fn valid_nested_boxes() {
        let tree = validate(snapshot(
            1,
            vec![
                node(3, NodeData::Box { enabled: false }, &[]),
                node(1, NodeData::Root, &[2]),
                node(2, NodeData::Box { enabled: true }, &[3]),
            ],
        ))
        .unwrap();
        assert_eq!(tree.depth(), 3);
    }

    #[test]
    fn rich_presentation_nodes_validate_and_canonicalize() {
        let tree = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(
                    2,
                    NodeData::Grid {
                        enabled: true,
                        columns: 2,
                    },
                    &[3, 4],
                ),
                node(
                    3,
                    NodeData::AlignedText {
                        value: "42".into(),
                        alignment: TextAlignment::End,
                    },
                    &[],
                ),
                node(
                    4,
                    NodeData::ShortcutButton {
                        label: "=".into(),
                        enabled: true,
                        shortcuts: vec![Shortcut::plain(ShortcutKey::Enter)],
                    },
                    &[],
                ),
            ],
        ))
        .unwrap();
        assert!(tree.canonical().contains("grid #2 columns=2"));
        assert!(tree.canonical().contains("align=end"));
    }

    #[test]
    fn countdown_nodes_round_trip_through_snapshot_validation() {
        let original = snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2, 3]),
                node(
                    2,
                    NodeData::Countdown {
                        schedule: ScheduleRef {
                            id: 41,
                            generation: 7,
                        },
                        precision: TimePrecision::Seconds,
                        format: CountdownFormat::MinutesSeconds,
                    },
                    &[],
                ),
                node(
                    3,
                    NodeData::AlignedCountdown {
                        schedule: ScheduleRef {
                            id: 42,
                            generation: 8,
                        },
                        precision: TimePrecision::Seconds,
                        format: CountdownFormat::MinutesSeconds,
                        alignment: TextAlignment::Center,
                    },
                    &[],
                ),
            ],
        );

        let first = validate(original).unwrap();
        let canonical_snapshot = first.to_snapshot();
        let second = validate(canonical_snapshot.clone()).unwrap();

        assert_eq!(second.to_snapshot(), canonical_snapshot);
        assert_eq!(second.canonical(), first.canonical());
    }

    #[test]
    fn editor_node_round_trips_with_document_revision_and_stable_canonical_output() {
        let original = snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(
                    2,
                    NodeData::Editor {
                        document_revision: 42,
                        text: "Scratchpad draft".into(),
                    },
                    &[],
                ),
            ],
        );

        let first = validate(original).unwrap();
        let canonical_snapshot = first.to_snapshot();
        let second = validate(canonical_snapshot.clone()).unwrap();

        assert_eq!(second.to_snapshot(), canonical_snapshot);
        assert_eq!(
            second.node(id(2)).unwrap().data,
            NodeData::Editor {
                document_revision: 42,
                text: "Scratchpad draft".into(),
            }
        );
        assert_eq!(
            first.canonical(),
            "root #1\n└── editor #2 document-revision=42 \"Scratchpad draft\"\n"
        );
        assert_eq!(second.canonical(), first.canonical());
    }

    #[test]
    fn countdown_text_alignment_is_never_missing() {
        let countdown = NodeData::Countdown {
            schedule: ScheduleRef {
                id: 41,
                generation: 7,
            },
            precision: TimePrecision::Seconds,
            format: CountdownFormat::MinutesSeconds,
        };
        let aligned = NodeData::AlignedCountdown {
            schedule: ScheduleRef {
                id: 42,
                generation: 8,
            },
            precision: TimePrecision::Seconds,
            format: CountdownFormat::MinutesSeconds,
            alignment: TextAlignment::End,
        };

        assert_eq!(countdown.text_alignment(), Some(TextAlignment::Start));
        assert_eq!(aligned.text_alignment(), Some(TextAlignment::End));
        assert_eq!(countdown.text_value(), None);
        assert_eq!(aligned.text_value(), None);
        assert!(countdown.enabled());
        assert!(aligned.enabled());
    }

    #[test]
    fn countdown_ref_only_matches_countdown_variants() {
        let expected = (
            ScheduleRef {
                id: 41,
                generation: 7,
            },
            TimePrecision::Seconds,
            CountdownFormat::MinutesSeconds,
        );
        let countdown = NodeData::Countdown {
            schedule: expected.0,
            precision: expected.1,
            format: expected.2,
        };
        let aligned = NodeData::AlignedCountdown {
            schedule: expected.0,
            precision: expected.1,
            format: expected.2,
            alignment: TextAlignment::Center,
        };

        assert_eq!(countdown.countdown_ref(), Some(expected));
        assert_eq!(aligned.countdown_ref(), Some(expected));

        let other_variants = [
            NodeData::Root,
            NodeData::Box { enabled: true },
            NodeData::Row { enabled: true },
            NodeData::Grid {
                enabled: true,
                columns: 2,
            },
            NodeData::Text {
                value: "literal".into(),
            },
            NodeData::AlignedText {
                value: "literal".into(),
                alignment: TextAlignment::End,
            },
            NodeData::Editor {
                document_revision: 42,
                text: "draft".into(),
            },
            NodeData::Button {
                label: "button".into(),
                enabled: true,
            },
            NodeData::ShortcutButton {
                label: "shortcut".into(),
                enabled: true,
                shortcuts: vec![Shortcut::plain(ShortcutKey::Enter)],
            },
        ];
        for data in other_variants {
            assert_eq!(data.countdown_ref(), None);
        }
    }

    #[test]
    fn invalid_grid_and_shortcut_contracts_are_rejected() {
        assert_eq!(Limits::default().max_shortcut_character_bytes, 4);

        let invalid_grid = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(
                    2,
                    NodeData::Grid {
                        enabled: true,
                        columns: 0,
                    },
                    &[],
                ),
            ],
        ));
        assert!(matches!(
            invalid_grid,
            Err(ValidationError::InvalidGridColumns { columns: 0, .. })
        ));

        let duplicate = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2, 3]),
                node(
                    2,
                    NodeData::ShortcutButton {
                        label: "First".into(),
                        enabled: true,
                        shortcuts: vec![Shortcut::plain(ShortcutKey::Character("7".into()))],
                    },
                    &[],
                ),
                node(
                    3,
                    NodeData::ShortcutButton {
                        label: "Second".into(),
                        enabled: false,
                        shortcuts: vec![Shortcut::plain(ShortcutKey::Character("7".into()))],
                    },
                    &[],
                ),
            ],
        ));
        assert!(matches!(
            duplicate,
            Err(ValidationError::DuplicateShortcut { .. })
        ));

        let multiple_scalars = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(
                    2,
                    NodeData::ShortcutButton {
                        label: "Bad".into(),
                        enabled: true,
                        shortcuts: vec![Shortcut::plain(ShortcutKey::Character("ab".into()))],
                    },
                    &[],
                ),
            ],
        ));
        assert!(matches!(
            multiple_scalars,
            Err(ValidationError::InvalidShortcutCharacter { .. })
        ));

        let four_byte_scalar = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(
                    2,
                    NodeData::ShortcutButton {
                        label: "Abacus".into(),
                        enabled: true,
                        shortcuts: vec![Shortcut::plain(ShortcutKey::Character("🧮".into()))],
                    },
                    &[],
                ),
            ],
        ));
        assert!(four_byte_scalar.is_ok());
    }

    #[test]
    fn plain_and_primary_shortcuts_for_the_same_key_coexist() {
        let tree = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(
                    2,
                    NodeData::ShortcutButton {
                        label: "Save".into(),
                        enabled: true,
                        shortcuts: vec![
                            Shortcut::plain(ShortcutKey::Character("s".into())),
                            Shortcut::new(
                                ShortcutKey::Character("s".into()),
                                ShortcutModifiers::primary(),
                            ),
                        ],
                    },
                    &[],
                ),
            ],
        ));
        assert!(tree.is_ok(), "{tree:?}");
    }

    #[test]
    fn identical_primary_shortcut_chords_on_two_buttons_are_rejected() {
        let duplicate = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2, 3]),
                node(
                    2,
                    NodeData::ShortcutButton {
                        label: "First".into(),
                        enabled: true,
                        shortcuts: vec![Shortcut::new(
                            ShortcutKey::Character("s".into()),
                            ShortcutModifiers::primary(),
                        )],
                    },
                    &[],
                ),
                node(
                    3,
                    NodeData::ShortcutButton {
                        label: "Second".into(),
                        enabled: false,
                        shortcuts: vec![Shortcut::new(
                            ShortcutKey::Character("s".into()),
                            ShortcutModifiers::primary(),
                        )],
                    },
                    &[],
                ),
            ],
        ));
        assert!(matches!(
            duplicate,
            Err(ValidationError::DuplicateShortcut { .. })
        ));
    }

    #[test]
    fn valid_text_and_button_leaves() {
        validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2, 3]),
                node(
                    2,
                    NodeData::Text {
                        value: "hello".into(),
                    },
                    &[],
                ),
                node(
                    3,
                    NodeData::Button {
                        label: "go".into(),
                        enabled: true,
                    },
                    &[],
                ),
            ],
        ))
        .unwrap();
    }

    #[test]
    fn rejects_missing_root() {
        assert!(matches!(
            validate(snapshot(1, vec![node(2, NodeData::Root, &[])])),
            Err(ValidationError::MissingRoot { root }) if root == id(1)
        ));
    }

    #[test]
    fn rejects_duplicate_id() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![node(1, NodeData::Root, &[]), node(1, NodeData::Root, &[])]
            )),
            Err(ValidationError::DuplicateNodeId { id: duplicate }) if duplicate == id(1)
        ));
    }

    #[test]
    fn rejects_multiple_roots() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![node(1, NodeData::Root, &[2]), node(2, NodeData::Root, &[])]
            )),
            Err(ValidationError::NonRootRootData { id: extra }) if extra == id(2)
        ));
    }

    #[test]
    fn rejects_root_with_parent() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[]),
                    node(2, NodeData::Box { enabled: true }, &[1]),
                ]
            )),
            Err(ValidationError::RootHasParent { root, parent })
                if root == id(1) && parent == id(2)
        ));
    }

    #[test]
    fn rejects_orphan() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[]),
                    node(2, NodeData::Box { enabled: true }, &[]),
                ]
            )),
            Err(ValidationError::Orphan { node: orphan }) if orphan == id(2)
        ));
    }

    #[test]
    fn rejects_unknown_child() {
        assert!(matches!(
            validate(snapshot(1, vec![node(1, NodeData::Root, &[2])])),
            Err(ValidationError::UnknownChild { parent, child })
                if parent == id(1) && child == id(2)
        ));
    }

    #[test]
    fn rejects_duplicate_child() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2, 2]),
                    node(2, NodeData::Box { enabled: true }, &[]),
                ]
            )),
            Err(ValidationError::DuplicateChild { parent, child })
                if parent == id(1) && child == id(2)
        ));
    }

    #[test]
    fn rejects_multiple_parents() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2, 3]),
                    node(2, NodeData::Box { enabled: true }, &[4]),
                    node(3, NodeData::Box { enabled: true }, &[4]),
                    node(4, NodeData::Box { enabled: true }, &[]),
                ]
            )),
            Err(ValidationError::MultipleParents { child, .. }) if child == id(4)
        ));
    }

    #[test]
    fn rejects_cycle() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[]),
                    node(2, NodeData::Box { enabled: true }, &[3]),
                    node(3, NodeData::Box { enabled: true }, &[2]),
                ]
            )),
            Err(ValidationError::Cycle { .. })
        ));
    }

    #[test]
    fn rejects_text_with_children() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(2, NodeData::Text { value: "x".into() }, &[3]),
                    node(3, NodeData::Box { enabled: true }, &[]),
                ]
            )),
            Err(ValidationError::LeafWithChildren { node: leaf }) if leaf == id(2)
        ));
    }

    #[test]
    fn rejects_editor_with_children() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(
                        2,
                        NodeData::Editor {
                            document_revision: 9,
                            text: "draft".into(),
                        },
                        &[3],
                    ),
                    node(3, NodeData::Box { enabled: true }, &[]),
                ]
            )),
            Err(ValidationError::LeafWithChildren { node: leaf }) if leaf == id(2)
        ));
    }

    #[test]
    fn rejects_button_with_children() {
        assert!(matches!(
            validate(snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(
                        2,
                        NodeData::Button {
                            label: "x".into(),
                            enabled: true,
                        },
                        &[3],
                    ),
                    node(3, NodeData::Box { enabled: true }, &[]),
                ]
            )),
            Err(ValidationError::LeafWithChildren { node: leaf }) if leaf == id(2)
        ));
    }

    #[test]
    fn rejects_excessive_depth() {
        let limits = Limits {
            max_depth: 2,
            ..Limits::default()
        };
        let result = Tree::from_snapshot(
            snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(2, NodeData::Box { enabled: true }, &[3]),
                    node(3, NodeData::Box { enabled: true }, &[]),
                ],
            ),
            &limits,
        );
        assert!(matches!(
            result,
            Err(ValidationError::TooDeep { node: deep, depth: 3, max: 2 }) if deep == id(3)
        ));
    }

    #[test]
    fn rejects_excessive_strings() {
        let limits = Limits {
            max_text_len: 2,
            max_label_len: 2,
            ..Limits::default()
        };
        let text = Tree::from_snapshot(
            snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(
                        2,
                        NodeData::Text {
                            value: "abc".into(),
                        },
                        &[],
                    ),
                ],
            ),
            &limits,
        );
        assert!(matches!(text, Err(ValidationError::TextTooLong { .. })));

        let label = Tree::from_snapshot(
            snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(
                        2,
                        NodeData::Button {
                            label: "abc".into(),
                            enabled: true,
                        },
                        &[],
                    ),
                ],
            ),
            &limits,
        );
        assert!(matches!(label, Err(ValidationError::LabelTooLong { .. })));
    }

    #[test]
    fn rejects_overlong_editor_text() {
        let limits = Limits {
            max_editor_text_len: 2,
            ..Limits::default()
        };
        let editor = Tree::from_snapshot(
            snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(
                        2,
                        NodeData::Editor {
                            document_revision: 11,
                            text: "abc".into(),
                        },
                        &[],
                    ),
                ],
            ),
            &limits,
        );
        assert!(matches!(
            editor,
            Err(ValidationError::TextTooLong {
                node,
                actual: 3,
                max: 2,
            }) if node == id(2)
        ));
    }

    #[test]
    fn string_limits_count_utf8_bytes() {
        let limits = Limits {
            max_text_len: 1,
            ..Limits::default()
        };
        let result = Tree::from_snapshot(
            snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(2, NodeData::Text { value: "é".into() }, &[]),
                ],
            ),
            &limits,
        );
        assert!(matches!(
            result,
            Err(ValidationError::TextTooLong { actual: 2, .. })
        ));
    }

    #[test]
    fn rejects_excessive_node_count() {
        let limits = Limits {
            max_nodes: 1,
            ..Limits::default()
        };
        let result = Tree::from_snapshot(
            snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2]),
                    node(2, NodeData::Box { enabled: true }, &[]),
                ],
            ),
            &limits,
        );
        assert!(matches!(
            result,
            Err(ValidationError::TooManyNodes { actual: 2, max: 1 })
        ));
    }

    #[test]
    fn rejects_excessive_child_count() {
        let limits = Limits {
            max_children: 1,
            ..Limits::default()
        };
        let result = Tree::from_snapshot(
            snapshot(
                1,
                vec![
                    node(1, NodeData::Root, &[2, 3]),
                    node(2, NodeData::Box { enabled: true }, &[]),
                    node(3, NodeData::Box { enabled: true }, &[]),
                ],
            ),
            &limits,
        );
        assert!(matches!(
            result,
            Err(ValidationError::TooManyChildren {
                actual: 2,
                max: 1,
                ..
            })
        ));
    }

    #[test]
    fn canonical_output_is_exact() {
        let tree = validate(snapshot(
            1,
            vec![
                node(
                    4,
                    NodeData::Button {
                        label: "Increment".into(),
                        enabled: true,
                    },
                    &[],
                ),
                node(1, NodeData::Root, &[2]),
                node(2, NodeData::Box { enabled: true }, &[3, 4]),
                node(
                    3,
                    NodeData::Text {
                        value: "Count: 1".into(),
                    },
                    &[],
                ),
            ],
        ))
        .unwrap();
        assert_eq!(
            tree.canonical(),
            "root #1\n└── box #2\n    ├── text #3 \"Count: 1\"\n    └── button #4 \"Increment\"\n"
        );
    }

    #[test]
    fn canonical_escapes_and_marks_disabled_nodes() {
        let tree = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(2, NodeData::Box { enabled: false }, &[3]),
                node(
                    3,
                    NodeData::Button {
                        label: "say \"hi\"\n".into(),
                        enabled: false,
                    },
                    &[],
                ),
            ],
        ))
        .unwrap();
        assert_eq!(
            tree.canonical(),
            "root #1\n└── box #2 disabled\n    └── button #3 \"say \\\"hi\\\"\\n\" disabled\n"
        );
    }

    #[test]
    fn snapshot_round_trip_is_deterministic() {
        let original = snapshot(
            1,
            vec![
                node(3, NodeData::Box { enabled: true }, &[]),
                node(1, NodeData::Root, &[3, 2]),
                node(2, NodeData::Box { enabled: true }, &[]),
            ],
        );
        let first = validate(original).unwrap();
        let canonical_snapshot = first.to_snapshot();
        assert_eq!(
            canonical_snapshot
                .nodes
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
            vec![id(1), id(2), id(3)]
        );
        assert_eq!(canonical_snapshot.nodes[0].children, vec![id(3), id(2)]);
        let second = validate(canonical_snapshot.clone()).unwrap();
        assert_eq!(second.to_snapshot(), canonical_snapshot);
        assert_eq!(second.canonical(), first.canonical());
    }

    #[test]
    fn countdown_canonical_output_is_stable_and_reference_only() {
        let tree = validate(snapshot(
            1,
            vec![
                node(1, NodeData::Root, &[2]),
                node(
                    2,
                    NodeData::Countdown {
                        schedule: ScheduleRef {
                            id: 123,
                            generation: 9,
                        },
                        precision: TimePrecision::Seconds,
                        format: CountdownFormat::MinutesSeconds,
                    },
                    &[],
                ),
            ],
        ))
        .unwrap();
        let expected = "root #1\n└── countdown #2 schedule={id=123,generation=9} precision=seconds format=minutes-seconds\n";

        assert_eq!(tree.canonical(), expected);
        assert_eq!(tree.canonical(), expected);
        assert!(!tree.canonical().contains("00:"));
    }

    fn patch_tree(nodes: Vec<Node>) -> Tree {
        validate(snapshot(1, nodes)).unwrap()
    }

    fn batch(patches: Vec<Patch>) -> PatchBatch {
        PatchBatch {
            base_revision: 7,
            next_revision: if patches.is_empty() { 7 } else { 8 },
            patches,
        }
    }

    #[test]
    fn valid_create_attach_batch() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        tree.apply(
            batch(vec![
                Patch::Create {
                    node: node(
                        2,
                        NodeData::Text {
                            value: "hello".into(),
                        },
                        &[],
                    ),
                },
                Patch::InsertChild {
                    parent: id(1),
                    index: 0,
                    child: id(2),
                },
            ]),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(tree.revision(), 8);
        assert_eq!(tree.node(id(1)).unwrap().children, vec![id(2)]);
    }

    #[test]
    fn valid_detach_delete_batch() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2]),
            node(2, NodeData::Box { enabled: true }, &[]),
        ]);
        tree.apply(
            batch(vec![
                Patch::RemoveChild {
                    parent: id(1),
                    index: 0,
                    expected_child: id(2),
                },
                Patch::Delete { id: id(2) },
            ]),
            &Limits::default(),
        )
        .unwrap();
        assert!(!tree.contains(id(2)));
        assert_eq!(tree.node_count(), 1);
    }

    #[test]
    fn valid_reorder_with_move_child() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2, 3, 4]),
            node(2, NodeData::Box { enabled: true }, &[]),
            node(3, NodeData::Box { enabled: true }, &[]),
            node(4, NodeData::Box { enabled: true }, &[]),
        ]);
        tree.apply(
            batch(vec![Patch::MoveChild {
                parent: id(1),
                from_index: 0,
                to_index: 2,
                expected_child: id(2),
            }]),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(
            tree.node(id(1)).unwrap().children,
            vec![id(3), id(4), id(2)]
        );
    }

    #[test]
    fn valid_empty_batch_is_no_op() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let before = tree.clone();
        tree.apply(batch(vec![]), &Limits::default()).unwrap();
        assert_eq!(tree, before);
    }

    #[test]
    fn text_and_countdown_patches_retarget_with_alignment_preserved() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2, 3]),
            node(
                2,
                NodeData::Text {
                    value: "start".into(),
                },
                &[],
            ),
            node(
                3,
                NodeData::AlignedText {
                    value: "aligned".into(),
                    alignment: TextAlignment::End,
                },
                &[],
            ),
        ]);
        let schedule = ScheduleRef {
            id: 123,
            generation: 9,
        };

        tree.apply(
            PatchBatch {
                base_revision: 7,
                next_revision: 8,
                patches: vec![
                    Patch::SetCountdown {
                        id: id(2),
                        schedule,
                        precision: TimePrecision::Seconds,
                        format: CountdownFormat::MinutesSeconds,
                    },
                    Patch::SetCountdown {
                        id: id(3),
                        schedule,
                        precision: TimePrecision::Seconds,
                        format: CountdownFormat::MinutesSeconds,
                    },
                ],
            },
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(
            tree.node(id(2)).unwrap().data,
            NodeData::Countdown {
                schedule,
                precision: TimePrecision::Seconds,
                format: CountdownFormat::MinutesSeconds,
            }
        );
        assert_eq!(
            tree.node(id(3)).unwrap().data,
            NodeData::AlignedCountdown {
                schedule,
                precision: TimePrecision::Seconds,
                format: CountdownFormat::MinutesSeconds,
                alignment: TextAlignment::End,
            }
        );

        tree.apply(
            PatchBatch {
                base_revision: 8,
                next_revision: 9,
                patches: vec![
                    Patch::SetText {
                        id: id(2),
                        value: "literal".into(),
                    },
                    Patch::SetText {
                        id: id(3),
                        value: "aligned literal".into(),
                    },
                ],
            },
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(
            tree.node(id(2)).unwrap().data,
            NodeData::Text {
                value: "literal".into(),
            }
        );
        assert_eq!(
            tree.node(id(3)).unwrap().data,
            NodeData::AlignedText {
                value: "aligned literal".into(),
                alignment: TextAlignment::End,
            }
        );
    }

    #[test]
    fn rejects_wrong_base_revision() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            PatchBatch {
                base_revision: 6,
                next_revision: 6,
                patches: vec![],
            },
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::RevisionMismatch {
                expected: 7,
                actual: 6
            })
        ));
    }

    #[test]
    fn rejects_skipped_next_revision() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            PatchBatch {
                base_revision: 7,
                next_revision: 9,
                patches: vec![Patch::SetEnabled {
                    id: id(1),
                    value: true,
                }],
            },
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::InvalidRevisionTransition { .. })
        ));
    }

    #[test]
    fn rejects_nonempty_batch_without_revision_increment() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            PatchBatch {
                base_revision: 7,
                next_revision: 7,
                patches: vec![Patch::Delete { id: id(1) }],
            },
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::InvalidRevisionTransition { .. })
        ));
    }

    #[test]
    fn rejects_empty_batch_with_revision_increment() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            PatchBatch {
                base_revision: 7,
                next_revision: 8,
                patches: vec![],
            },
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::InvalidRevisionTransition { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_create() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            batch(vec![Patch::Create {
                node: node(1, NodeData::Box { enabled: true }, &[]),
            }]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::DuplicateCreate { patch_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_create_with_pre_attached_children() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            batch(vec![Patch::Create {
                node: node(2, NodeData::Box { enabled: true }, &[1]),
            }]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::CreateNotDetached { patch_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_too_many_patches() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let limits = Limits {
            max_patches: 0,
            ..Limits::default()
        };
        let result = tree.apply(batch(vec![Patch::Delete { id: id(1) }]), &limits);
        assert!(matches!(
            result,
            Err(PatchError::TooManyPatches { actual: 1, max: 0 })
        ));
    }

    #[test]
    fn rejects_delete_attached_node() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2]),
            node(2, NodeData::Box { enabled: true }, &[]),
        ]);
        let result = tree.apply(batch(vec![Patch::Delete { id: id(2) }]), &Limits::default());
        assert!(matches!(
            result,
            Err(PatchError::DeleteAttached { patch_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_delete_non_leaf() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2]),
            node(2, NodeData::Box { enabled: true }, &[3]),
            node(3, NodeData::Text { value: "x".into() }, &[]),
        ]);
        let result = tree.apply(
            batch(vec![
                Patch::RemoveChild {
                    parent: id(1),
                    index: 0,
                    expected_child: id(2),
                },
                Patch::Delete { id: id(2) },
            ]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::DeleteNonLeaf { patch_index: 1, .. })
        ));
    }

    #[test]
    fn rejects_insert_unknown_child() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            batch(vec![Patch::InsertChild {
                parent: id(1),
                index: 0,
                child: id(2),
            }]),
            &Limits::default(),
        );
        assert!(
            matches!(result, Err(PatchError::UnknownNode { patch_index: 0, id: unknown }) if unknown == id(2))
        );
    }

    #[test]
    fn rejects_insert_unknown_parent() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            batch(vec![Patch::InsertChild {
                parent: id(2),
                index: 0,
                child: id(1),
            }]),
            &Limits::default(),
        );
        assert!(
            matches!(result, Err(PatchError::UnknownParent { patch_index: 0, parent }) if parent == id(2))
        );
    }

    #[test]
    fn rejects_remove_wrong_expected_child() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2]),
            node(2, NodeData::Box { enabled: true }, &[]),
        ]);
        let result = tree.apply(
            batch(vec![Patch::RemoveChild {
                parent: id(1),
                index: 0,
                expected_child: id(3),
            }]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::WrongExpectedChild { patch_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_move_wrong_expected_child() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2, 3]),
            node(2, NodeData::Box { enabled: true }, &[]),
            node(3, NodeData::Box { enabled: true }, &[]),
        ]);
        let result = tree.apply(
            batch(vec![Patch::MoveChild {
                parent: id(1),
                from_index: 0,
                to_index: 1,
                expected_child: id(3),
            }]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::WrongExpectedChild { patch_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_invalid_index() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            batch(vec![Patch::RemoveChild {
                parent: id(1),
                index: u32::MAX,
                expected_child: id(1),
            }]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::IndexOutOfRange { patch_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_set_text_on_button() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2]),
            node(
                2,
                NodeData::Button {
                    label: "go".into(),
                    enabled: true,
                },
                &[],
            ),
        ]);
        let result = tree.apply(
            batch(vec![Patch::SetText {
                id: id(2),
                value: "bad".into(),
            }]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::WrongNodeKind { patch_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_set_countdown_on_button_like_set_text() {
        let nodes = vec![
            node(1, NodeData::Root, &[2]),
            node(
                2,
                NodeData::Button {
                    label: "go".into(),
                    enabled: true,
                },
                &[],
            ),
        ];
        let mut countdown_tree = patch_tree(nodes.clone());
        let countdown_result = countdown_tree.apply(
            batch(vec![Patch::SetCountdown {
                id: id(2),
                schedule: ScheduleRef {
                    id: 123,
                    generation: 9,
                },
                precision: TimePrecision::Seconds,
                format: CountdownFormat::MinutesSeconds,
            }]),
            &Limits::default(),
        );
        let mut text_tree = patch_tree(nodes);
        let text_result = text_tree.apply(
            batch(vec![Patch::SetText {
                id: id(2),
                value: "bad".into(),
            }]),
            &Limits::default(),
        );

        assert_eq!(
            countdown_result,
            Err(PatchError::WrongNodeKind {
                patch_index: 0,
                id: id(2),
            })
        );
        assert_eq!(countdown_result, text_result);
    }

    #[test]
    fn rejects_set_label_on_text() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2]),
            node(
                2,
                NodeData::Text {
                    value: "hello".into(),
                },
                &[],
            ),
        ]);
        let result = tree.apply(
            batch(vec![Patch::SetLabel {
                id: id(2),
                value: "bad".into(),
            }]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::WrongNodeKind { patch_index: 0, .. })
        ));
    }

    #[test]
    fn rejects_attach_node_twice() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            batch(vec![
                Patch::Create {
                    node: node(2, NodeData::Box { enabled: true }, &[]),
                },
                Patch::InsertChild {
                    parent: id(1),
                    index: 0,
                    child: id(2),
                },
                Patch::InsertChild {
                    parent: id(1),
                    index: 1,
                    child: id(2),
                },
            ]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::AttachedTwice { patch_index: 2, .. })
        ));
    }

    #[test]
    fn rejects_created_unreachable_node() {
        let mut tree = patch_tree(vec![node(1, NodeData::Root, &[])]);
        let result = tree.apply(
            batch(vec![Patch::Create {
                node: node(2, NodeData::Box { enabled: true }, &[]),
            }]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::UnreachableAfterBatch { patch_index: 0, .. })
        ));
    }

    #[test]
    fn wraps_complete_staged_tree_validation_failure() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2]),
            node(
                2,
                NodeData::Text {
                    value: "leaf".into(),
                },
                &[],
            ),
        ]);
        let result = tree.apply(
            batch(vec![
                Patch::Create {
                    node: node(3, NodeData::Box { enabled: true }, &[]),
                },
                Patch::InsertChild {
                    parent: id(2),
                    index: 0,
                    child: id(3),
                },
            ]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::Validation(ValidationError::LeafWithChildren { node: leaf }))
                if leaf == id(2)
        ));
    }

    #[test]
    fn failed_batch_is_atomic_including_revision_and_canonical_output() {
        let mut tree = patch_tree(vec![
            node(1, NodeData::Root, &[2]),
            node(
                2,
                NodeData::Text {
                    value: "before".into(),
                },
                &[],
            ),
        ]);
        let before_tree = tree.clone();
        let before_revision = tree.revision();
        let before_canonical = tree.canonical();
        let result = tree.apply(
            batch(vec![
                Patch::SetText {
                    id: id(2),
                    value: "after".into(),
                },
                Patch::SetLabel {
                    id: id(2),
                    value: "invalid".into(),
                },
            ]),
            &Limits::default(),
        );
        assert!(matches!(
            result,
            Err(PatchError::WrongNodeKind { patch_index: 1, .. })
        ));
        assert_eq!(tree, before_tree);
        assert_eq!(tree.revision(), before_revision);
        assert_eq!(tree.canonical(), before_canonical);
    }

    mod property_tests {
        use super::*;
        use proptest::prelude::*;

        fn arbitrary_string() -> impl Strategy<Value = String> {
            prop::collection::vec(any::<char>(), 0..=128)
                .prop_map(|chars| chars.into_iter().collect())
        }

        fn arbitrary_node() -> impl Strategy<Value = Node> {
            (
                1_u64..=8,
                0_u8..4,
                arbitrary_string(),
                any::<bool>(),
                prop::collection::vec(1_u64..=8, 0..=16),
            )
                .prop_map(|(value, kind, string, enabled, children)| Node {
                    id: id(value),
                    data: match kind {
                        0 => NodeData::Root,
                        1 => NodeData::Box { enabled },
                        2 => NodeData::Text { value: string },
                        _ => NodeData::Button {
                            label: string,
                            enabled,
                        },
                    },
                    children: children.into_iter().map(id).collect(),
                })
        }

        fn arbitrary_snapshot() -> impl Strategy<Value = TreeSnapshot> {
            (
                any::<u64>(),
                1_u64..=8,
                prop::collection::vec(arbitrary_node(), 0..=16),
            )
                .prop_map(|(revision, root, nodes)| TreeSnapshot {
                    revision,
                    root: id(root),
                    nodes,
                })
        }

        fn arbitrary_valid_snapshot() -> impl Strategy<Value = TreeSnapshot> {
            prop::collection::vec((0_u8..3, arbitrary_string(), any::<bool>()), 0..=7).prop_map(
                |entries| {
                    let children = (2_u64..).take(entries.len()).map(id).collect::<Vec<_>>();
                    let mut nodes = vec![Node {
                        id: id(1),
                        data: NodeData::Root,
                        children,
                    }];
                    nodes.extend(entries.into_iter().enumerate().map(
                        |(index, (kind, string, enabled))| Node {
                            id: id(u64::try_from(index).unwrap_or(0) + 2),
                            data: match kind {
                                0 => NodeData::Box { enabled },
                                1 => NodeData::Text { value: string },
                                _ => NodeData::Button {
                                    label: string,
                                    enabled,
                                },
                            },
                            children: vec![],
                        },
                    ));
                    TreeSnapshot {
                        revision: 7,
                        root: id(1),
                        nodes,
                    }
                },
            )
        }

        fn arbitrary_patch() -> impl Strategy<Value = Patch> {
            let node_id = (1_u64..=8).prop_map(id);
            prop_oneof![
                arbitrary_node().prop_map(|node| Patch::Create { node }),
                node_id.clone().prop_map(|id| Patch::Delete { id }),
                (node_id.clone(), arbitrary_string())
                    .prop_map(|(id, value)| Patch::SetText { id, value }),
                (node_id.clone(), arbitrary_string())
                    .prop_map(|(id, value)| Patch::SetLabel { id, value }),
                (node_id.clone(), any::<bool>())
                    .prop_map(|(id, value)| Patch::SetEnabled { id, value }),
                (node_id.clone(), any::<u32>(), node_id.clone()).prop_map(
                    |(parent, index, child)| {
                        Patch::InsertChild {
                            parent,
                            index,
                            child,
                        }
                    }
                ),
                (node_id.clone(), any::<u32>(), node_id.clone()).prop_map(
                    |(parent, index, expected_child)| Patch::RemoveChild {
                        parent,
                        index,
                        expected_child,
                    }
                ),
                (node_id.clone(), any::<u32>(), any::<u32>(), node_id,).prop_map(
                    |(parent, from_index, to_index, expected_child)| Patch::MoveChild {
                        parent,
                        from_index,
                        to_index,
                        expected_child,
                    }
                ),
            ]
        }

        fn property_limits() -> Limits {
            Limits {
                max_nodes: 8,
                max_depth: 6,
                max_children: 5,
                max_text_len: 32,
                max_label_len: 32,
                max_patches: 12,
                ..Limits::default()
            }
        }

        proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn arbitrary_malformed_snapshots_never_panic(snapshot in arbitrary_snapshot()) {
            let result = Tree::from_snapshot(snapshot, &property_limits());
            if let Ok(tree) = result {
                let canonical_snapshot = tree.to_snapshot();
                let round_trip = Tree::from_snapshot(
                    canonical_snapshot.clone(),
                    &property_limits(),
                )
                .expect("a validated tree's snapshot must revalidate");
                prop_assert_eq!(round_trip.to_snapshot(), canonical_snapshot);
                prop_assert_eq!(round_trip.canonical(), tree.canonical());
            }
        }

        #[test]
        fn arbitrary_patch_batches_are_atomic_and_preserve_invariants(
            base_revision in prop_oneof![Just(7), any::<u64>()],
            next_revision in prop_oneof![Just(7), Just(8), any::<u64>()],
            patches in prop::collection::vec(arbitrary_patch(), 0..=16),
        ) {
            let mut tree = patch_tree(vec![
                node(1, NodeData::Root, &[2, 3]),
                node(2, NodeData::Box { enabled: true }, &[]),
                node(3, NodeData::Text { value: "seed".into() }, &[]),
            ]);
            let before = tree.clone();
            let before_revision = tree.revision();
            let before_canonical = tree.canonical();
            let result = tree.apply(
                PatchBatch { base_revision, next_revision, patches },
                &property_limits(),
            );

            match result {
                Ok(()) => {
                    let snapshot = tree.to_snapshot();
                    let revalidated = Tree::from_snapshot(snapshot.clone(), &property_limits())
                        .expect("an accepted tree's snapshot must revalidate");
                    prop_assert_eq!(revalidated.to_snapshot(), snapshot);
                    prop_assert_eq!(revalidated.canonical(), tree.canonical());
                }
                Err(_) => {
                    prop_assert_eq!(&tree, &before);
                    prop_assert_eq!(tree.revision(), before_revision);
                    prop_assert_eq!(tree.canonical(), before_canonical);
                }
            }
        }


        #[test]
        fn valid_snapshots_round_trip_canonically(snapshot in arbitrary_valid_snapshot()) {
            let tree = Tree::from_snapshot(snapshot, &Limits::default())
                .expect("the strategy only constructs valid trees");
            let canonical_snapshot = tree.to_snapshot();
            let round_trip = Tree::from_snapshot(canonical_snapshot.clone(), &Limits::default())
                .expect("canonical snapshots must remain valid");
            prop_assert_eq!(round_trip.to_snapshot(), canonical_snapshot);
            prop_assert_eq!(round_trip.canonical(), tree.canonical());
        }

        #[test]
        fn accepted_batches_always_revalidate(
            value in arbitrary_string(),
            enabled in any::<bool>(),
            move_child in any::<bool>(),
        ) {
            let mut tree = patch_tree(vec![
                node(1, NodeData::Root, &[2, 3]),
                node(2, NodeData::Box { enabled: true }, &[]),
                node(3, NodeData::Text { value: "seed".into() }, &[]),
            ]);
            let mut patches = vec![
                Patch::SetEnabled { id: id(2), value: enabled },
                Patch::SetText { id: id(3), value },
            ];
            if move_child {
                patches.push(Patch::MoveChild {
                    parent: id(1),
                    from_index: 0,
                    to_index: 1,
                    expected_child: id(2),
                });
            }
            tree.apply(batch(patches), &Limits::default())
                .expect("the strategy only constructs valid batches");
            let snapshot = tree.to_snapshot();
            let revalidated = Tree::from_snapshot(snapshot.clone(), &Limits::default())
                .expect("accepted trees must remain valid");
            prop_assert_eq!(revalidated.to_snapshot(), snapshot);
            prop_assert_eq!(revalidated.canonical(), tree.canonical());
        }
        }
    }
}
