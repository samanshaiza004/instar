//! The host-side lifecycle of node ids across snapshots.
//!
//! The protocol says a node key is `(id, generation)` but leaves what that
//! means to the host. This ledger is where a guest's chosen ids acquire
//! meaning: an id never seen before starts at generation 0, a live id keeps
//! its generation, and a retired id may only come back higher.
//!
//! The ledger is deliberately stateful and host-side. `instar-ui-protocol`'s
//! bounds apply to every decode of every message; this bound applies to one
//! runtime generation's history of ids, which no single message can see.

use std::collections::{HashMap, HashSet};

use crate::{Tree, TreeError};

/// Maximum distinct node ids ever observed per ledger lifetime.
///
/// [`instar_ui_protocol::limits`] is documented as bounds applied to every
/// decode; this is a stateful host-side ledger bound, so it lives here and not
/// there. `MAX_NODES` bounds how many nodes are live at once; this bounds how
/// many ids a long-running guest can burn over time.
pub const MAX_NODE_IDS: usize = 65_536;

/// One id's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    generation: u32,
    live: bool,
}

/// The id lifecycle for one runtime generation.
///
/// Two-phase on purpose: [`KeyLedger::validate`] may refuse before anything is
/// mutated, and [`KeyLedger::apply`] then records the accepted snapshot. That
/// split is what lets a host reject a bad snapshot while leaving its previous
/// interface standing.
#[derive(Debug, Default)]
pub struct KeyLedger {
    entries: HashMap<u32, Entry>,
}

impl KeyLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuses a snapshot that violates the lifecycle rules or would push the
    /// count of distinct ids ever observed past [`MAX_NODE_IDS`].
    ///
    /// Counts the ids this snapshot introduces without mutating anything, so a
    /// refusal cannot leave the ledger half-updated.
    pub fn validate(&self, tree: &Tree) -> Result<(), TreeError> {
        let mut introduced = 0usize;
        let mut introduced_ids = HashSet::new();
        for node in tree.iter() {
            let id = node.key.id;
            match self.entries.get(&id) {
                None => {
                    if !introduced_ids.insert(id) {
                        continue;
                    }
                    if node.key.generation != 0 {
                        return Err(TreeError::BadFirstGeneration { key: node.key });
                    }
                    introduced += 1;
                }
                Some(entry) if entry.live => {
                    if node.key.generation != entry.generation {
                        return Err(TreeError::GenerationChanged {
                            key: node.key,
                            live: entry.generation,
                        });
                    }
                }
                Some(entry) => {
                    if node.key.generation <= entry.generation {
                        return Err(TreeError::GenerationNotAdvanced {
                            key: node.key,
                            retired: entry.generation,
                        });
                    }
                }
            }
        }

        let observed = self.entries.len().saturating_add(introduced);
        if observed > MAX_NODE_IDS {
            return Err(TreeError::TooManyNodeIds { observed });
        }
        Ok(())
    }

    /// Records an accepted snapshot: every key in the tree becomes live at its
    /// stated generation, and every id that was live and is absent becomes
    /// retired, keeping its generation.
    ///
    /// Runs only after [`KeyLedger::validate`] accepted the snapshot. Old
    /// generations can never become live again within this ledger's lifetime.
    pub fn apply(&mut self, tree: &Tree) {
        let live: HashSet<u32> = tree.iter().map(|node| node.key.id).collect();
        for (id, entry) in &mut self.entries {
            if !live.contains(id) && entry.live {
                entry.live = false;
            }
        }
        for node in tree.iter() {
            self.entries.insert(
                node.key.id,
                Entry {
                    generation: node.key.generation,
                    live: true,
                },
            );
        }
    }

    /// Forgets everything. Called when the Wasm runtime generation dies: the
    /// whole ledger goes with it, so a fresh generation starts from a clean id
    /// history.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// How many distinct ids have ever been observed in this ledger's lifetime.
    pub fn observed_ids(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Node, NodeKey, NodeKind, WireLayout};

    /// A root (0) holding one text leaf at `id`.
    fn tree_with(id: u32, generation: u32) -> Tree {
        Tree::new(Node::root(
            0,
            vec![Node {
                key: NodeKey::new(id, generation),
                kind: NodeKind::Text {
                    text: "x".to_string(),
                },
                layout: WireLayout::default(),
                style: Default::default(),
                children: Vec::new(),
            }],
        ))
    }

    #[test]
    fn a_fresh_id_at_generation_zero_is_accepted() {
        let mut ledger = KeyLedger::new();
        let tree = tree_with(7, 0);

        ledger.validate(&tree).unwrap();
        ledger.apply(&tree);
        assert_eq!(ledger.observed_ids(), 2, "the root and the leaf");
    }

    #[test]
    fn a_fresh_id_at_a_non_zero_generation_is_refused() {
        let ledger = KeyLedger::new();
        assert_eq!(
            ledger.validate(&tree_with(7, 1)),
            Err(TreeError::BadFirstGeneration {
                key: NodeKey::new(7, 1),
            })
        );
    }

    #[test]
    fn a_live_id_resent_at_the_same_generation_is_accepted() {
        let mut ledger = KeyLedger::new();
        ledger.apply(&tree_with(7, 0));

        ledger.validate(&tree_with(7, 0)).unwrap();
    }

    #[test]
    fn a_live_id_resent_at_a_different_generation_is_refused() {
        let mut ledger = KeyLedger::new();
        ledger.apply(&tree_with(7, 0));

        assert_eq!(
            ledger.validate(&tree_with(7, 1)),
            Err(TreeError::GenerationChanged {
                key: NodeKey::new(7, 1),
                live: 0,
            })
        );
    }

    #[test]
    fn a_retired_id_at_a_higher_generation_is_accepted() {
        let mut ledger = KeyLedger::new();
        ledger.apply(&tree_with(7, 0));
        ledger.apply(&empty_like_tree());

        ledger.validate(&tree_with(7, 1)).unwrap();
        ledger.apply(&tree_with(7, 1));
        assert_eq!(ledger.observed_ids(), 2, "root and id 7");
    }

    #[test]
    fn a_retired_id_at_the_same_or_lower_generation_is_refused() {
        let mut ledger = KeyLedger::new();
        ledger.apply(&tree_with(7, 5));
        ledger.apply(&empty_like_tree());

        assert_eq!(
            ledger.validate(&tree_with(7, 5)),
            Err(TreeError::GenerationNotAdvanced {
                key: NodeKey::new(7, 5),
                retired: 5,
            }),
            "a retired id may not return at the generation it was retired at"
        );
        assert_eq!(
            ledger.validate(&tree_with(7, 4)),
            Err(TreeError::GenerationNotAdvanced {
                key: NodeKey::new(7, 4),
                retired: 5,
            })
        );
    }

    /// The hostile-guest case the doc names: `(7, 0)` must not be resendable
    /// after removal.
    #[test]
    fn the_hostile_guest_cannot_resend_7_0_after_removal() {
        let mut ledger = KeyLedger::new();
        ledger.apply(&tree_with(7, 0));
        ledger.apply(&empty_like_tree());

        assert_eq!(
            ledger.validate(&tree_with(7, 0)),
            Err(TreeError::GenerationNotAdvanced {
                key: NodeKey::new(7, 0),
                retired: 0,
            })
        );
    }

    #[test]
    fn the_max_node_ids_bound_refuses() {
        let mut ledger = KeyLedger::new();
        let mut children = Vec::with_capacity(MAX_NODE_IDS - 1);
        for id in 1..MAX_NODE_IDS as u32 {
            children.push(Node::text(id, "x"));
        }
        ledger.apply(&Tree::new(Node::root(0, children)));
        assert_eq!(ledger.observed_ids(), MAX_NODE_IDS);

        assert_eq!(
            ledger.validate(&tree_with(MAX_NODE_IDS as u32, 0)),
            Err(TreeError::TooManyNodeIds {
                observed: MAX_NODE_IDS + 1,
            })
        );
    }

    /// A tree with only the root, so an id that was live becomes absent.
    fn empty_like_tree() -> Tree {
        Tree::new(Node::root(0, Vec::new()))
    }
}
