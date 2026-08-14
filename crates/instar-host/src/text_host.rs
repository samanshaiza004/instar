//! The host's half of the text-resource bridge (B2e-1).
//!
//! Three lifetimes, three owners, and the whole of what B2e exists to prove:
//!
//! ```text
//! GenerationId  owns  guest capability leases     dies with the Store
//! TextSystem    owns  the native resources        outlives any guest
//! HostWindow    owns  presentation attachments    B2e-4: retained
//! ```
//!
//! This registry is **host-global and keyed by generation**, never per window.
//! A buffer must not disappear because a native window did; the two have
//! nothing to do with each other.
//!
//! # The registry is authoritative, not the id
//!
//! > No text operation using an opaque key is accepted merely because the
//! > underlying `TextBufferId` or `TextViewId` is live. The current generation
//! > must own a matching lease.
//!
//! `TextViewId`'s own generation closes ABA — a stale key cannot reach the
//! resource that replaced it. It says nothing about *authority*:
//!
//! ```text
//! generation 17 creates V4, then dies
//! V4 survives, because something else will own it (B2e-4)
//! generation 18 presents a key naming V4
//!   -> the id is live
//!   -> without the registry, accepted
//! ```
//!
//! So every operation resolves through [`TextHost::resolve_view_lease`] or
//! [`TextHost::resolve_buffer_lease`], which check ownership before identity.

use std::collections::{HashMap, HashSet};

use instar_kernel::runtime::GenerationId;
use instar_kernel::text_bridge::{
    ApplyEditsOutcome, BridgeAppliedEdit, BridgeRangeContents, BridgeTextEdit, NextEditOutcome,
    OpaqueResourceKey, ScreenedTextRequest, TextAnswer, TextOperation, TextRefusal,
};
use instar_text::{
    AppliedEdit, MAX_TEXT_BUFFERS, MAX_TEXT_VIEWS, TextBufferId, TextEdit, TextError, TextSystem,
    TextViewId,
};

use crate::text_sync::{
    BufferSync, EditNotification, MAX_PENDING_EDIT_BYTES, MAX_PENDING_EDITS, NextEditAdmission,
    SyncState, WaitRefusal, WaiterId,
};

/// What one guest generation currently holds.
#[derive(Debug, Default)]
struct GenerationLeases {
    buffers: HashSet<TextBufferId>,
    views: HashSet<TextViewId>,
}

impl GenerationLeases {
    fn is_empty(&self) -> bool {
        self.buffers.is_empty() && self.views.is_empty()
    }
}

/// How many resources and leases exist, for tests that assert a return to
/// baseline rather than that some `drop` ran.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextResourceCounts {
    pub guest_buffer_leases: usize,
    pub guest_view_leases: usize,
    /// Retained view attachments, counting duplicates. B2e-4 makes each of
    /// them a second owner, so this count can stay nonzero after every guest
    /// lease is gone.
    pub retained_view_attachments: usize,
    pub live_buffers: usize,
    pub live_views: usize,
}

/// The text subsystem, and who is allowed to name what in it.
#[derive(Debug, Default)]
pub struct TextHost {
    system: TextSystem,
    leases: HashMap<GenerationId, GenerationLeases>,
    /// The retained `NodeKey -> TextViewId` map's views, as a second owner.
    ///
    /// A reference count, because the same view may be attached at several
    /// retained nodes (or by several windows) and each attachment must be
    /// released once before the view may be reclaimed. Keyed by view identity,
    /// not by window: the registry is host-global, and a window going away
    /// must not silently kill a document another surface (or a future one)
    /// still shows.
    retained_views: HashMap<TextViewId, usize>,
    /// What each generation still owes each buffer it can name (C1).
    ///
    /// Keyed by the pair, because synchronization is a fact about a
    /// *relationship* rather than about either side: the buffer outlives the
    /// generation, and the generation may name several buffers. Entries are
    /// born with the buffer and die with the generation, which is why they sit
    /// here beside the leases rather than inside `TextSystem` -- the text
    /// model has no idea a guest exists.
    sync: HashMap<(GenerationId, TextBufferId), BufferSync>,
    /// The source of every [`crate::text_sync::WaiterId`] this host mints.
    ///
    /// One counter for every buffer rather than one per relationship: a
    /// `WaiterId` only ever has to be unique *within* the single `BufferSync`
    /// that compares it, so a global monotonic source is more than sufficient
    /// and is simpler than keying a counter per buffer for no correctness
    /// gain.
    next_waiter_id: u64,
}

/// A `TextBufferId` and an [`OpaqueResourceKey`] are the same two numbers.
///
/// The translation exists in exactly one place, and this is it: the kernel
/// cannot perform it because it has no idea these types exist, which is what
/// keeps `instar-kernel -> instar-text` from being a dependency.
fn buffer_key(id: TextBufferId) -> OpaqueResourceKey {
    OpaqueResourceKey {
        slot: id.id,
        incarnation: id.generation,
    }
}

fn view_key(id: TextViewId) -> OpaqueResourceKey {
    OpaqueResourceKey {
        slot: id.id,
        incarnation: id.generation,
    }
}

fn buffer_id(key: OpaqueResourceKey) -> TextBufferId {
    TextBufferId {
        id: key.slot,
        generation: key.incarnation,
    }
}

fn view_id(key: OpaqueResourceKey) -> TextViewId {
    TextViewId {
        id: key.slot,
        generation: key.incarnation,
    }
}

/// Translates one inbound guest edit into `instar-text` vocabulary.
///
/// `u64 -> usize` is narrowing here, unlike every other conversion in this
/// file: these offsets came from a guest, not from a revision this host
/// minted. A value that does not fit becomes `usize::MAX` rather than
/// wrapping -- `as usize` would truncate a hostile 64-bit offset down to
/// whatever its low bits happen to be, which on a 32-bit host could turn an
/// out-of-range offset into one that looks valid. `usize::MAX` instead
/// guarantees `TextStorage::check` refuses it as out of bounds, the same
/// refusal a genuinely oversized in-range offset gets.
fn edit_from_bridge(edit: BridgeTextEdit) -> TextEdit {
    TextEdit {
        range: usize::try_from(edit.start).unwrap_or(usize::MAX)
            ..usize::try_from(edit.end).unwrap_or(usize::MAX),
        replacement: edit.replacement,
    }
}

/// Translates a resolved [`EditNotification`] into the kernel's fixed-width
/// vocabulary. `instar-host` is the only layer allowed to perform this
/// translation, for the same reason it is the only layer that translates an
/// [`OpaqueResourceKey`] into a [`TextBufferId`]: `instar-kernel` must not
/// depend on `instar-text`.
fn bridge_notification(notification: EditNotification) -> NextEditOutcome {
    match notification {
        EditNotification::Edits(edits) => {
            NextEditOutcome::Edits(edits.iter().map(bridge_edit).collect())
        }
        EditNotification::Desynchronized(revision) => NextEditOutcome::Desynchronized(revision.0),
    }
}

/// One applied edit, translated into fixed-width scalars. Widening only:
/// `usize -> u64` cannot fail on any platform Instar targets, so there is no
/// narrowing conversion in this direction to guard.
fn bridge_edit(applied: &AppliedEdit) -> BridgeAppliedEdit {
    BridgeAppliedEdit {
        base_revision: applied.base_revision.0,
        resulting_revision: applied.resulting_revision.0,
        start: applied.edit.range.start as u64,
        end: applied.edit.range.end as u64,
        replacement: applied.edit.replacement.clone(),
    }
}

impl TextHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a fresh, never-reused [`WaiterId`].
    fn next_waiter_id(&mut self) -> WaiterId {
        self.next_waiter_id += 1;
        WaiterId(self.next_waiter_id)
    }

    /// Registers a freshly opened buffer's lease and its synchronized-at-
    /// baseline relationship.
    ///
    /// Shared by both bootstrap paths (`CreateBuffer` and
    /// `CreateBufferWithContents`): a document is born `Synchronized` at
    /// whatever revision `TextSystem::open_buffer` just gave it, with an
    /// empty queue -- the guest supplied its own bytes, if any, so there is
    /// nothing to report back as an edit. Empty and non-empty bootstrap get
    /// identical treatment here on purpose.
    fn register_new_buffer(&mut self, generation: GenerationId, id: TextBufferId) {
        self.leases.entry(generation).or_default().buffers.insert(id);
        let baseline = self.system.revision(id).unwrap_or_default();
        self.sync
            .insert((generation, id), BufferSync::synchronized(baseline));
    }

    pub fn system(&self) -> &TextSystem {
        &self.system
    }

    pub fn system_mut(&mut self) -> &mut TextSystem {
        &mut self.system
    }

    pub fn counts(&self) -> TextResourceCounts {
        TextResourceCounts {
            guest_buffer_leases: self.leases.values().map(|l| l.buffers.len()).sum(),
            guest_view_leases: self.leases.values().map(|l| l.views.len()).sum(),
            retained_view_attachments: self.retained_views.values().sum(),
            live_buffers: self.system.live_buffers(),
            live_views: self.system.live_views(),
        }
    }

    /// How many retained attachments exist, counting duplicates.
    pub fn retained_view_attachments(&self) -> usize {
        self.retained_views.values().sum()
    }

    /// Records that the retained UI tree attaches `id`.
    ///
    /// This is the second owner the lifetime law states: a view lives while a
    /// guest lease names it **or** a retained attachment does. Each call is
    /// one attachment, so a view attached at two nodes is retained twice and
    /// must be released twice. Returns `false` without recording anything when
    /// `id` is not a live view, so a stale or invented identity cannot be
    /// turned into an owner that would hide a leak.
    pub fn retain_view_attachment(&mut self, id: TextViewId) -> bool {
        if self.system.view(id).is_err() {
            return false;
        }
        *self.retained_views.entry(id).or_insert(0) += 1;
        true
    }

    /// Drops one retained attachment.
    ///
    /// The view survives if a guest lease (or another retained attachment)
    /// still names it; otherwise collection reclaims it and anything it was
    /// keeping alive. Returns whether one retained attachment actually
    /// existed to drop, and never underflows: a release without a matching
    /// retain is a no-op.
    pub fn release_view_attachment(&mut self, id: TextViewId) -> bool {
        let Some(count) = self.retained_views.get_mut(&id) else {
            return false;
        };
        if *count == 1 {
            self.retained_views.remove(&id);
        } else {
            *count -= 1;
        }
        self.collect_unowned_resources();
        true
    }

    /// Atomically moves a retained attachment from `old` to `new`.
    ///
    /// The new view is acquired **before** the old one is released, so a
    /// replacement cannot destroy the old view in the interval between the
    /// two — the `V7 leaving / V12 arriving` intermediate state the frozen
    /// mutants forbid. Returns `false` without changing anything if `new` is
    /// not a live view.
    pub fn replace_view_attachment(
        &mut self,
        old: Option<TextViewId>,
        new: Option<TextViewId>,
    ) -> bool {
        let Some(new) = new else {
            if let Some(old) = old {
                self.release_view_attachment(old);
            }
            return true;
        };
        if old == Some(new) {
            return true;
        }
        if !self.retain_view_attachment(new) {
            return false;
        }
        if let Some(old) = old {
            self.release_view_attachment(old);
        }
        true
    }

    /// Applies a host-local edit and tells every generation that owes the
    /// buffer about it.
    ///
    /// **The only edit path.** Callers do not reach `system_mut().apply_edit`
    /// directly, because "every host-local edit is recorded" then rests on
    /// each of them remembering to record it — and B4 already added two edit
    /// sites, with more owed to package E. Routing through here makes the
    /// recording structural: a new caller gets it without knowing it exists.
    ///
    /// Recording happens after the edit is applied, and only for an edit that
    /// was applied: a refused edit moved no revision and owes no notification.
    pub fn apply_edit(
        &mut self,
        view: TextViewId,
        edit: TextEdit,
    ) -> Result<AppliedEdit, TextError> {
        let applied = self.system.apply_edit(view, edit)?;
        let Ok(buffer) = self.system.view(view).map(|state| state.buffer()) else {
            return Ok(applied);
        };
        for ((_, owed), state) in self.sync.iter_mut() {
            if *owed == buffer {
                state.record(&applied);
            }
        }
        Ok(applied)
    }

    /// Applies a guest's batch of edits, in the order frozen for C3b:
    ///
    /// ```text
    /// 1  generation screen           already done: ScreenedTextRequest
    /// 2  capability/lease resolve -> NoSuchResource
    /// 3  inbound count/byte bounds-> EditBatchTooLarge, before anything clones
    /// 4  expected revision        -> Conflict(current), before any edit runs
    /// 5  sequential validation    -> InvalidEdit
    /// 6  resulting document size  -> BufferTooLarge
    /// 7  swap the clone in
    /// 8  publish to every relationship but the source
    /// ```
    ///
    /// Steps 5-7 are exactly [`TextSystem::apply_edits_to_buffer`]'s
    /// clone-and-swap; this method's own job is only what surrounds it: the
    /// two refusals that must be decided *before* that call is worth making,
    /// and the fan-out after it succeeds.
    ///
    /// `expected_revision` is checked against the buffer's revision as it
    /// stood before this call touches anything -- a stale revision paired
    /// with an otherwise-malformed batch is `Conflict`, not `InvalidEdit`:
    /// judging byte offsets against a document state the caller never saw is
    /// meaningless, so there is no reason to look at them at all.
    ///
    /// Never echoed to `generation`: it already knows what it submitted. Every
    /// other relationship watching this buffer is recorded normally.
    pub fn apply_guest_edits(
        &mut self,
        generation: GenerationId,
        buffer: OpaqueResourceKey,
        expected_revision: u64,
        edits: Vec<TextEdit>,
    ) -> Result<ApplyEditsOutcome, TextRefusal> {
        let id = self.resolve_buffer_lease(generation, buffer)?;

        let prospective_bytes = edits
            .iter()
            .try_fold(0usize, |total, edit| total.checked_add(edit.replacement.len()));
        let fits = edits.len() <= MAX_PENDING_EDITS
            && prospective_bytes.is_some_and(|total| total <= MAX_PENDING_EDIT_BYTES);
        if !fits {
            return Err(TextRefusal::EditBatchTooLarge);
        }

        let current = self.system.revision(id).unwrap_or_default();
        if current.0 != expected_revision {
            return Ok(ApplyEditsOutcome::Conflict(current.0));
        }

        let applied = self
            .system
            .apply_edits_to_buffer(id, &edits)
            .map_err(|error| match error {
                TextError::BufferTooLarge { .. } => TextRefusal::BufferTooLarge,
                _ => TextRefusal::InvalidEdit,
            })?;

        let resulting = applied
            .last()
            .map(|edit| edit.resulting_revision.0)
            .unwrap_or(current.0);

        for ((owner, owed), state) in self.sync.iter_mut() {
            if *owed == id && *owner != generation {
                for applied_edit in &applied {
                    state.record(applied_edit);
                }
            }
        }

        Ok(ApplyEditsOutcome::Applied(resulting))
    }

    /// Reads an exact byte range (C4b).
    ///
    /// **Strictly observational.** `&self`, not `&mut self`: there is
    /// nothing here *to* mutate, which is the property itself, stated as a
    /// type rather than as a rule this method has to remember to follow.
    /// Verified, not merely asserted: writing `self.sync.get_mut(...)`
    /// anywhere in this body without widening the signature to `&mut self`
    /// is `error[E0596]: cannot borrow self.sync as mutable` -- a compile
    /// error, not a test that could regress. It is not the recovery
    /// mechanism -- see `read-range`'s WIT doc for the double-application
    /// hazard that makes it one.
    ///
    /// The revision comes from the same `&TextBuffer` the bytes are sliced
    /// from, in one synchronous call, so it always names the exact state
    /// those bytes belong to.
    pub fn read_range(
        &self,
        generation: GenerationId,
        buffer: OpaqueResourceKey,
        start: u64,
        end: u64,
    ) -> Result<BridgeRangeContents, TextRefusal> {
        let id = self.resolve_buffer_lease(generation, buffer)?;
        let buffer = self
            .system
            .buffer(id)
            .expect("resolve_buffer_lease already confirmed this buffer is live");
        let start = usize::try_from(start).unwrap_or(usize::MAX);
        let end = usize::try_from(end).unwrap_or(usize::MAX);
        let revision = buffer.revision().0;
        let contents = buffer
            .text()
            .slice(start..end)
            .map_err(|_| TextRefusal::InvalidEdit)?
            .materialize();
        Ok(BridgeRangeContents { contents, revision })
    }

    /// What one generation still owes on one buffer. Read-only: the state is
    /// advanced by [`TextHost::apply_edit`] and by nothing else.
    pub fn sync_state(&self, generation: GenerationId, buffer: TextBufferId) -> Option<&SyncState> {
        self.sync.get(&(generation, buffer)).map(BufferSync::state)
    }

    /// How many (generation, buffer) synchronization relationships exist.
    ///
    /// The counter teardown tests assert a return to baseline against, in the
    /// same spirit as [`TextResourceCounts`].
    pub fn sync_relationships(&self) -> usize {
        self.sync.len()
    }

    /// Whether a `next-edit` caller is genuinely asleep on this relationship.
    ///
    /// Read-only, and it changes nothing: the reason it exists is that "the
    /// guest is suspended" is otherwise unobservable from outside `TextHost`,
    /// and a test that wants to fire a host-local edit *while the guest is
    /// asleep* has no way to confirm that without it -- racing the two and
    /// hoping suspension wins is indistinguishable, from a passing test, from
    /// the edit landing first and the queue answering `next-edit`
    /// immediately, which never touches the wake path at all.
    pub fn has_waiter(&self, generation: GenerationId, buffer: TextBufferId) -> bool {
        self.sync
            .get(&(generation, buffer))
            .is_some_and(BufferSync::has_waiter)
    }

    /// A key this generation is allowed to use, as a buffer id.
    pub fn resolve_buffer_lease(
        &self,
        generation: GenerationId,
        key: OpaqueResourceKey,
    ) -> Result<TextBufferId, TextRefusal> {
        let id = buffer_id(key);
        // Ownership first, identity second. A live id this generation does not
        // hold is not this generation's to touch.
        if !self
            .leases
            .get(&generation)
            .is_some_and(|leases| leases.buffers.contains(&id))
        {
            return Err(TextRefusal::NoSuchResource);
        }
        self.system
            .buffer(id)
            .map(|_| id)
            .map_err(|_| TextRefusal::NoSuchResource)
    }

    pub fn resolve_view_lease(
        &self,
        generation: GenerationId,
        key: OpaqueResourceKey,
    ) -> Result<TextViewId, TextRefusal> {
        let id = view_id(key);
        if !self
            .leases
            .get(&generation)
            .is_some_and(|leases| leases.views.contains(&id))
        {
            return Err(TextRefusal::NoSuchResource);
        }
        self.system
            .view(id)
            .map(|_| id)
            .map_err(|_| TextRefusal::NoSuchResource)
    }

    /// Serves one screened request.
    ///
    /// The ordering inside creation is load-bearing: allocate, **register the
    /// lease**, and only then reply. Registering after the reply would leave a
    /// window in which terminalization lands between the two and produces
    /// exactly the orphan this registry exists to prevent.
    pub fn serve(&mut self, request: ScreenedTextRequest) {
        let generation = request.generation();
        match request.operation() {
            TextOperation::CreateBuffer => match self.system.open_buffer("") {
                Ok(id) => {
                    self.register_new_buffer(generation, id);
                    request.answer(TextAnswer::Created(buffer_key(id)));
                }
                Err(_) => request.refuse(TextRefusal::TooManyBuffers(MAX_TEXT_BUFFERS as u32)),
            },
            // C4a: the same bootstrap, pre-populated. `open_buffer` enforces
            // the size ceiling itself, before allocating anything -- this arm
            // only has to translate its one distinguishable refusal.
            // Anything else `open_buffer` could return here is in practice
            // always `TooManyBuffers`, the same assumption the empty-buffer
            // arm above already makes.
            TextOperation::CreateBufferWithContents { contents } => {
                match self.system.open_buffer(&contents) {
                    Ok(id) => {
                        self.register_new_buffer(generation, id);
                        request.answer(TextAnswer::Created(buffer_key(id)));
                    }
                    Err(TextError::BufferTooLarge { .. }) => {
                        request.refuse(TextRefusal::BufferTooLarge)
                    }
                    Err(_) => {
                        request.refuse(TextRefusal::TooManyBuffers(MAX_TEXT_BUFFERS as u32))
                    }
                }
            }
            TextOperation::CreateView { buffer } => {
                let buffer = match self.resolve_buffer_lease(generation, buffer) {
                    Ok(id) => id,
                    Err(refusal) => return request.refuse(refusal),
                };
                match self.system.open_view(buffer) {
                    Ok(id) => {
                        self.leases.entry(generation).or_default().views.insert(id);
                        request.answer(TextAnswer::Created(view_key(id)));
                    }
                    Err(_) => request.refuse(TextRefusal::TooManyViews(MAX_TEXT_VIEWS as u32)),
                }
            }
            TextOperation::ReleaseBuffer { key } => {
                let id = match self.resolve_buffer_lease(generation, key) {
                    Ok(id) => id,
                    Err(refusal) => return request.refuse(refusal),
                };
                self.release_buffer(generation, id);
                request.answer(TextAnswer::Released);
            }
            TextOperation::ReleaseView { key } => {
                let id = match self.resolve_view_lease(generation, key) {
                    Ok(id) => id,
                    Err(refusal) => return request.refuse(refusal),
                };
                self.release_view(generation, id);
                request.answer(TextAnswer::Released);
            }
            TextOperation::NextEdit { buffer } => {
                let id = match self.resolve_buffer_lease(generation, buffer) {
                    Ok(id) => id,
                    Err(refusal) => return request.refuse(refusal),
                };
                // Minted before the lookup below, so it never overlaps the
                // mutable borrow `admit` needs. Spent whether or not this
                // call ends up installing a waiter -- ids need only be
                // unique, never dense, the same policy `OperationRegistry`
                // already uses for host operation ids.
                let waiter_id = self.next_waiter_id();
                let relationship = self.sync.get_mut(&(generation, id)).expect(
                    "a buffer lease implies a sync relationship: CreateBuffer \
                     installs one and release_generation_leases removes both \
                     together",
                );
                match relationship.admit(MAX_PENDING_EDITS, waiter_id) {
                    Ok(NextEditAdmission::Ready(notification)) => {
                        request.answer(TextAnswer::NextEdit(bridge_notification(notification)));
                    }
                    Ok(NextEditAdmission::Wait(rx)) => {
                        request.answer(TextAnswer::NextEdit(NextEditOutcome::Wait(rx)));
                    }
                    Err(WaitRefusal::AlreadyWaiting) => {
                        request.refuse(TextRefusal::AlreadyWaiting);
                    }
                }
            }
            TextOperation::ApplyEdits {
                buffer,
                expected_revision,
                edits,
            } => {
                let edits: Vec<TextEdit> = edits.into_iter().map(edit_from_bridge).collect();
                match self.apply_guest_edits(generation, buffer, expected_revision, edits) {
                    Ok(outcome) => request.answer(TextAnswer::AppliedEdits(outcome)),
                    Err(refusal) => request.refuse(refusal),
                }
            }
            TextOperation::ReadRange { buffer, start, end } => {
                match self.read_range(generation, buffer, start, end) {
                    Ok(contents) => request.answer(TextAnswer::RangeRead(contents)),
                    Err(refusal) => request.refuse(refusal),
                }
            }
        }
    }

    /// Drops one generation's lease, then lets the subsystem decide whether
    /// the resource dies.
    ///
    /// A buffer with views still on it survives its guest lease: the guest
    /// losing the ability to *name* a document is not the document ending.
    fn release_buffer(&mut self, generation: GenerationId, id: TextBufferId) {
        if let Some(leases) = self.leases.get_mut(&generation) {
            leases.buffers.remove(&id);
        }
        if self.system.views_of(id) == 0 {
            self.system.close_buffer(id);
        }
    }

    fn release_view(&mut self, generation: GenerationId, id: TextViewId) {
        let buffer = self.system.view(id).ok().map(|view| view.buffer());
        if let Some(leases) = self.leases.get_mut(&generation) {
            leases.views.remove(&id);
        }

        // B2e-4: a retained UI attachment is a second owner. Dropping the
        // guest lease must not kill a view the retained tree still shows.
        let still_owned = self.leases.values().any(|l| l.views.contains(&id))
            || self.retained_views.contains_key(&id);
        if !still_owned {
            self.system.close_view(id);
        }

        // The buffer may have been kept alive only by this view. Nobody holds
        // a lease on it and nothing views it, so it goes too.
        if let Some(buffer) = buffer
            && self.system.views_of(buffer) == 0
            && !self.leases.values().any(|l| l.buffers.contains(&buffer))
        {
            self.system.close_buffer(buffer);
        }
    }

    /// A dead generation's leases and whatever they were keeping alive.
    ///
    /// Called from `Host::on_guest_gone` for both a trap and a clean exit,
    /// because a guest whose `run` returned has ended just as completely as
    /// one that trapped — and because Store destruction runs no guest
    /// destructors at all on the trap path.
    ///
    /// Keyed only by generation. Using a `WindowId` to decide what to release
    /// would tie a document's lifetime to a surface.
    pub fn release_generation(&mut self, generation: GenerationId) {
        self.release_generation_leases(generation);
        self.collect_unowned_resources();
    }

    /// Forgets what a generation held. Destroys nothing.
    ///
    /// Split from collection deliberately. Today the two always run together,
    /// so the split buys nothing yet — but B2e-4 added retained UI attachment
    /// as a second owner of a view, and it changes only the *ownership
    /// predicate* below. Without the split it would instead be rewriting a
    /// function whose name says a generation's death destroys documents,
    /// which is precisely the thing B2e exists to disprove.
    fn release_generation_leases(&mut self, generation: GenerationId) {
        self.leases.remove(&generation);
        self.leases.retain(|_, leases| !leases.is_empty());
        // Synchronization state is a fact about the relationship, so it dies
        // with the generation and not with the buffer. A surviving document
        // owes a dead guest nothing.
        self.sync.retain(|(owner, _), _| *owner != generation);
    }

    /// Destroys every text resource nothing owns any more, and nothing else.
    ///
    /// The ownership predicate, and the whole of what B2e-4 extends:
    ///
    /// ```text
    /// a view    is owned while a guest lease names it
    ///           or a retained UI attachment does
    /// a buffer  is owned while a guest lease names it, or a live view does
    /// ```
    ///
    /// Runs after every path that can remove an owner: a guest release, an
    /// attachment detach, a replacement, or a generation teardown. Each of
    /// those paths decrements its own ownership first, so collection is a
    /// pure "what has no owner left?" pass that cannot itself forget to
    /// decrement.
    pub fn collect_unowned_resources(&mut self) {
        let leased_views: HashSet<TextViewId> = self
            .leases
            .values()
            .flat_map(|leases| leases.views.iter().copied())
            .collect();
        let unowned: Vec<TextViewId> = self
            .system
            .views()
            .filter(|id| !leased_views.contains(id) && !self.retained_views.contains_key(id))
            .collect();
        for id in unowned {
            self.system.close_view(id);
        }

        let leased_buffers: HashSet<TextBufferId> = self
            .leases
            .values()
            .flat_map(|leases| leases.buffers.iter().copied())
            .collect();
        let unowned: Vec<TextBufferId> = self
            .system
            .buffers()
            .filter(|id| !leased_buffers.contains(id) && self.system.views_of(*id) == 0)
            .collect();
        for id in unowned {
            self.system.close_buffer(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use instar_kernel::text_bridge::{TextOperation, text_request};

    const G17: GenerationId = GenerationId(17);
    const G18: GenerationId = GenerationId(18);

    /// Serves one operation and returns what the guest would have seen.
    fn serve(
        host: &mut TextHost,
        generation: GenerationId,
        operation: TextOperation,
    ) -> Result<TextAnswer, TextRefusal> {
        let (request, wait) = text_request(generation, operation);
        let screened = request.screen(generation).expect("current");
        host.serve(screened);
        wait.blocking_recv().expect("answered")
    }

    fn create_buffer(host: &mut TextHost, generation: GenerationId) -> OpaqueResourceKey {
        match serve(host, generation, TextOperation::CreateBuffer) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a buffer, got {other:?}"),
        }
    }

    fn create_view(
        host: &mut TextHost,
        generation: GenerationId,
        buffer: OpaqueResourceKey,
    ) -> OpaqueResourceKey {
        match serve(host, generation, TextOperation::CreateView { buffer }) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a view, got {other:?}"),
        }
    }

    /// C4a: a non-empty bootstrap gets exactly the same synchronization
    /// treatment as an empty one -- born `Synchronized` at whatever revision
    /// the fresh buffer holds, with nothing queued.
    #[test]
    fn a_non_empty_bootstrap_is_synchronized_at_baseline_with_an_empty_queue() {
        let mut host = TextHost::new();
        let buffer = match serve(
            &mut host,
            G17,
            TextOperation::CreateBufferWithContents {
                contents: "hello world".to_string(),
            },
        ) {
            Ok(TextAnswer::Created(key)) => key,
            other => panic!("expected a buffer, got {other:?}"),
        };
        let id = buffer_id(buffer);

        assert_eq!(
            host.system().buffer(id).unwrap().len_bytes(),
            "hello world".len(),
            "the guest's own bytes reached the document"
        );
        let state = host.sync_state(G17, id).expect("born with the buffer");
        assert_eq!(
            state.queued(),
            0,
            "the guest supplied these bytes itself; there is nothing to \
             report back as an edit"
        );
        assert!(state.is_synchronized());
    }

    /// A bootstrap payload past the ceiling is refused and leaves nothing
    /// behind -- no lease, no buffer, no sync relationship. Reuses the
    /// coarse-refusal-mapping discipline `apply_guest_edits` established:
    /// `open_buffer` can only fail two ways, and only one of them is
    /// distinguished here.
    #[test]
    fn a_bootstrap_past_the_ceiling_is_refused_and_creates_nothing() {
        let mut host = TextHost::new();
        let oversized = "x".repeat(instar_text::MAX_TEXT_BUFFER_BYTES + 1);

        let refusal = match serve(
            &mut host,
            G17,
            TextOperation::CreateBufferWithContents { contents: oversized },
        ) {
            Err(refusal) => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert_eq!(refusal, TextRefusal::BufferTooLarge);
        assert_eq!(
            host.counts(),
            TextResourceCounts::default(),
            "a refused bootstrap leaves no lease, no buffer, and no \
             relationship behind"
        );
        assert_eq!(host.sync_relationships(), 0);
    }

    /// Every host-local edit reaches the generation that owes the buffer.
    ///
    /// The mutant this exists for is a new edit site calling
    /// `system_mut().apply_edit` directly and skipping the queue -- which is
    /// why `TextHost::apply_edit` is the only edit path rather than a
    /// convenience over one.
    #[test]
    fn a_host_local_edit_is_owed_to_the_generation_that_holds_the_buffer() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view = host.resolve_view_lease(G17, view).expect("leased");
        let id = buffer_id(buffer);

        assert_eq!(
            host.sync_state(G17, id)
                .expect("born with the buffer")
                .queued(),
            0,
            "a fresh buffer owes nothing"
        );

        host.apply_edit(view, TextEdit::insert(0, "hello"))
            .expect("the edit applies");

        let state = host.sync_state(G17, id).expect("still owed");
        assert_eq!(state.queued(), 1);
        assert_eq!(state.queued_bytes(), 5);
        assert!(state.is_synchronized());
    }

    /// A refused edit owes nothing, because it moved nothing.
    #[test]
    fn a_refused_edit_queues_no_notification() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view = host.resolve_view_lease(G17, view).expect("leased");
        let id = buffer_id(buffer);

        assert!(
            host.apply_edit(view, TextEdit::delete(4..9)).is_err(),
            "the buffer is empty, so that range does not exist"
        );
        assert_eq!(host.sync_state(G17, id).expect("present").queued(), 0);
    }

    /// Synchronization state is a fact about a relationship, so it dies with
    /// the generation -- not with the document, which outlives it.
    #[test]
    fn a_dead_generation_owes_nothing_and_leaks_nothing() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let leased = host.resolve_view_lease(G17, view).expect("leased");
        host.apply_edit(leased, TextEdit::insert(0, "hello"))
            .expect("applies");
        assert_eq!(host.sync_relationships(), 1);

        host.release_generation(G17);

        assert_eq!(
            host.sync_relationships(),
            0,
            "a surviving document owes a dead guest nothing"
        );
        assert_eq!(host.counts(), TextResourceCounts::default());
    }

    /// A stale `expected_revision` takes precedence over a malformed range.
    ///
    /// Judging byte offsets against a document state the caller never saw is
    /// meaningless, so revision is checked before the batch is inspected at
    /// all -- a request that is wrong in both ways at once is `Conflict`,
    /// never `InvalidEdit`.
    #[test]
    fn a_stale_revision_reports_conflict_even_with_a_malformed_range() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view = host.resolve_view_lease(G17, view).expect("leased");
        let id = buffer_id(buffer);

        // Advances the revision out from under the batch below, exactly as
        // a slower guest's own edit would.
        host.apply_edit(view, TextEdit::insert(0, "hi"))
            .expect("moves the revision to 1");
        let current = host.system().revision(id).expect("live buffer");
        assert_eq!(current.0, 1);

        // Stale relative to `current`, and separately inverted -- a range no
        // validator could accept regardless of revision.
        let stale_revision = 0;
        let malformed = vec![TextEdit {
            range: 9..3,
            replacement: "nope".to_string(),
        }];

        let outcome = host
            .apply_guest_edits(G17, buffer, stale_revision, malformed)
            .expect("a stale revision is an outcome, not a refusal");

        assert_eq!(
            outcome,
            ApplyEditsOutcome::Conflict(1),
            "revision is judged before the batch is ever inspected"
        );
    }

    /// A guest's own batch is never echoed back to it, but another
    /// generation watching the same buffer hears about it normally.
    ///
    /// No guest-facing API today lets a second generation acquire a lease
    /// on a buffer another generation created -- `CreateView` requires the
    /// caller to already hold the buffer lease -- so this installs a second
    /// relationship directly, the same way C1's tests exercised the sync
    /// state machine without going through lease acquisition at all. The
    /// fan-out this test is about is a property of the sync map, not of how
    /// a relationship came to exist. The origin filter is the only thing
    /// standing between "everyone receives" and "no one receives" -- both
    /// would make this test pass for the wrong reason, which is why it
    /// checks the source and the watcher separately rather than only one.
    #[test]
    fn a_guest_batch_reaches_every_other_relationship_but_not_its_source() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let id = buffer_id(buffer);

        let baseline = host.system().revision(id).unwrap_or_default();
        host.sync.insert((G18, id), BufferSync::synchronized(baseline));

        let outcome = host
            .apply_guest_edits(G17, buffer, baseline.0, vec![TextEdit::insert(0, "hi")])
            .expect("a fresh, well-formed batch applies");
        assert!(matches!(outcome, ApplyEditsOutcome::Applied(_)));

        assert_eq!(
            host.sync_state(G17, id).expect("source relationship").queued(),
            0,
            "the source generation already knows what it applied"
        );
        assert_eq!(
            host.sync_state(G18, id)
                .expect("watching relationship")
                .queued(),
            1,
            "another generation watching the same buffer must hear about it"
        );
    }

    /// C4b: a straightforward read returns the exact bytes and the exact
    /// revision they came from.
    #[test]
    fn read_range_returns_the_exact_bytes_and_revision() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view = host.resolve_view_lease(G17, view).expect("leased");
        host.apply_edit(view, TextEdit::insert(0, "hello world"))
            .expect("applies");

        let read = host.read_range(G17, buffer, 0, 5).expect("valid range");
        assert_eq!(read.contents, "hello");
        assert_eq!(read.revision, 1);
    }

    /// A malformed range gets the same coarse refusal `apply-edits` uses for
    /// the same underlying check, not `instar-text`'s own taxonomy.
    #[test]
    fn read_range_refuses_a_malformed_range() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);

        assert_eq!(
            host.read_range(G17, buffer, 9, 3).unwrap_err(),
            TextRefusal::InvalidEdit
        );
    }

    /// `read-range` is strictly observational: reading a buffer whose
    /// relationship has pending edits queued must not drain, clear, or
    /// otherwise touch that queue. Draining is `next-edit`'s job; treating a
    /// read as recovery is exactly the double-application hazard
    /// `read-range`'s own WIT doc names.
    #[test]
    fn read_range_never_touches_synchronization_state() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view = host.resolve_view_lease(G17, view).expect("leased");
        let id = buffer_id(buffer);

        host.apply_edit(view, TextEdit::insert(0, "hello"))
            .expect("applies, and queues this edit for G17's own relationship \
                     -- host-local edits have no source generation to exempt");
        let queued_before = host.sync_state(G17, id).unwrap().queued();
        let synchronized_before = host.sync_state(G17, id).unwrap().is_synchronized();
        assert_eq!(queued_before, 1, "the fixture must start with something queued");

        let read = host.read_range(G17, buffer, 0, 5).expect("valid range");
        assert_eq!(read.contents, "hello");
        assert_eq!(read.revision, 1);

        assert_eq!(
            host.sync_state(G17, id).unwrap().queued(),
            queued_before,
            "a read must not drain or clear the pending queue"
        );
        assert_eq!(
            host.sync_state(G17, id).unwrap().is_synchronized(),
            synchronized_before,
            "a read must not change synchronized/desynchronized state"
        );
    }

    /// One generation's edits are not owed to another's relationship.
    #[test]
    fn each_generation_owes_only_the_buffers_it_holds() {
        let mut host = TextHost::new();
        let seventeen = create_buffer(&mut host, G17);
        let eighteen = create_buffer(&mut host, G18);
        let view = create_view(&mut host, G17, seventeen);
        let view = host.resolve_view_lease(G17, view).expect("leased");

        host.apply_edit(view, TextEdit::insert(0, "hi"))
            .expect("applies");

        assert_eq!(
            host.sync_state(G17, buffer_id(seventeen)).unwrap().queued(),
            1
        );
        assert_eq!(
            host.sync_state(G18, buffer_id(eighteen)).unwrap().queued(),
            0,
            "a different buffer, and a different relationship"
        );
    }

    // ---------------------------------------------- C2b-1b: NextEdit wiring

    /// A synchronized, empty buffer has nothing to report, so `NextEdit`
    /// resolves to `Wait` rather than an empty batch.
    #[test]
    fn next_edit_on_an_idle_buffer_waits() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);

        match serve(&mut host, G17, TextOperation::NextEdit { buffer }) {
            Ok(TextAnswer::NextEdit(NextEditOutcome::Wait(_))) => {}
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    /// The field-preservation claim: every scalar an `AppliedEdit` carries
    /// survives translation into the kernel's fixed-width vocabulary
    /// unchanged. `bridge_edit` is a straight widening copy, and this is the
    /// test that would fail if a field were ever dropped, swapped, or
    /// narrowed.
    ///
    /// Both queued edits are checked in full, and deliberately for different
    /// reasons: an insertion has an empty range (`start == end`) and a
    /// non-empty replacement, a deletion the reverse. Checking only one of
    /// the two leaves a mutant with nothing to trip -- a swapped start/end is
    /// unobservable against an insertion's `0..0`, and a replacement forced
    /// to always be empty is unobservable against a deletion's. Only the pair
    /// together exercises every field in both directions.
    #[test]
    fn next_edit_reports_a_queued_edit_with_every_field_intact() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view = host.resolve_view_lease(G17, view).expect("leased");

        host.apply_edit(view, TextEdit::insert(0, "hello world"))
            .expect("applies");
        host.apply_edit(view, TextEdit::delete(6..11))
            .expect("applies");

        match serve(&mut host, G17, TextOperation::NextEdit { buffer }) {
            Ok(TextAnswer::NextEdit(NextEditOutcome::Edits(edits))) => {
                assert_eq!(edits.len(), 2, "both queued edits are reported");

                let insert = &edits[0];
                assert_eq!(insert.base_revision, 0);
                assert_eq!(insert.resulting_revision, 1);
                assert_eq!(insert.start, 0);
                assert_eq!(insert.end, 0, "an insertion's range is empty");
                assert_eq!(insert.replacement, "hello world");

                let delete = &edits[1];
                assert_eq!(delete.base_revision, 1);
                assert_eq!(delete.resulting_revision, 2);
                assert_eq!(delete.start, 6);
                assert_eq!(delete.end, 11);
                assert_eq!(delete.replacement, "", "a deletion's replacement is empty");
            }
            other => panic!("expected the queued edits, got {other:?}"),
        }
    }

    /// The desync marker reaches `serve`'s caller as `Desynchronized`, not as
    /// an empty `Edits` batch -- the two mean different things to a guest.
    #[test]
    fn next_edit_reports_desynchronization() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view = host.resolve_view_lease(G17, view).expect("leased");

        for i in 0..=crate::text_sync::MAX_PENDING_EDITS {
            host.apply_edit(view, TextEdit::insert(i, "x"))
                .expect("applies");
        }

        match serve(&mut host, G17, TextOperation::NextEdit { buffer }) {
            Ok(TextAnswer::NextEdit(NextEditOutcome::Desynchronized(_))) => {}
            other => panic!("expected the desync marker, got {other:?}"),
        }
    }

    /// Ownership before identity, the same rule every other operation
    /// enforces: a generation that never leased the buffer is refused before
    /// any sync state is touched.
    #[test]
    fn next_edit_on_a_buffer_this_generation_does_not_lease_is_refused() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);

        assert!(matches!(
            serve(&mut host, G18, TextOperation::NextEdit { buffer }),
            Err(TextRefusal::NoSuchResource)
        ));
    }

    /// At most one outstanding `NextEdit` per (generation, buffer): the
    /// second is a deterministic refusal, not a second sleeper racing the
    /// first.
    ///
    /// The first receiver has to stay alive for this to test anything: a
    /// dropped one is an *abandoned* waiter, which `admit` evicts and
    /// replaces rather than refuses (see C2a's
    /// `an_abandoned_waiter_is_replaced_rather_than_counted`). Only a
    /// genuinely live sleeper produces `AlreadyWaiting`.
    #[test]
    fn a_second_next_edit_while_one_waits_is_refused() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);

        let _first_receiver = match serve(&mut host, G17, TextOperation::NextEdit { buffer }) {
            Ok(TextAnswer::NextEdit(NextEditOutcome::Wait(rx))) => rx,
            other => panic!("expected the first call to wait, got {other:?}"),
        };

        assert!(matches!(
            serve(&mut host, G17, TextOperation::NextEdit { buffer }),
            Err(TextRefusal::AlreadyWaiting)
        ));
    }

    #[test]
    fn creating_a_buffer_and_a_view_registers_both_leases() {
        let mut host = TextHost::new();
        assert_eq!(host.counts(), TextResourceCounts::default());

        let buffer = create_buffer(&mut host, G17);
        assert_eq!(
            host.counts(),
            TextResourceCounts {
                guest_buffer_leases: 1,
                live_buffers: 1,
                ..Default::default()
            }
        );

        let view = create_view(&mut host, G17, buffer);
        let counts = host.counts();
        assert_eq!(counts.guest_view_leases, 1);
        assert_eq!(counts.live_views, 1);

        // The view really refers to that buffer, not to some other one.
        let view_id = host.resolve_view_lease(G17, view).expect("leased");
        assert_eq!(
            host.system().view(view_id).expect("live").buffer(),
            host.resolve_buffer_lease(G17, buffer).expect("leased")
        );
    }

    /// A guest losing the ability to *name* a document is not the document
    /// ending.
    #[test]
    fn dropping_a_buffer_lease_while_a_view_exists_keeps_the_buffer() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);

        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseBuffer { key: buffer }),
            Ok(TextAnswer::Released)
        ));

        let counts = host.counts();
        assert_eq!(counts.guest_buffer_leases, 0, "the lease is gone");
        assert_eq!(counts.live_buffers, 1, "the buffer is not");
        assert!(
            host.resolve_view_lease(G17, view).is_ok(),
            "and so is the view"
        );

        // Now the view goes too, and nothing is left to keep the buffer.
        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseView { key: view }),
            Ok(TextAnswer::Released)
        ));
        assert_eq!(host.counts(), TextResourceCounts::default());
    }

    /// Slot reuse advances the incarnation, so a released key cannot reach
    /// what replaced it.
    #[test]
    fn a_released_key_cannot_reach_the_resource_that_replaced_it() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let first = create_view(&mut host, G17, buffer);
        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseView { key: first }),
            Ok(TextAnswer::Released)
        ));

        let second = create_view(&mut host, G17, buffer);
        assert_eq!(second.slot, first.slot, "the slot was reused");
        assert_ne!(
            second.incarnation, first.incarnation,
            "and the incarnation moved, or the stale key would still work"
        );
        assert_eq!(
            host.resolve_view_lease(G17, first),
            Err(TextRefusal::NoSuchResource)
        );
    }

    /// The registry is authoritative, not the id.
    ///
    /// This is the case a generational id cannot catch on its own: the
    /// resource is genuinely live, and the asking generation simply has no
    /// authority over it.
    #[test]
    fn another_generation_cannot_use_a_live_resource_it_does_not_lease() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);

        assert!(host.resolve_view_lease(G17, view).is_ok(), "17 owns it");
        assert_eq!(
            host.resolve_view_lease(G18, view),
            Err(TextRefusal::NoSuchResource),
            "the id is live and 18 still may not touch it -- ABA protection \
             answers which resource, never whose"
        );
        assert_eq!(
            serve(&mut host, G18, TextOperation::ReleaseView { key: view }).unwrap_err(),
            TextRefusal::NoSuchResource
        );
    }

    /// Both terminal paths, because a clean exit destroys a generation as
    /// completely as a trap does.
    #[test]
    fn a_dead_generation_returns_every_lease_to_baseline() {
        for label in ["clean exit", "trap"] {
            let mut host = TextHost::new();
            let buffer = create_buffer(&mut host, G17);
            create_view(&mut host, G17, buffer);
            create_view(&mut host, G17, buffer);
            assert_eq!(host.counts().live_views, 2, "{label}");

            host.release_generation(G17);

            assert_eq!(
                host.counts(),
                TextResourceCounts::default(),
                "{label}: leases and resources both returned to baseline"
            );
        }
    }

    /// One generation dying does not touch another's.
    #[test]
    fn releasing_one_generation_leaves_another_alone() {
        let mut host = TextHost::new();
        let seventeen = create_buffer(&mut host, G17);
        let eighteen = create_buffer(&mut host, G18);

        host.release_generation(G17);

        assert_eq!(
            host.resolve_buffer_lease(G17, seventeen),
            Err(TextRefusal::NoSuchResource)
        );
        assert!(host.resolve_buffer_lease(G18, eighteen).is_ok());
        assert_eq!(host.counts().live_buffers, 1);
    }

    /// The race, in both of its two allowed shapes.
    ///
    /// There is no third: an orphaned resource with no owner at all is the
    /// outcome the registry exists to make impossible.
    #[test]
    fn a_create_racing_a_dead_generation_has_exactly_two_outcomes() {
        // Creation wins: the resource exists, its lease is registered, and
        // teardown then reclaims it.
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        create_view(&mut host, G17, buffer);
        assert_eq!(host.counts().live_views, 1);
        host.release_generation(G17);
        assert_eq!(
            host.counts(),
            TextResourceCounts::default(),
            "creation won, and teardown reclaimed what it made"
        );

        // Terminal wins: the request is screened against a retired generation
        // and never reaches the subsystem at all.
        let mut host = TextHost::new();
        host.release_generation(G17);
        let (request, wait) = text_request(G17, TextOperation::CreateBuffer);
        let stale = request.screen(GenerationId(0)).expect_err("retired");
        assert_eq!(stale, G17);
        assert_eq!(
            wait.blocking_recv().expect("answered").unwrap_err(),
            TextRefusal::StaleGeneration
        );
        assert_eq!(
            host.counts(),
            TextResourceCounts::default(),
            "terminal won, and nothing was allocated"
        );
    }

    /// The wiring: `on_guest_gone` releases by generation, and the window it
    /// names has no say.
    ///
    /// The obvious fault -- releasing only what belongs to `window_id` -- is
    /// not expressible here, because `release_generation` takes no window. The
    /// risk this test covers is the other one: that the call is placed *below*
    /// the clean-exit early return, where a guest whose `run` returned would
    /// leak everything it held.
    #[test]
    fn a_guest_that_exits_cleanly_releases_its_leases_through_the_host() {
        for (label, error) in [("clean exit", None), ("trap", Some("boom".to_string()))] {
            let mut host = crate::Host::new();
            let buffer = create_buffer(host.text_resources_mut(), G17);
            create_view(host.text_resources_mut(), G17, buffer);
            assert_eq!(host.text_resources().counts().live_views, 1, "{label}");

            // A window id that names no window at all: resource lifetime has
            // nothing to do with whether a surface exists.
            host.on_guest_gone(instar_window::WindowId::from_raw(99), G17, error);

            assert_eq!(
                host.text_resources().counts(),
                TextResourceCounts::default(),
                "{label}: a dead generation's leases go, whatever the window"
            );
        }
    }

    /// A view for a buffer the asking generation does not hold is refused
    /// before anything is allocated.
    #[test]
    fn a_view_cannot_be_opened_on_a_buffer_this_generation_does_not_lease() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);

        assert_eq!(
            serve(&mut host, G18, TextOperation::CreateView { buffer }).unwrap_err(),
            TextRefusal::NoSuchResource
        );
        assert_eq!(host.counts().live_views, 0, "and nothing was allocated");
    }

    /// The first half of B2e-4's view-lifetime OR: dropping the guest lease
    /// does not kill an attached view.
    #[test]
    fn a_retained_attachment_keeps_a_view_alive_after_its_guest_lease_goes() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view_id = host.resolve_view_lease(G17, view).expect("leased");

        assert!(host.retain_view_attachment(view_id));
        assert_eq!(host.counts().retained_view_attachments, 1);

        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseView { key: view }),
            Ok(TextAnswer::Released)
        ));

        let counts = host.counts();
        assert_eq!(counts.guest_view_leases, 0, "the lease is gone");
        assert_eq!(counts.retained_view_attachments, 1);
        assert_eq!(counts.live_views, 1, "the attachment is the second owner");
        assert_eq!(counts.live_buffers, 1, "and it keeps the buffer too");

        assert!(host.release_view_attachment(view_id));
        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseBuffer { key: buffer }),
            Ok(TextAnswer::Released)
        ));
        assert_eq!(
            host.counts(),
            TextResourceCounts::default(),
            "the final release returns everything to baseline"
        );
    }

    /// The second half of the OR: detaching does not kill a view the guest
    /// still holds.
    #[test]
    fn detaching_a_view_the_guest_still_holds_keeps_it_alive() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view_id = host.resolve_view_lease(G17, view).expect("leased");
        assert!(host.retain_view_attachment(view_id));

        assert!(host.release_view_attachment(view_id));

        let counts = host.counts();
        assert_eq!(
            counts.retained_view_attachments, 0,
            "the attachment is gone"
        );
        assert_eq!(counts.guest_view_leases, 1, "the lease is not");
        assert_eq!(counts.live_views, 1);

        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseView { key: view }),
            Ok(TextAnswer::Released)
        ));
        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseBuffer { key: buffer }),
            Ok(TextAnswer::Released)
        ));
        assert_eq!(host.counts(), TextResourceCounts::default());
    }

    /// Teardown kills a dead generation's leases, never an attached view.
    #[test]
    fn a_dead_generation_keeps_retained_views() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view_id = host.resolve_view_lease(G17, view).expect("leased");
        assert!(host.retain_view_attachment(view_id));

        host.release_generation(G17);

        let counts = host.counts();
        assert_eq!(counts.guest_view_leases, 0);
        assert_eq!(counts.retained_view_attachments, 1);
        assert_eq!(counts.live_views, 1);
        assert_eq!(counts.live_buffers, 1);

        assert!(host.release_view_attachment(view_id));
        assert_eq!(host.counts(), TextResourceCounts::default());
    }

    /// Replacement acquires the new attachment before releasing the old one:
    /// the old view is destroyed only when nothing owns it any more, and the
    /// retained count never leaves exactly one.
    #[test]
    fn replacement_collects_the_old_view_and_keeps_exactly_one_attachment() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let first = create_view(&mut host, G17, buffer);
        let second = create_view(&mut host, G17, buffer);
        let first_id = host.resolve_view_lease(G17, first).expect("leased");
        let second_id = host.resolve_view_lease(G17, second).expect("leased");
        assert!(host.retain_view_attachment(first_id));

        // Drop first's guest lease while it is still attached. The attachment
        // is the second owner, so the view survives until it is replaced.
        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseView { key: first }),
            Ok(TextAnswer::Released)
        ));
        assert_eq!(host.counts().live_views, 2);

        assert!(host.replace_view_attachment(Some(first_id), Some(second_id)));

        let counts = host.counts();
        assert_eq!(counts.retained_view_attachments, 1);
        assert_eq!(counts.live_views, 1);
        assert!(host.system().view(second_id).is_ok(), "the new view lives");
        assert!(
            host.system().view(first_id).is_err(),
            "the old view had no owner left, so collection took it"
        );

        // A same-view replacement is a no-op, not a remove-and-re-add.
        assert!(host.replace_view_attachment(Some(second_id), Some(second_id)));
        assert_eq!(host.counts().retained_view_attachments, 1);
    }

    #[test]
    fn a_stale_or_unretained_view_is_not_touched() {
        let mut host = TextHost::new();
        let ghost = TextViewId {
            id: 900,
            generation: 0,
        };

        assert!(
            !host.retain_view_attachment(ghost),
            "a ghost cannot be an owner"
        );
        assert_eq!(host.counts().retained_view_attachments, 0);
        assert!(
            !host.release_view_attachment(ghost),
            "releasing what was never retained is a no-op"
        );
        assert!(
            !host.replace_view_attachment(None, Some(ghost)),
            "replacing onto a ghost changes nothing"
        );
        assert_eq!(host.counts(), TextResourceCounts::default());
    }

    /// Two retained nodes can attach the same view; each needs its own
    /// release, and the count reports the sum, not the number of distinct
    /// views.
    #[test]
    fn duplicate_retains_are_counted_and_released_individually() {
        let mut host = TextHost::new();
        let buffer = create_buffer(&mut host, G17);
        let view = create_view(&mut host, G17, buffer);
        let view_id = host.resolve_view_lease(G17, view).expect("leased");

        assert!(host.retain_view_attachment(view_id));
        assert!(host.retain_view_attachment(view_id));
        assert_eq!(host.retained_view_attachments(), 2);

        assert!(host.release_view_attachment(view_id));
        assert_eq!(
            host.retained_view_attachments(),
            1,
            "one of two attachments is gone, the view stays"
        );
        assert_eq!(host.counts().live_views, 1);

        assert!(host.release_view_attachment(view_id));
        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseView { key: view }),
            Ok(TextAnswer::Released)
        ));
        assert!(matches!(
            serve(&mut host, G17, TextOperation::ReleaseBuffer { key: buffer }),
            Ok(TextAnswer::Released)
        ));
        assert_eq!(
            host.counts(),
            TextResourceCounts::default(),
            "the final of two releases returns everything to baseline"
        );
        assert!(
            !host.release_view_attachment(view_id),
            "a release without a matching retain cannot underflow"
        );
    }
}
