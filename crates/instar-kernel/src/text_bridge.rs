//! The kernel's half of the text-resource bridge (B2e-1).
//!
//! `instar-kernel` knows that the world imports `text-buffer` and `text-view`
//! — generated bindings force that, and pretending otherwise would be a
//! fiction. What it does not know is what a buffer *contains*, how a view
//! references one, what a selection is, or when a document should die. All of
//! that lives behind a [`TextSink`] the embedder installs, exactly as
//! [`crate::bridge::CommitSink`] hides what a UI batch means.
//!
//! ```text
//! guest calls create-empty-buffer()
//!   -> kernel wraps it in a TextRequest with a one-shot reply
//!   -> TextSink hands it to whoever owns the text subsystem
//!   -> guest stays suspended, costing nothing
//!   -> that owner replies with an opaque key, or a refusal
//! ```
//!
//! # The key says nothing about what it names
//!
//! [`OpaqueResourceKey`] is two `u32`s. The kernel cannot ask it what buffer
//! it refers to, because it has nothing to ask. `instar-host` is the only
//! layer that translates one into a `TextBufferId` or a `TextViewId`, which is
//! why there is no `instar-kernel -> instar-text` dependency.
//!
//! # `incarnation`, not `generation`
//!
//! There are two unrelated generations in this system:
//!
//! ```text
//! GenerationId            one Store, one component instance, one guest task
//! the key's incarnation   ABA protection for one host resource slot
//! ```
//!
//! Naming both `generation` in bridge code — where they appear within three
//! lines of each other — is a defect waiting to be written.
//!
//! # Screening comes before the operation is readable
//!
//! Same discipline as a commit, and for the same reason: a request from a dead
//! generation must be refused before it can cause any work. A [`TextRequest`]
//! offers no way to see what was asked; only the [`ScreenedTextRequest`] that
//! survives [`TextRequest::screen`] has an operation on it.

use tokio::sync::oneshot;

use crate::runtime::GenerationId;

/// The largest attachment side table a commit may carry.
///
/// This is the maximum *useful* attachment-table cardinality: a valid
/// snapshot has at most `MAX_NODES` nodes and every text view refers to at
/// most one slot, so no valid tree can reference more than `MAX_NODES`
/// distinct slots; entries beyond that cannot contribute, so Instar refuses
/// them to bound resolution work.
///
/// Unreferenced and duplicate entries remain legal *within* the bound. The
/// table is a guest-supplied side table, not a projection of the wire tree:
/// it may name nothing, and it may name the same text view many times.
///
/// It is therefore a policy on table size, NOT a theorem falling out of wire
/// validity — a one-node tree supplying 4097 entries is meaningful under
/// Instar's semantics and is refused by this rule alone.
///
/// The check occurs AFTER Component Model list lifting: generated `bindgen!`
/// materializes a `list<T>` into an owned Rust collection before the host
/// impl is entered, so the bound governs Instar's own work; it is NOT a claim
/// that the guest cannot cause the canonical ABI to lift a larger argument
/// first.
///
/// The literal 4096 cannot reference `MAX_NODES` directly: `instar-kernel`
/// must not depend on `instar-ui-protocol` (see the forbidden-dependency note
/// in its Cargo.toml description). The relationship is asserted in
/// `instar-host` instead (see tests).
pub const MAX_TEXT_ATTACHMENTS: usize = 4_096;

/// A host resource, named without saying what kind of thing it is.
///
/// Deliberately not `TextBufferId` or `TextViewId`: those are `instar-text`
/// vocabulary, and a kernel that could name them would be a kernel that could
/// grow opinions about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpaqueResourceKey {
    pub slot: u32,
    /// Which occupant of `slot` this is. See the module docs on naming.
    pub incarnation: u32,
}

/// A guest's capability onto a host-owned buffer.
///
/// Separate Rust types for buffers and views, so a buffer handle cannot be
/// accepted where a view is wanted. The Component Model already keeps them
/// apart on the wire; this keeps them apart in the host too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestTextBuffer {
    pub key: OpaqueResourceKey,
}

/// A guest's capability onto a host-owned view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestTextView {
    pub key: OpaqueResourceKey,
}

/// What a guest asked the text subsystem to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOperation {
    CreateBuffer,
    CreateView { buffer: OpaqueResourceKey },
    ReleaseBuffer { key: OpaqueResourceKey },
    ReleaseView { key: OpaqueResourceKey },
}

/// Why a text request was refused.
///
/// Every variant is an ordinary refusal a guest should be able to handle, not
/// a bug. Corrupted host state traps instead; `trappable` bindings grant the
/// *ability* to trap, not an instruction to use it for everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRefusal {
    /// The request outlived its generation.
    StaleGeneration,
    /// No sink, a disconnected one, or a host shutting down. Never a hang: a
    /// capability boundary that can park a guest is worse than one that says
    /// no.
    HostUnavailable,
    TooManyBuffers(u32),
    TooManyViews(u32),
    /// The key named nothing this generation can reach.
    NoSuchResource,
}

/// Why a commit's text-view attachments were refused.
///
/// A different family from [`TextRefusal`]: that is about text-resource
/// requests, this is about the side table a commit carries. The kernel can
/// name the table and its rules without being able to resolve a single key;
/// `instar-host` performs the actual resolution in later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentRefusal {
    /// The side table exceeded [`MAX_TEXT_ATTACHMENTS`].
    TooManyAttachments,
    /// The key failed to resolve to a lease this generation owns — a stale
    /// incarnation or cross-generation use, not merely an unknown name.
    UnavailableTextView,
    /// The entry named a slot outside the side table it indexes into.
    AttachmentOutOfRange,
    /// Two live NodeKeys reached one TextViewId. Duplicate side-table entries
    /// are legal; this is the illegal duplicate-owner state.
    TextViewAlreadyAttached,
}

/// What the text subsystem did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAnswer {
    Created(OpaqueResourceKey),
    Released,
}

/// Answers exactly once, including on the paths that forget to.
///
/// The same guard `CommitRequest` uses, and for the same reason: "the guest is
/// woken" is a property of the type here, not a property of every exit path
/// remembering.
#[derive(Debug)]
struct Reply(Option<oneshot::Sender<Result<TextAnswer, TextRefusal>>>);

impl Reply {
    fn send(mut self, verdict: Result<TextAnswer, TextRefusal>) {
        if let Some(reply) = self.0.take() {
            let _ = reply.send(verdict);
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(reply) = self.0.take() {
            let _ = reply.send(Err(TextRefusal::HostUnavailable));
        }
    }
}

/// A guest's text request, waiting for whoever owns the text subsystem.
///
/// Has no accessor for the operation. The only route to it is
/// [`TextRequest::screen`], which is also the generation check — so "refuse a
/// dead generation before doing anything on its behalf" cannot be got wrong by
/// writing the two steps in the other order.
#[derive(Debug)]
pub struct TextRequest {
    generation: GenerationId,
    operation: TextOperation,
    reply: Reply,
}

impl TextRequest {
    /// Cheap, and the only thing readable before screening.
    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    /// The first gate: is this from the generation still running?
    pub fn screen(self, current: GenerationId) -> Result<ScreenedTextRequest, GenerationId> {
        if self.generation != current {
            let stale = self.generation;
            self.reply.send(Err(TextRefusal::StaleGeneration));
            return Err(stale);
        }
        Ok(ScreenedTextRequest {
            generation: self.generation,
            operation: self.operation,
            reply: self.reply,
        })
    }

    /// Refuses without screening, for a host that cannot serve this at all.
    pub fn refuse(self, reason: TextRefusal) {
        self.reply.send(Err(reason));
    }
}

/// A text request that has passed the generation check.
#[derive(Debug)]
pub struct ScreenedTextRequest {
    generation: GenerationId,
    operation: TextOperation,
    reply: Reply,
}

impl ScreenedTextRequest {
    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    pub fn operation(&self) -> TextOperation {
        self.operation
    }

    pub fn answer(self, answer: TextAnswer) {
        self.reply.send(Ok(answer));
    }

    pub fn refuse(self, reason: TextRefusal) {
        self.reply.send(Err(reason));
    }
}

/// Where text requests go once the kernel has wrapped them.
///
/// Implemented by whoever owns the text subsystem — in Instar, the thread that
/// owns the retained tree. Returning the request on failure rather than
/// swallowing it leaves the caller holding something it can still answer.
pub trait TextSink: Send + Sync + 'static {
    fn submit(&self, request: TextRequest) -> Result<(), TextRequest>;
}

/// Builds a request and the channel the guest suspends on.
pub fn text_request(
    generation: GenerationId,
    operation: TextOperation,
) -> (
    TextRequest,
    oneshot::Receiver<Result<TextAnswer, TextRefusal>>,
) {
    let (reply, wait) = oneshot::channel();
    (
        TextRequest {
            generation,
            operation,
            reply: Reply(Some(reply)),
        },
        wait,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEN1: GenerationId = GenerationId(1);
    const GEN2: GenerationId = GenerationId(2);

    #[tokio::test]
    async fn a_stale_request_is_refused_before_its_operation_can_be_read() {
        let (request, wait) = text_request(GEN1, TextOperation::CreateBuffer);

        let stale = request.screen(GEN2).expect_err("generation 1 is gone");

        assert_eq!(stale, GEN1);
        assert_eq!(
            wait.await.expect("answered"),
            Err(TextRefusal::StaleGeneration)
        );
    }

    /// A dropped request answers rather than parking the guest forever.
    #[tokio::test]
    async fn a_dropped_request_still_wakes_the_guest() {
        let (request, wait) = text_request(GEN1, TextOperation::CreateBuffer);
        drop(request);
        assert_eq!(
            wait.await.expect("answered"),
            Err(TextRefusal::HostUnavailable),
            "a capability boundary that can hang is worse than one that refuses"
        );
    }

    #[tokio::test]
    async fn a_screened_request_carries_its_operation_and_answers_once() {
        let key = OpaqueResourceKey {
            slot: 3,
            incarnation: 1,
        };
        let (request, wait) = text_request(GEN1, TextOperation::CreateView { buffer: key });

        let screened = request.screen(GEN1).expect("current generation");
        assert_eq!(
            screened.operation(),
            TextOperation::CreateView { buffer: key }
        );
        screened.answer(TextAnswer::Created(OpaqueResourceKey {
            slot: 9,
            incarnation: 0,
        }));

        assert_eq!(
            wait.await.expect("answered"),
            Ok(TextAnswer::Created(OpaqueResourceKey {
                slot: 9,
                incarnation: 0
            }))
        );
    }
}
