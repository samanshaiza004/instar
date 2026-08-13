//! The text-view attachment half of UI admission.
//!
//! A commit names its text surfaces through five identities, each owned by a
//! different layer:
//!
//! ```text
//! borrow<text-view>              the WIT handle the guest's commit argument carried
//! Resource<GuestTextView>        the Component Model resource the kernel lifted it from
//! OpaqueResourceKey              the two numbers the kernel copied out of that resource
//! TextViewId                     the host text subsystem's identity, resolved from the key
//! NodeKey -> TextViewId          the map the window retains beside the tree
//! ```
//!
//! That is four translations to get wrong, and three of them happen in
//! different crates. `instar-kernel` can carry the first four without knowing
//! the fifth exists; `instar-ui` can build a tree of `TextView` nodes without
//! knowing any of the first four exists; only `instar-host` sees both sides.
//! This module is the meeting point, and keeping it separate makes the chain
//! a thing a reader looks for in one place instead of a scattered set of
//! lookups.
//!
//! # A ladder of stronger statements
//!
//! The intermediate types here are not documentation; they are checkpoints
//! that cannot be skipped in an order that would let a bad commit through.
//! A [`ValidatedUiCommit`] has proved the bytes describe a meaningful tree
//! and the tree diff accepted it, but it still names attachments nobody has
//! resolved. A [`StagedUiCommit`] has proved every attachment is authorized,
//! no two live nodes claim one view, the ledger accepted the snapshot's id
//! lifecycle, and both diffs are computed — so nothing after it can refuse.
//! Writing the admission path as "decode, then validate, then resolve, then
//! stage" is still possible, but the types make the stronger, normative order
//! the one that constructs the final value.

use std::collections::BTreeMap;

use instar_kernel::text_bridge::AttachmentRefusal;
use instar_text::TextViewId;
use instar_ui::{ChangeSet, NodeKey, TextAttachmentRef, Tree, TreeError};

/// Why an entire UI commit was refused.
///
/// One refusal family for the two admission vocabularies a commit crosses:
/// tree problems (the batch, the diff, the ledger) and attachment problems
/// (the side table, resolution, uniqueness). The bridge maps each family to
/// its own guest-visible rejection; the host keeps them separate so a caller
/// can tell whether a refused commit tripped the tree half or the attachment
/// half without flattening both taxonomies into one error enum.
#[derive(Debug, Clone, PartialEq)]
pub enum UiCommitRefusal {
    Tree(TreeError),
    Attachment(AttachmentRefusal),
}

/// Bytes are structurally valid; the semantic tree is valid.
///
/// The tree diff has run and could still have refused
/// ([`TreeError::KindChanged`]), so the commit can still fail before this
/// point — but the attachment refs have only been carried, not resolved.
pub(crate) struct ValidatedUiCommit {
    pub tree: Tree,
    pub tree_changes: ChangeSet,
    /// Carried so the type's statement stays true even after the slot
    /// resolution has moved the information into a `NodeKey -> TextViewId`
    /// map; 3c's consumers of this checkpoint read it.
    #[allow(dead_code)]
    pub attachment_refs: Vec<TextAttachmentRef>,
}

/// Every attachment names a resource this generation is authorized to use,
/// uniqueness holds, the ledger has accepted the snapshot's id lifecycle, and
/// both diffs are computed. NO REFUSAL REMAINS POSSIBLE.
pub(crate) struct StagedUiCommit {
    pub tree: Tree,
    pub tree_changes: ChangeSet,
    pub attachments: BTreeMap<NodeKey, TextViewId>,
    /// Computed for the host's presentation layer; nothing in this package
    /// consumes it yet.
    #[allow(dead_code)]
    pub attachment_changes: AttachmentChangeSet,
}

/// What changed in the retained `NodeKey -> TextViewId` map.
///
/// Computed by comparing the OLD and NEW maps and NOTHING ELSE. The diff must
/// never see a slot number, a side-table index, or an input vector:
/// attachment-table order is deliberately meaningless — the guest supplies a
/// scratch table and the wire tree indexes into it by position — and a diff
/// able to observe that order could disagree with the maps that are the only
/// semantic statement left after resolution.
///
/// `replaced` means the same NodeKey now names a different `TextViewId`. The
/// maps are [`BTreeMap`]s so iteration here is deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentChangeSet {
    pub attached: Vec<NodeKey>,
    pub detached: Vec<NodeKey>,
    pub replaced: Vec<NodeKey>,
}

impl AttachmentChangeSet {
    /// No `NodeKey` was attached, detached, or repointed.
    pub fn is_empty(&self) -> bool {
        self.attached.is_empty() && self.detached.is_empty() && self.replaced.is_empty()
    }

    /// Diffs the old retained map against the new one.
    ///
    /// Both inputs are already the resolved, retained form: slots and side
    /// tables are gone by the time this can be called.
    pub fn diff(old: &BTreeMap<NodeKey, TextViewId>, new: &BTreeMap<NodeKey, TextViewId>) -> Self {
        let mut attached = Vec::new();
        let mut replaced = Vec::new();
        for (key, view) in new {
            match old.get(key) {
                None => attached.push(*key),
                Some(old_view) if old_view != view => replaced.push(*key),
                Some(_) => {}
            }
        }
        let mut detached = Vec::new();
        for key in old.keys() {
            if !new.contains_key(key) {
                detached.push(*key);
            }
        }
        Self {
            attached,
            detached,
            replaced,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(id: u32) -> TextViewId {
        TextViewId { id, generation: 0 }
    }

    fn key(id: u32) -> NodeKey {
        NodeKey::first(id)
    }

    #[test]
    fn identical_maps_diff_to_nothing() {
        let mut old = BTreeMap::new();
        old.insert(key(10), view(7));
        old.insert(key(20), view(8));
        let new = old.clone();

        assert_eq!(
            AttachmentChangeSet::diff(&old, &new),
            AttachmentChangeSet::default()
        );
    }

    /// Insertion order is not a difference.
    ///
    /// Deliberately *not* named for the side table: nothing here has a slot in
    /// it, so this cannot prove the diff ignores slot numbers — only that it
    /// ignores the order the maps were built in. The claim about slots needs
    /// real wire bytes and real resolution, and it is made by
    /// `swapping_every_slot_still_diff_to_nothing` in `lib.rs`.
    #[test]
    fn the_order_entries_were_inserted_in_is_not_a_difference() {
        let mut old = BTreeMap::new();
        old.insert(key(10), view(7));
        old.insert(key(20), view(8));
        let mut new = BTreeMap::new();
        new.insert(key(20), view(8));
        new.insert(key(10), view(7));

        assert_eq!(
            AttachmentChangeSet::diff(&old, &new),
            AttachmentChangeSet::default()
        );
    }

    #[test]
    fn a_new_node_is_attached_and_a_gone_one_detached() {
        let mut old = BTreeMap::new();
        old.insert(key(10), view(7));
        let mut new = BTreeMap::new();
        new.insert(key(20), view(8));

        let changes = AttachmentChangeSet::diff(&old, &new);
        assert_eq!(changes.attached, vec![key(20)]);
        assert_eq!(changes.detached, vec![key(10)]);
        assert!(changes.replaced.is_empty());
    }

    #[test]
    fn the_same_node_now_naming_a_different_view_is_replaced() {
        let mut old = BTreeMap::new();
        old.insert(key(10), view(7));
        let mut new = BTreeMap::new();
        new.insert(key(10), view(12));

        let changes = AttachmentChangeSet::diff(&old, &new);
        assert_eq!(changes.replaced, vec![key(10)]);
        assert!(changes.attached.is_empty());
        assert!(changes.detached.is_empty());
    }
}
