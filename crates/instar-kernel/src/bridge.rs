//! The kernel's half of the runtime/main-thread bridge (WP7B1).
//!
//! `instar-kernel` does not know what a UI tree is, and this module is careful
//! not to teach it. What it adds is a way for the guest's `commit` call to be
//! *answered by somebody else* — a [`CommitSink`] the embedder installs, which
//! in Instar is the thread that owns winit's event loop and the retained tree.
//!
//! ```text
//! guest calls commit(batch, text_views).await
//!   -> kernel wraps it in a CommitRequest with a one-shot reply
//!   -> CommitSink hands it to whoever owns presentation
//!   -> guest stays suspended, costing nothing
//!   -> that owner replies; the guest wakes with a revision or a rejection
//! ```
//!
//! # The bytes are unreachable until the generation has been checked
//!
//! `docs/PHASE-1.md` makes the main thread's ordering normative, and its first
//! rule is that a stale generation is rejected *before* its batch is decoded —
//! otherwise a dead guest can still make the host spend parser and allocation
//! work on its behalf.
//!
//! That rule is enforced by the type, not by a comment: a [`CommitRequest`]
//! offers no way at all to see the batch *or the text-view side table it
//! carries*. The only thing you can do with one is [`CommitRequest::screen`]
//! it against the current generation, and only the [`ScreenedCommit`] that
//! comes back has bytes or keys on it. The side table is an opaque-key list
//! in 3a — resolving it to `TextViewId`s belongs to the host, and belongs
//! *after* screening exactly like the batch does. Screening also replies on
//! the failure path, so a rejected commit cannot be left parked by forgetting
//! to answer it.
//!
//! # Every commit is answered exactly once, including the ones nobody handles
//!
//! A guest suspended in `commit().await` forever is the worst failure this
//! bridge can produce: it is silent, it survives every timeout, and no counter
//! goes up. So the reply is not something a code path has to remember to send.
//! [`Reply`] owns the one-shot and answers [`CommitRejection::HostUnavailable`]
//! from its own `Drop`, which makes exactly-once *total*: a request dropped on
//! any exit path — a panicking main thread, an early `return`, a queue that
//! disconnected, a shutdown between submission and application — resolves the
//! guest rather than stranding it. Reaching the bad state takes deliberately
//! leaking the reply; forgetting cannot get you there.

use tokio::sync::oneshot;

use crate::runtime::GenerationId;
use crate::text_bridge::{AttachmentRefusal, OpaqueResourceKey};

/// An event on its way to a guest.
///
/// A newtype over bytes rather than a bare `Vec<u8>` because the two
/// directions of this bridge carry byte vectors for entirely different
/// reasons, and mixing up an event with a committed batch is a mistake worth
/// making unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestEvent(pub Vec<u8>);

impl GuestEvent {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// Why a commit was not applied.
///
/// Mirrors the `commit-error` variants in `wit/kernel.wit`; it exists
/// separately so the host can reject a batch without linking the generated
/// bindings' types into its own vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitRejection {
    /// The batch failed to decode or to validate.
    Invalid(String),
    /// One of the commit's text-view attachments was refused, before any tree
    /// work happened.
    Attachment(AttachmentRefusal),
    /// The committing generation has been superseded.
    StaleGeneration,
    /// Nobody is left to apply it — the presentation side is shutting down.
    HostUnavailable,
}

/// The guest's end of one commit, and the guarantee that it is answered.
///
/// Every commit resolves to exactly one of four terminal verdicts — a
/// revision, or [`CommitRejection::Invalid`], [`CommitRejection::Attachment`],
/// [`CommitRejection::StaleGeneration`], or
/// [`CommitRejection::HostUnavailable`] — and the last of those is the default
/// rather than a case anyone has to handle. Dropping a `Reply` without
/// answering *is* `HostUnavailable`, because a presentation side that
/// disappears mid-commit does not get the chance to be tidy about it.
///
/// This is why it is a distinct type rather than a bare
/// `oneshot::Sender`. Relying on the receiver noticing a closed channel would
/// work for the paths that drop the sender, but it makes "the guest is woken"
/// a property of every future exit path remembering something, instead of a
/// property of the type. Here it is the latter.
#[derive(Debug)]
struct Reply(Option<oneshot::Sender<Result<u64, CommitRejection>>>);

impl Reply {
    /// Answers, consuming the guard so `Drop` has nothing left to do.
    fn send(mut self, verdict: Result<u64, CommitRejection>) {
        // A send error means the guest went away first — its generation was
        // destroyed while this commit was in flight. Nothing to do about it,
        // and nothing wrong with it.
        if let Some(reply) = self.0.take() {
            let _ = reply.send(verdict);
        }
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(reply) = self.0.take() {
            let _ = reply.send(Err(CommitRejection::HostUnavailable));
        }
    }
}

/// A guest's commit, waiting for the side that owns the retained tree.
///
/// Deliberately has no accessor for the batch or for the text-view side table
/// it carries. See the module docs: the only route to either is
/// [`CommitRequest::screen`], which is also the generation check, so the
/// normative "check before decoding" ordering cannot be got wrong by writing
/// the two steps in the other order.
#[derive(Debug)]
pub struct CommitRequest {
    generation: GenerationId,
    batch: Vec<u8>,
    text_views: Vec<OpaqueResourceKey>,
    reply: Reply,
}

impl CommitRequest {
    /// Which generation committed this. Cheap, and the only thing readable
    /// before screening.
    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    /// The first and cheapest gate: is this from the generation still running?
    ///
    /// On a mismatch the guest is answered with [`CommitRejection::StaleGeneration`]
    /// here, and the batch is dropped without ever being looked at.
    pub fn screen(self, current: GenerationId) -> Result<ScreenedCommit, GenerationId> {
        if self.generation != current {
            let stale = self.generation;
            self.reply.send(Err(CommitRejection::StaleGeneration));
            return Err(stale);
        }
        Ok(ScreenedCommit {
            generation: self.generation,
            batch: self.batch,
            text_views: self.text_views,
            reply: self.reply,
        })
    }
}

/// A commit that has passed the generation check, and may therefore be
/// decoded and have its text-view side table read.
#[derive(Debug)]
pub struct ScreenedCommit {
    generation: GenerationId,
    batch: Vec<u8>,
    text_views: Vec<OpaqueResourceKey>,
    reply: Reply,
}

impl ScreenedCommit {
    pub fn generation(&self) -> GenerationId {
        self.generation
    }

    pub fn batch(&self) -> &[u8] {
        &self.batch
    }

    /// The text-view side table, in the order the guest supplied it.
    ///
    /// Unreachable before screening — a stale commit's keys must not be
    /// resolvable any more than its batch must be decodable.
    pub fn text_view_keys(&self) -> &[OpaqueResourceKey] {
        &self.text_views
    }

    /// Applied. `revision` is the host's identifier for the interface that is
    /// now current.
    ///
    /// Called after layout, not merely after the tree is swapped: the guest
    /// resuming from `commit().await` should mean "the host accepted this as a
    /// usable presentation state", not "the bytes entered a tree". Rendering
    /// need not have happened — only the work that could still have refused
    /// the batch.
    pub fn accept(self, revision: u64) {
        self.reply.send(Ok(revision));
    }

    /// Not applied, and the previous interface still stands.
    pub fn reject(self, reason: CommitRejection) {
        self.reply.send(Err(reason));
    }
}

/// Where commits go once the kernel has wrapped them.
///
/// Implemented by whoever owns the retained tree. The kernel calls this from
/// the runtime thread, inside the suspended guest call, so an implementation
/// may take as long as it needs — it is the *guest* that waits, not a
/// platform-owned thread.
///
/// Returning the request on failure rather than swallowing it means the caller
/// still holds something it can answer, which is how a shutting-down host
/// turns a doomed commit into [`CommitRejection::HostUnavailable`] instead of
/// leaving the guest parked forever.
pub trait CommitSink: Send + Sync + 'static {
    fn submit(&self, request: CommitRequest) -> Result<(), CommitRequest>;
}

/// Builds a request and the channel the guest suspends on.
///
/// Public because a request is the whole vocabulary of a [`CommitSink`], and
/// anything testing or reimplementing one needs to be able to make a request
/// without a guest behind it.
pub fn commit_request(
    generation: GenerationId,
    batch: Vec<u8>,
    text_views: Vec<OpaqueResourceKey>,
) -> (
    CommitRequest,
    oneshot::Receiver<Result<u64, CommitRejection>>,
) {
    let (reply, wait) = oneshot::channel();
    (
        CommitRequest {
            generation,
            batch,
            text_views,
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
    async fn screening_a_current_generation_yields_the_batch() {
        let (request, wait) = commit_request(GEN1, b"batch".to_vec(), Vec::new());
        let screened = request.screen(GEN1).expect("gen1 is current");
        assert_eq!(screened.batch(), b"batch");
        screened.accept(7);
        assert_eq!(wait.await.expect("replied"), Ok(7));
    }

    #[tokio::test]
    async fn screening_a_stale_generation_replies_without_exposing_the_batch() {
        let (request, wait) = commit_request(GEN1, b"batch".to_vec(), Vec::new());
        assert_eq!(
            request.screen(GEN2).unwrap_err(),
            GEN1,
            "a superseded generation's commit must not become screened"
        );
        assert_eq!(
            wait.await.expect("replied"),
            Err(CommitRejection::StaleGeneration),
            "screening answers the guest itself, so a rejection cannot be forgotten"
        );
    }

    /// Dropping a request without answering it is a real case — it is what a
    /// host tearing down mid-commit does — and the guest must not be left
    /// parked on it.
    ///
    /// The assertion is on the *verdict*, not merely on the channel having
    /// closed. A closed channel would also unblock the guest, but it would
    /// leave "the guest is woken" resting on nobody ever holding a request
    /// past its usefulness. A delivered `HostUnavailable` is the type doing
    /// the work.
    #[tokio::test]
    async fn dropping_a_request_answers_it_as_host_unavailable() {
        let (request, wait) = commit_request(GEN1, b"batch".to_vec(), Vec::new());
        drop(request);
        assert_eq!(
            wait.await
                .expect("the drop guard replies rather than closing"),
            Err(CommitRejection::HostUnavailable),
        );
    }

    /// The same guarantee after screening. This is the window that matters
    /// most: the batch has been accepted for decoding, so the guest has every
    /// reason to expect an answer, and the host is mid-way through the one
    /// sequence that can panic on it.
    #[tokio::test]
    async fn dropping_a_screened_commit_answers_it_too() {
        let (request, wait) = commit_request(GEN1, b"batch".to_vec(), Vec::new());
        drop(request.screen(GEN1).expect("gen1 is current"));
        assert_eq!(
            wait.await
                .expect("the drop guard replies rather than closing"),
            Err(CommitRejection::HostUnavailable),
        );
    }

    /// Exactly once, not at least once: answering must disarm the guard, or
    /// every accepted commit would be followed by a phantom `HostUnavailable`
    /// on a channel the guest may already have moved past.
    #[tokio::test]
    async fn answering_disarms_the_guard() {
        let (request, mut wait) = commit_request(GEN1, b"batch".to_vec(), Vec::new());
        request.screen(GEN1).expect("gen1 is current").accept(7);

        assert_eq!(wait.try_recv(), Ok(Ok(7)));
        assert!(
            matches!(wait.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
            "a second verdict must not follow the first"
        );
    }

    /// The side table is part of the screened commit, not of the request:
    /// exactly as with the batch, the only way to read the keys is to pass
    /// the generation gate first. Order and contents must survive unchanged.
    #[tokio::test]
    async fn screened_commit_carries_text_view_keys_unchanged_and_in_order() {
        let keys = vec![
            OpaqueResourceKey {
                slot: 3,
                incarnation: 1,
            },
            OpaqueResourceKey {
                slot: 9,
                incarnation: 2,
            },
            OpaqueResourceKey {
                slot: 3,
                incarnation: 1,
            },
        ];
        let (request, wait) = commit_request(GEN1, b"batch".to_vec(), keys.clone());

        let screened = request.screen(GEN1).expect("gen1 is current");
        assert_eq!(
            screened.text_view_keys(),
            keys.as_slice(),
            "the side table must arrive at the host in the exact order the guest supplied"
        );
        screened.accept(7);
        assert_eq!(wait.await.expect("replied"), Ok(7));
    }

    /// A stale commit's keys are unreadable before screening, exactly like its
    /// batch. The `CommitRequest` type has no `text_view_keys` accessor, so
    /// "refused before the keys are readable" is a property of the API rather
    /// than a convention; this test pins the refusal itself.
    #[tokio::test]
    async fn a_stale_commit_refuses_before_its_keys_can_be_read() {
        let keys = vec![OpaqueResourceKey {
            slot: 3,
            incarnation: 1,
        }];
        let (request, wait) = commit_request(GEN1, b"batch".to_vec(), keys);

        assert_eq!(
            request.screen(GEN2).unwrap_err(),
            GEN1,
            "a superseded generation's commit must not become screened"
        );
        assert_eq!(
            wait.await.expect("replied"),
            Err(CommitRejection::StaleGeneration),
            "the keys are dropped with the batch, and the guest is told so"
        );
    }
}
