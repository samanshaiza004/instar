//! What changed between the interface the guest last committed and this one.
//!
//! # The contract
//!
//! > A guest commits a full UI **snapshot**. The host diffs it against the
//! > retained tree by [`NodeKey`] and applies only what changed. **Host nodes
//! > are not destroyed and recreated because another snapshot arrived.**
//!
//! The snapshot is authoritative as a *description*. The retained tree is the
//! interaction, layout, and render object, and it persists across commits.
//!
//! # Why snapshots rather than guest-sent deltas
//!
//! The predecessor codebase had a mutation protocol — `set_text(key, value)`
//! and a patch pipeline — and Instar deliberately does not. A guest sending
//! deltas must track its own dirty state, and a guest that gets that wrong
//! shows stale UI forever with nothing able to detect it. That is a whole
//! class of host/guest disagreement bugs that full snapshots cannot have:
//! guest and host completely re-synchronize on every accepted commit, and
//! recovery from any confusion is "send another snapshot".
//!
//! It is the same reasoning as the commit reply guard and the generation
//! screen — make the bad state unrepresentable rather than merely tested for.
//!
//! The cost is wire bytes proportional to the tree rather than to the change.
//! Whether that matters is a question for the benchmark, not for intuition. If
//! snapshot transport turns out to dominate while layout and paint are cheap,
//! a delta fast path can be added later with the snapshot kept as the
//! resynchronization oracle. Not before evidence.
//!
//! # What this module is for
//!
//! Producing the *information* a later stage acts on. Diffing does not itself
//! make anything faster; it makes it possible to stop doing work, which is a
//! different thing and has to come first — otherwise text shaping, layout,
//! and accessibility all get wired into an "everything changed" pipeline and
//! have to be unpicked afterwards.

use std::collections::HashMap;

use crate::{Node, NodeKey, NodeKind, Tree, TreeError};

/// Everything that differs between two snapshots, grouped by what it forces
/// the host to redo.
///
/// A node can appear in several lists: changing a button's label is
/// `text_changed` (it must be re-shaped) *and* `layout_changed` (it may now be
/// a different width).
///
/// Keys are reported once per list and in no guaranteed order. Callers should
/// treat these as sets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    /// Keys present in the new snapshot and absent from the old.
    pub created: Vec<NodeKey>,
    /// Keys present in the old tree and absent from the new snapshot.
    ///
    /// Whoever holds per-node transient state — scroll offsets, hover, focus —
    /// must drop it for these. An id that comes back later is a *new* node at
    /// a new generation, not the same node with a history.
    pub removed: Vec<NodeKey>,
    /// Nodes whose child sequence changed: insertion, removal, or reorder.
    ///
    /// Reported against the **parent**, because the parent is what has to be
    /// re-laid-out. A child moving between parents shows up as a structural
    /// change on both.
    pub structure_changed: Vec<NodeKey>,
    /// Nodes whose layout intent changed — dimensions, padding, gap.
    pub layout_changed: Vec<NodeKey>,
    /// Nodes whose displayed string changed. These, and only these, need
    /// re-shaping.
    pub text_changed: Vec<NodeKey>,
    /// Nodes whose appearance changed without changing their text or geometry.
    ///
    /// Empty until the protocol carries style (Stage 2). It exists now so the
    /// type does not change shape underneath the routing that reads it.
    pub style_changed: Vec<NodeKey>,
    /// Nodes whose interactivity changed. Affects hit-testing, accessibility,
    /// and how the node is drawn — a disabled control is drawn as unavailable
    /// rather than hidden.
    pub enabled_changed: Vec<NodeKey>,
}

impl ChangeSet {
    /// Nothing differs. The commit is a no-op and the host can skip layout,
    /// scene lowering, and any accessibility update entirely.
    ///
    /// Worth having as a first-class answer: a guest that re-commits an
    /// identical snapshot — a common shape for an event it decided to ignore —
    /// should cost nothing beyond the decode.
    pub fn is_empty(&self) -> bool {
        self.created.is_empty()
            && self.removed.is_empty()
            && self.structure_changed.is_empty()
            && self.layout_changed.is_empty()
            && self.text_changed.is_empty()
            && self.style_changed.is_empty()
            && self.enabled_changed.is_empty()
    }

    /// Nodes whose text must be re-shaped.
    ///
    /// Text changes always; style changes once style can affect metrics
    /// (Stage 2) — a font size or weight change re-shapes even though the
    /// string is identical.
    pub fn needs_reshape(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.text_changed
            .iter()
            .chain(&self.style_changed)
            .chain(&self.created)
            .copied()
    }

    /// Whether geometry has to be recomputed at all.
    ///
    /// Deliberately a whole-tree answer rather than a per-node one: flexbox
    /// distributes space between siblings, so one child changing width can
    /// move every other child of that parent. Working out the true blast
    /// radius is a Taffy-caching question the benchmark has not asked yet.
    ///
    /// **`enabled_changed` is deliberately absent.** A disabled control is
    /// drawn differently but measured identically — `layout::measure` looks at
    /// the text and whether the node is a button, and never at enabled-ness.
    /// Including it here would re-lay-out the whole tree every time a button
    /// greyed out, which the counter guest alone does on its first click.
    pub fn needs_layout(&self) -> bool {
        !self.created.is_empty()
            || !self.removed.is_empty()
            || !self.structure_changed.is_empty()
            || !self.layout_changed.is_empty()
            || !self.text_changed.is_empty()
            || !self.style_changed.is_empty()
    }

    /// Whether anything on screen would look different.
    pub fn needs_paint(&self) -> bool {
        !self.is_empty()
    }

    /// Whether the accessibility tree has to be updated.
    ///
    /// Text, structure, and enabled-ness are all things a screen reader
    /// reports. Pure geometry changes matter too, because bounds are exposed —
    /// which is why this is currently the same question as `needs_paint`.
    pub fn needs_a11y(&self) -> bool {
        !self.is_empty()
    }

    /// Every key mentioned anywhere, deduplicated. Diagnostics and tests.
    pub fn touched(&self) -> Vec<NodeKey> {
        let mut keys: Vec<NodeKey> = self
            .created
            .iter()
            .chain(&self.removed)
            .chain(&self.structure_changed)
            .chain(&self.layout_changed)
            .chain(&self.text_changed)
            .chain(&self.style_changed)
            .chain(&self.enabled_changed)
            .copied()
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}

/// Compares the retained tree against a newly committed snapshot.
///
/// `previous` is `None` for a guest's opening commit, where every node is
/// [`ChangeSet::created`].
///
/// # Rules
///
/// - **Identity is [`NodeKey`]**, which the guest chooses and the protocol
///   already guarantees is unique within a tree.
/// - **Same key, same kind** → compare layout intent, text, enabled-ness, and
///   the child key sequence.
/// - **Same key, different kind** → [`TreeError::KindChanged`]. *Not* a
///   silent replacement: a guest reusing a key for a different kind of thing
///   is describing something it did not mean, and quietly swapping the node
///   would move whatever transient state the host holds for that key —
///   focus, scroll offset, an in-flight press — onto an unrelated control.
/// - **Absent from the old** → created. **Absent from the new** → removed.
/// - **Reordered children** → the parent is `structure_changed` and
///   `layout_changed`.
/// - **Identical subtree** → stop descending.
///
/// # Errors
///
/// Only [`TreeError::KindChanged`]. Everything else about the snapshot has
/// already been validated by the time it gets here.
///
/// The error matters for atomicity: this runs **before** the snapshot is
/// promoted, so a refused diff leaves the previous interface standing exactly
/// as a refused decode does.
pub fn diff(previous: Option<&Tree>, next: &Tree) -> Result<ChangeSet, TreeError> {
    let mut changes = ChangeSet::default();

    let Some(previous) = previous else {
        // Opening commit: everything is new. Reported as created rather than
        // as an empty diff, so a first frame is not mistaken for a no-op.
        collect_keys(&next.root, &mut changes.created);
        return Ok(changes);
    };

    let old_nodes = index(previous);
    let new_nodes = index(next);

    for (key, node) in &new_nodes {
        match old_nodes.get(key) {
            None => changes.created.push(*key),
            Some(old) => compare(old, node, &mut changes)?,
        }
    }
    for key in old_nodes.keys() {
        if !new_nodes.contains_key(key) {
            changes.removed.push(*key);
        }
    }

    Ok(changes)
}

/// Every node in a tree, by key.
///
/// Keys are unique within a tree — `Tree::from_wire` rejects duplicates — so
/// a flat map is a faithful index rather than a lossy one.
fn index(tree: &Tree) -> HashMap<NodeKey, &Node> {
    tree.iter().map(|node| (node.key, node)).collect()
}

fn collect_keys(node: &Node, into: &mut Vec<NodeKey>) {
    into.push(node.key);
    for child in &node.children {
        collect_keys(child, into);
    }
}

/// Compares one node against its previous self, recording what differs.
///
/// Does not recurse: [`diff`] visits every node by key, so children are
/// compared on their own turn. What this checks about children is only the
/// *sequence* of their keys, which is the parent's business.
fn compare(old: &Node, new: &Node, changes: &mut ChangeSet) -> Result<(), TreeError> {
    if old.kind.name() != new.kind.name() {
        return Err(TreeError::KindChanged {
            key: new.key,
            was: old.kind.name(),
            now: new.kind.name(),
        });
    }

    // Cheap and total: if the whole subtree is identical there is nothing to
    // report for this node, and `diff`'s per-key walk will find nothing for
    // its descendants either.
    if old == new {
        return Ok(());
    }

    if old.layout != new.layout {
        changes.layout_changed.push(new.key);
    }

    match (&old.kind, &new.kind) {
        (NodeKind::Text { text: was }, NodeKind::Text { text: now }) if was != now => {
            changes.text_changed.push(new.key);
            // Re-shaping can produce a different intrinsic size.
            changes.layout_changed.push(new.key);
        }
        (
            NodeKind::Button {
                label: was,
                enabled: was_enabled,
            },
            NodeKind::Button {
                label: now,
                enabled: now_enabled,
            },
        ) => {
            if was != now {
                changes.text_changed.push(new.key);
                changes.layout_changed.push(new.key);
            }
            if was_enabled != now_enabled {
                changes.enabled_changed.push(new.key);
            }
        }
        _ => {}
    }

    let old_children: Vec<NodeKey> = old.children.iter().map(|child| child.key).collect();
    let new_children: Vec<NodeKey> = new.children.iter().map(|child| child.key).collect();
    if old_children != new_children {
        changes.structure_changed.push(new.key);
        if !changes.layout_changed.contains(&new.key) {
            changes.layout_changed.push(new.key);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WireDimension, WireLayout};

    /// A root (0) containing a single column (1) with the given children.
    fn snapshot(children: Vec<Node>) -> Tree {
        Tree::new(Node::root(0, vec![Node::column(1, children)]))
    }

    /// A small snapshot used by the focused change tests: root > column >
    /// (text, button, disabled button).
    fn sample() -> Tree {
        snapshot(vec![
            Node::text(2, "hello"),
            Node::button(3, "Go").with_layout(padded(4)),
            Node::button(4, "Stop").disabled(),
        ])
    }

    fn padded(padding: u16) -> WireLayout {
        WireLayout {
            padding,
            ..WireLayout::default()
        }
    }

    fn assert_same_keys(actual: Vec<NodeKey>, expected: &[NodeKey]) {
        let mut actual = actual;
        actual.sort_unstable();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(
            actual, expected,
            "the reported key set is wrong; order is not part of the ChangeSet contract"
        );
    }

    #[test]
    fn the_opening_commit_creates_every_node_and_nothing_else() {
        let tree = sample();

        let changes = diff(None, &tree).unwrap();

        assert_eq!(
            changes,
            ChangeSet {
                created: vec![
                    NodeKey::first(0),
                    NodeKey::first(1),
                    NodeKey::first(2),
                    NodeKey::first(3),
                    NodeKey::first(4)
                ],
                ..ChangeSet::default()
            },
            "the first snapshot must be reported as all-new so the host does not mistake it for a no-op"
        );
    }

    #[test]
    fn an_identical_resnapshot_produces_an_empty_change_set() {
        let tree = sample();

        let changes = diff(Some(&tree), &tree).unwrap();

        assert!(
            changes.is_empty(),
            "an identical commit must cost the host nothing: {changes:?}"
        );
    }

    /// The host skips layout and paint when the diff is empty. If that ever
    /// happened for unequal trees, a real change would silently stop
    /// appearing, so the equivalence has to be pinned directly.
    #[test]
    fn diff_is_empty_exactly_when_the_snapshots_are_equal() {
        let pairs = vec![
            (sample(), sample()),
            (
                sample(),
                snapshot(vec![
                    Node::text(2, "hello!"),
                    Node::button(3, "Go").with_layout(padded(4)),
                    Node::button(4, "Stop").disabled(),
                ]),
            ),
            (
                sample(),
                snapshot(vec![
                    Node::text(2, "hello"),
                    Node::button(3, "Go").with_layout(padded(8)),
                    Node::button(4, "Stop").disabled(),
                ]),
            ),
            (
                sample(),
                snapshot(vec![
                    Node::text(2, "hello"),
                    Node::button(3, "Go").with_layout(padded(4)),
                    Node::button(4, "Stop"),
                ]),
            ),
            (
                sample(),
                snapshot(vec![
                    Node::text(2, "hello"),
                    Node::button(3, "Go").with_layout(padded(4)),
                    Node::button(4, "Stop").disabled(),
                    Node::text(5, "added"),
                ]),
            ),
            (
                sample(),
                snapshot(vec![
                    Node::text(2, "hello"),
                    Node::button(3, "Go").with_layout(padded(4)),
                ]),
            ),
            (
                sample(),
                snapshot(vec![
                    Node::button(3, "Go").with_layout(padded(4)),
                    Node::text(2, "hello"),
                    Node::button(4, "Stop").disabled(),
                ]),
            ),
            (
                sample(),
                snapshot(vec![
                    Node::text(2, "changed"),
                    Node::button(3, "Go").with_layout(padded(4)),
                    Node::button(4, "Stop").disabled(),
                ]),
            ),
        ];

        for (index, (previous, next)) in pairs.into_iter().enumerate() {
            let noop = diff(Some(&previous), &next).unwrap().is_empty();
            assert_eq!(
                noop,
                previous == next,
                "pair {index}: the no-op shortcut may only fire for identical snapshots"
            );
        }
    }

    #[test]
    fn a_changed_text_string_reshapes_and_relayouts_that_node_only() {
        let previous = snapshot(vec![Node::text(2, "before"), Node::text(3, "untouched")]);
        let next = snapshot(vec![Node::text(2, "after"), Node::text(3, "untouched")]);

        let changes = diff(Some(&previous), &next).unwrap();

        assert_eq!(
            changes,
            ChangeSet {
                text_changed: vec![NodeKey::first(2)],
                layout_changed: vec![NodeKey::first(2)],
                ..ChangeSet::default()
            },
            "a new string re-shapes the node and may resize it; its sibling must be untouched"
        );
    }

    #[test]
    fn a_changed_button_label_reshapes_and_relayouts_that_button_only() {
        let previous = snapshot(vec![Node::button(2, "Go")]);
        let next = snapshot(vec![Node::button(2, "Stop")]);

        let changes = diff(Some(&previous), &next).unwrap();

        assert_eq!(
            changes,
            ChangeSet {
                text_changed: vec![NodeKey::first(2)],
                layout_changed: vec![NodeKey::first(2)],
                ..ChangeSet::default()
            },
            "a new label must be re-shaped and may resize the button"
        );
    }

    #[test]
    fn toggling_a_buttons_enabled_flag_reports_only_the_enabled_change() {
        let previous = snapshot(vec![Node::button(2, "Go").disabled()]);
        let next = snapshot(vec![Node::button(2, "Go")]);

        let changes = diff(Some(&previous), &next).unwrap();

        assert_eq!(
            changes,
            ChangeSet {
                enabled_changed: vec![NodeKey::first(2)],
                ..ChangeSet::default()
            },
            "interactivity flips without re-shaping the label or changing geometry"
        );
    }

    #[test]
    fn a_layout_intent_change_reports_only_a_layout_change() {
        let deltas = [
            (padded(4), padded(12)),
            (
                WireLayout {
                    gap: 8,
                    ..WireLayout::default()
                },
                WireLayout {
                    gap: 16,
                    ..WireLayout::default()
                },
            ),
            (
                WireLayout {
                    width: WireDimension::Fixed(100),
                    ..WireLayout::default()
                },
                WireLayout {
                    width: WireDimension::Fixed(200),
                    ..WireLayout::default()
                },
            ),
        ];

        for (index, (was, now)) in deltas.into_iter().enumerate() {
            let previous = snapshot(vec![Node::text(2, "same").with_layout(was)]);
            let next = snapshot(vec![Node::text(2, "same").with_layout(now)]);

            let changes = diff(Some(&previous), &next).unwrap();

            assert_eq!(
                changes,
                ChangeSet {
                    layout_changed: vec![NodeKey::first(2)],
                    ..ChangeSet::default()
                },
                "layout delta {index} is geometry intent and needs no re-shaping"
            );
        }
    }

    #[test]
    fn appending_a_child_creates_the_child_and_dirties_the_parent() {
        let previous = snapshot(vec![Node::text(2, "a"), Node::text(3, "b")]);
        let next = snapshot(vec![
            Node::text(2, "a"),
            Node::text(3, "b"),
            Node::text(4, "c"),
        ]);

        let changes = diff(Some(&previous), &next).unwrap();

        assert_eq!(
            changes,
            ChangeSet {
                created: vec![NodeKey::first(4)],
                structure_changed: vec![NodeKey::first(1)],
                layout_changed: vec![NodeKey::first(1)],
                ..ChangeSet::default()
            },
            "the new child is created and the column must re-run its children sequence and geometry"
        );
    }

    #[test]
    fn removing_a_child_removes_it_and_dirties_the_parent() {
        let previous = snapshot(vec![Node::text(2, "a"), Node::text(3, "b")]);
        let next = snapshot(vec![Node::text(2, "a")]);

        let changes = diff(Some(&previous), &next).unwrap();

        assert_eq!(
            changes,
            ChangeSet {
                removed: vec![NodeKey::first(3)],
                structure_changed: vec![NodeKey::first(1)],
                layout_changed: vec![NodeKey::first(1)],
                ..ChangeSet::default()
            },
            "the removed child's transient state must be dropped, and the column must relayout without it"
        );
    }

    #[test]
    fn swapping_children_dirties_the_parent_without_creating_or_removing() {
        let previous = snapshot(vec![Node::text(2, "a"), Node::text(3, "b")]);
        let next = snapshot(vec![Node::text(3, "b"), Node::text(2, "a")]);

        let changes = diff(Some(&previous), &next).unwrap();

        assert_eq!(
            changes,
            ChangeSet {
                structure_changed: vec![NodeKey::first(1)],
                layout_changed: vec![NodeKey::first(1)],
                ..ChangeSet::default()
            },
            "reordering is a parent-level change; no node identity is gained or lost"
        );
    }

    /// A key naming a different kind is refused instead of silently replacing
    /// the node: the host may hold focus, scroll, or press state against that
    /// key, and a quiet swap would move it onto an unrelated control.
    #[test]
    fn reusing_a_key_for_a_different_kind_is_refused() {
        let previous = snapshot(vec![Node::text(5, "hello")]);
        let next = snapshot(vec![Node::button(5, "hello")]);

        assert_eq!(
            diff(Some(&previous), &next),
            Err(TreeError::KindChanged {
                key: NodeKey::first(5),
                was: "text",
                now: "button",
            }),
            "a key reused across kinds must be refused, not treated as a replacement"
        );
    }

    #[test]
    fn moving_a_node_between_parents_dirties_both_parents() {
        let previous = Tree::new(Node::root(
            0,
            vec![
                Node::column(1, vec![Node::text(10, "mover"), Node::text(11, "stays")]),
                Node::column(2, vec![Node::text(12, "other")]),
            ],
        ));
        let next = Tree::new(Node::root(
            0,
            vec![
                Node::column(1, vec![Node::text(11, "stays")]),
                Node::column(2, vec![Node::text(12, "other"), Node::text(10, "mover")]),
            ],
        ));

        let changes = diff(Some(&previous), &next).unwrap();

        assert_same_keys(
            changes.structure_changed,
            &[NodeKey::first(1), NodeKey::first(2)],
        );
        assert_same_keys(
            changes.layout_changed,
            &[NodeKey::first(1), NodeKey::first(2)],
        );
        assert!(changes.created.is_empty(), "a move is not a creation");
        assert!(changes.removed.is_empty(), "a move is not a removal");
        assert!(
            changes.text_changed.is_empty(),
            "the moved node's contents are unchanged"
        );
        assert!(
            changes.enabled_changed.is_empty(),
            "nothing's interactivity changed"
        );
    }

    #[test]
    fn removing_a_subtree_removes_every_descendant_key() {
        let previous = Tree::new(Node::root(
            0,
            vec![Node::column(
                1,
                vec![
                    Node::column(2, vec![Node::text(3, "a"), Node::text(4, "b")]),
                    Node::button(5, "keep"),
                ],
            )],
        ));
        let next = Tree::new(Node::root(
            0,
            vec![Node::column(1, vec![Node::button(5, "keep")])],
        ));

        let changes = diff(Some(&previous), &next).unwrap();

        assert_same_keys(
            changes.removed,
            &[NodeKey::first(2), NodeKey::first(3), NodeKey::first(4)],
        );
        assert_same_keys(changes.structure_changed, &[NodeKey::first(1)]);
        assert_same_keys(changes.layout_changed, &[NodeKey::first(1)]);
        assert!(changes.created.is_empty(), "only removals happened");
    }

    /// This is the property the module exists for: a deep single-leaf edit in
    /// a ~500-node snapshot must not force the whole tree through layout.
    #[test]
    fn changing_one_deep_leaf_in_a_large_tree_touches_only_that_leaf() {
        const COLUMNS: u32 = 498;
        let previous = nested_chain(COLUMNS, "tip");
        let next = nested_chain(COLUMNS, "changed");

        let changes = diff(Some(&previous), &next).unwrap();

        assert_eq!(
            changes.text_changed,
            vec![NodeKey::first(COLUMNS + 1)],
            "exactly the changed leaf may need re-shaping"
        );
        assert_eq!(
            changes.touched(),
            vec![NodeKey::first(COLUMNS + 1)],
            "a deep edit must touch exactly one key, not its ancestors"
        );
    }

    fn nested_chain(columns: u32, tip: &str) -> Tree {
        let mut node = Node::text(columns + 1, tip);
        for key in (1..=columns).rev() {
            node = Node::column(key, vec![node]);
        }
        Tree::new(Node::root(0, vec![node]))
    }

    /// The predicate flags gate the host's skip-work shortcuts: an empty diff
    /// must turn every one of them off, while each kind of change must keep
    /// the pipeline honest.
    #[test]
    fn the_predicates_track_whether_any_work_is_required() {
        let empty = diff(Some(&sample()), &sample()).unwrap();
        assert_eq!(empty.needs_reshape().count(), 0, "a no-op needs no shaping");
        assert!(!empty.needs_layout(), "a no-op must skip layout");
        assert!(!empty.needs_paint(), "a no-op must skip painting");
        assert!(!empty.needs_a11y(), "a no-op must skip accessibility");

        let layout_only = diff(
            Some(&snapshot(vec![
                Node::button(2, "Go").with_layout(padded(4)),
            ])),
            &snapshot(vec![Node::button(2, "Go").with_layout(padded(12))]),
        )
        .unwrap();
        assert_eq!(
            layout_only.needs_reshape().count(),
            0,
            "geometry intent does not re-shape"
        );
        assert!(layout_only.needs_layout(), "geometry changes need layout");
        assert!(layout_only.needs_paint(), "geometry changes need painting");
        assert!(
            layout_only.needs_a11y(),
            "bounds are exposed to screen readers"
        );

        let text_only = diff(
            Some(&snapshot(vec![Node::text(2, "short")])),
            &snapshot(vec![Node::text(2, "a much longer line")]),
        )
        .unwrap();
        assert_eq!(
            text_only.needs_reshape().collect::<Vec<_>>(),
            vec![NodeKey::first(2)],
            "changed text must be re-shaped"
        );
        assert!(
            text_only.needs_layout(),
            "re-shaping can change intrinsic size"
        );

        let enabled_only = diff(
            Some(&snapshot(vec![Node::button(2, "Go").disabled()])),
            &snapshot(vec![Node::button(2, "Go")]),
        )
        .unwrap();
        assert_eq!(
            enabled_only.needs_reshape().count(),
            0,
            "enabled-ness does not re-shape"
        );
        assert!(
            !enabled_only.needs_layout(),
            "a disabled control is drawn differently but measured identically; \
             relaying out the tree for it would cost a full pass every time a \
             button greyed out"
        );
        assert!(
            enabled_only.needs_paint(),
            "a disabled control is drawn differently -- and this, not \
             needs_layout, is what says the commit was not a no-op"
        );
        assert!(
            enabled_only.needs_a11y(),
            "screen readers report enabled-ness"
        );
    }

    /// touched() is the diagnostics view of a ChangeSet: a key mentioned in
    /// several lists must appear once, and the result must be stable so
    /// callers can compare change sets across commits.
    #[test]
    fn touched_deduplicates_and_returns_sorted_keys() {
        let previous = snapshot(vec![
            Node::text(2, "old"),
            Node::button(3, "Go"),
            Node::button(4, "Stop"),
        ]);
        let next = snapshot(vec![
            Node::text(2, "new"),
            Node::button(3, "Go").disabled(),
            Node::button(4, "Stop").with_layout(padded(8)),
            Node::text(5, "added"),
        ]);

        let changes = diff(Some(&previous), &next).unwrap();

        assert_eq!(
            changes.touched(),
            vec![
                NodeKey::first(1),
                NodeKey::first(2),
                NodeKey::first(3),
                NodeKey::first(4),
                NodeKey::first(5)
            ],
            "the parent appears once despite structure+layout, the text once despite text+layout, all sorted"
        );

        let mut reshaped: Vec<NodeKey> = changes.needs_reshape().collect();
        reshaped.sort_unstable();
        assert_eq!(
            reshaped,
            vec![NodeKey::first(2), NodeKey::first(5)],
            "reshape work is changed text plus newly created nodes"
        );
    }
}
