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

use crate::text_host::TextHost;
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

/// The retained text-view attachments, kept beside the retained tree.
///
/// B2e-3 proved the `NodeKey -> TextViewId` map correct and inert; B2e-4 gives
/// it the one behaviour it owes: the map is a **second owner** of each
/// attached view, so detaching is the only thing that lets an attached view
/// die. Every mutation goes through [`RetainedTextAttachments::retain`],
/// [`RetainedTextAttachments::release`], or
/// [`RetainedTextAttachments::clear`], which update the map and the host's
/// ownership ledger together — there is no way to change one and not the
/// other.
#[derive(Debug, Default)]
pub struct RetainedTextAttachments {
    map: BTreeMap<NodeKey, TextViewId>,
}

// B2e-4's consumers land in `lib.rs`, which this package is not editing; the
// type is complete and tested so the wiring has nothing left to design.
#[allow(dead_code)]
impl RetainedTextAttachments {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: NodeKey) -> Option<&TextViewId> {
        self.map.get(&key)
    }

    pub fn map(&self) -> &BTreeMap<NodeKey, TextViewId> {
        &self.map
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Atomically replaces the retained map and updates the host's ownership
    /// ledger.
    ///
    /// The diff is computed here, from the map and nothing else. Newly
    /// attached and repointed views are retained **before** detached and
    /// replaced views are released, so a replacement can never destroy the
    /// old view in the interval between it leaving and the new one arriving —
    /// the observable intermediate state the frozen B2e-4 mutants forbid.
    /// `new` must name only live views, which staging already guarantees; the
    /// host answers each acquisition itself and ignores one that is not live.
    pub fn retain(&mut self, host: &mut TextHost, new: BTreeMap<NodeKey, TextViewId>) {
        let changes = AttachmentChangeSet::diff(&self.map, &new);

        for key in changes.attached.iter().chain(&changes.replaced) {
            if let Some(view) = new.get(key) {
                host.retain_view_attachment(*view);
            }
        }
        for key in changes.detached.iter().chain(&changes.replaced) {
            if let Some(view) = self.map.remove(key) {
                host.release_view_attachment(view);
            }
        }

        self.map = new;
        host.collect_unowned_resources();
    }

    /// Detaches one node's attachment and lets the host reclaim anything that
    /// lost its last owner. Returns the view the node was showing.
    pub fn release(&mut self, host: &mut TextHost, key: NodeKey) -> Option<TextViewId> {
        let view = self.map.remove(&key)?;
        host.release_view_attachment(view);
        Some(view)
    }

    /// Detaches every retained attachment, for a window that is going away.
    ///
    /// Views with a guest lease survive; only attachments with no other owner
    /// are reclaimed, exactly as the lifetime law requires.
    pub fn clear(&mut self, host: &mut TextHost) {
        let views: Vec<TextViewId> = self.map.values().copied().collect();
        self.map.clear();
        for view in views {
            host.release_view_attachment(view);
        }
    }

    /// Runs the host's collection pass against the current retained state.
    ///
    /// Kept explicit so a caller that released several attachments can choose
    /// when reclamation happens; every method above already collects.
    pub fn collect(&self, host: &mut TextHost) {
        host.collect_unowned_resources();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::text_host::TextResourceCounts;
    use instar_kernel::text_bridge::{TextOperation, text_request};

    const G17: instar_kernel::runtime::GenerationId = instar_kernel::runtime::GenerationId(17);

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

    fn create_buffer(host: &mut TextHost) -> instar_text::TextBufferId {
        let (request, wait) = text_request(G17, TextOperation::CreateBuffer);
        let screened = request.screen(G17).expect("current");
        host.serve(screened);
        let key = match wait.blocking_recv().expect("answered") {
            Ok(instar_kernel::text_bridge::TextAnswer::Created(key)) => key,
            other => panic!("expected a buffer, got {other:?}"),
        };
        host.resolve_buffer_lease(G17, key).expect("leased")
    }

    fn create_view(host: &mut TextHost, buffer: instar_text::TextBufferId) -> TextViewId {
        let (request, wait) = text_request(
            G17,
            TextOperation::CreateView {
                buffer: instar_kernel::text_bridge::OpaqueResourceKey {
                    slot: buffer.id,
                    incarnation: buffer.generation,
                },
            },
        );
        let screened = request.screen(G17).expect("current");
        host.serve(screened);
        let key = match wait.blocking_recv().expect("answered") {
            Ok(instar_kernel::text_bridge::TextAnswer::Created(key)) => key,
            other => panic!("expected a view, got {other:?}"),
        };
        host.resolve_view_lease(G17, key).expect("leased")
    }

    fn map(entries: &[(u32, u32)]) -> BTreeMap<NodeKey, TextViewId> {
        entries
            .iter()
            .map(|(node, view_id)| (key(*node), view(*view_id)))
            .collect()
    }

    fn release_view(host: &mut TextHost, view: TextViewId) {
        let (request, wait) = text_request(
            G17,
            TextOperation::ReleaseView {
                key: instar_kernel::text_bridge::OpaqueResourceKey {
                    slot: view.id,
                    incarnation: view.generation,
                },
            },
        );
        host.serve(request.screen(G17).expect("current"));
        assert!(matches!(
            wait.blocking_recv().expect("answered"),
            Ok(instar_kernel::text_bridge::TextAnswer::Released)
        ));
    }

    fn release_buffer(host: &mut TextHost, buffer: instar_text::TextBufferId) {
        let (request, wait) = text_request(
            G17,
            TextOperation::ReleaseBuffer {
                key: instar_kernel::text_bridge::OpaqueResourceKey {
                    slot: buffer.id,
                    incarnation: buffer.generation,
                },
            },
        );
        host.serve(request.screen(G17).expect("current"));
        assert!(matches!(
            wait.blocking_recv().expect("answered"),
            Ok(instar_kernel::text_bridge::TextAnswer::Released)
        ));
    }

    #[test]
    fn retain_acquires_new_attachments_and_releases_gone_ones() {
        let mut host = TextHost::new();
        let mut retained = RetainedTextAttachments::new();
        let buffer = create_buffer(&mut host);
        let first = create_view(&mut host, buffer);
        let second = create_view(&mut host, buffer);

        retained.retain(&mut host, map(&[(10, first.id)]));
        assert_eq!(host.counts().retained_view_attachments, 1);
        assert_eq!(host.retained_view_attachments(), 1);
        assert_eq!(retained.get(key(10)), Some(&first));

        retained.retain(&mut host, map(&[(10, second.id)]));
        assert_eq!(
            host.counts().retained_view_attachments,
            1,
            "a replacement holds exactly one attachment"
        );
        assert_eq!(retained.get(key(10)), Some(&second));
        assert!(
            host.system().view(first).is_ok(),
            "the replaced view still has a guest lease, so it survives"
        );

        release_view(&mut host, first);
        assert!(
            host.system().view(first).is_err(),
            "replaced and no longer leased: collection takes it"
        );

        retained.retain(&mut host, BTreeMap::new());
        assert_eq!(host.counts().retained_view_attachments, 0);
        assert!(
            host.system().view(second).is_ok(),
            "the detached view still has a guest lease"
        );

        release_view(&mut host, second);
        release_buffer(&mut host, buffer);
        assert_eq!(host.counts(), TextResourceCounts::default());
    }

    #[test]
    fn release_detaches_one_node_and_collects() {
        let mut host = TextHost::new();
        let mut retained = RetainedTextAttachments::new();
        let buffer = create_buffer(&mut host);
        let first = create_view(&mut host, buffer);
        let second = create_view(&mut host, buffer);
        retained.retain(&mut host, map(&[(10, first.id), (20, second.id)]));

        assert_eq!(retained.release(&mut host, key(10)), Some(first));
        assert!(
            retained.release(&mut host, key(10)).is_none(),
            "already detached"
        );
        assert_eq!(host.counts().retained_view_attachments, 1);
        assert_eq!(
            host.counts().live_views,
            2,
            "both views still have guest leases"
        );

        retained.clear(&mut host);
        assert_eq!(host.counts().retained_view_attachments, 0);
        assert_eq!(host.counts().live_views, 2, "guest leases keep both");

        release_view(&mut host, first);
        release_view(&mut host, second);
        release_buffer(&mut host, buffer);
        assert_eq!(host.counts(), TextResourceCounts::default());
    }
}
