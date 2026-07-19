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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeData {
    Root,
    Box { enabled: bool },
    Text { value: String },
    Button { label: String, enabled: bool },
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
    pub max_label_len: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: 10_000,
            max_depth: 64,
            max_children: 4_096,
            max_text_len: 64 * 1024,
            max_label_len: 4 * 1024,
        }
    }
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
                NodeData::Text { value } => {
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
                NodeData::Button { label, .. } => {
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
                NodeData::Root | NodeData::Box { .. } => {}
            }
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
        NodeData::Text { value } => {
            output.push_str("text");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            output.push_str(" \"");
            output.extend(value.escape_debug());
            output.push('"');
            return;
        }
        NodeData::Button { label, enabled } => {
            output.push_str("button");
            output.push_str(" #");
            output.push_str(&node.id.to_string());
            output.push_str(" \"");
            output.extend(label.escape_debug());
            output.push('"');
            if !enabled {
                output.push_str(" disabled");
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
}
