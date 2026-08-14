//! The guest that misbehaves on request (WP8).
//!
//! The bridge's interesting properties are all about what happens when things
//! go wrong: while a generation is dying, while a hundred events are queued
//! behind a commit, while a host operation is still in flight, while a reply
//! will never arrive. A well-behaved guest cannot exercise any of that.
//!
//! Every failure mode here is reachable the same way any other interaction is:
//! the host hit-tests a click and sends an event. Nothing about the wire format
//! knows these buttons are special, and nothing in the host has a back door for
//! them — which is the point. A hostile guest is not a different kind of guest,
//! it is a guest doing things the protocol permits and the host must survive.
//!
//! # What each mode is for
//!
//! | Button | What it does | What it proves the host survives |
//! |---|---|---|
//! | Count | Increments and commits | the ordinary round trip, so timings are measurable |
//! | Bulk | Starts a host operation, never awaits it | a commit staying prompt under concurrent load |
//! | Garbage | Commits bytes that cannot decode | a rejection that leaves the interface standing |
//! | Nonsense | Commits a batch that decodes but means nothing | semantic validation, distinct from parsing |
//! | Giant | Commits a batch over the protocol's size limit | the wire format's hard bounds |
//! | Silent | Stops committing entirely | a guest that goes quiet without dying |
//! | Trap | Panics | the host-owned crash surface |
//! | Flood | Returns an error far over the crash cap | that the crash surface itself is bounded |
//! | Spin | Runs a non-yielding Wasm loop forever | that out-of-band shutdown can interrupt executing Wasm |
//! | Grow | Allocates past the Instar memory policy | that a policy violation is a contained guest failure |
//! | Sleep | Awaits a 30s host operation | that shutdown stays bounded for a guest suspended in a host op |
//! | Commit text views | Commits the interface with two text-view nodes and a crossed side table | attachment resolution is by identity, not slot position |
//! | Reverse text table | Re-commits the same tree with the side table reversed | a re-commit that changes the table but not the retained map is a no-op |
//! | Await edit | Suspends in `next-edit` and reports the exact fields it receives | a real host-local edit reaches a real suspended guest unchanged (C2c) |
//! | Apply edits | Calls `apply-edits` with a well-formed batch | a real guest's batch round-trips through the WIT/bridge mapping unchanged (C3c) |
//! | Apply malformed edit | Calls `apply-edits` with an inverted range | a real guest sees `invalid-edit`, not the storage layer's own taxonomy |
//! | Apply stale revision | Calls `apply-edits` with a revision that cannot be current | a real guest sees `conflict`, and nothing it sent lands |
//! | Bootstrap max legal | Calls `create-buffer` with a payload exactly at the size ceiling | a maximum legal transfer succeeds end to end (C4a) |
//! | Bootstrap oversized | Calls `create-buffer` with a payload over the ceiling but still liftable | Instar's own refusal, not Wasmtime's, catches a liftable-but-too-large payload |
//! | Bootstrap absurd | Calls `create-buffer` with a payload past the hostcall-fuel budget | Wasmtime stops an absurd payload before `TextHost` ever sees it |
//! | Acquire non-empty text | Bootstraps a non-empty buffer and its two views | real content and a real revision history, for C4d's convergence claim |
//! | Resynchronize | Calls `resynchronize` and reports the resulting length and revision | a real guest converges with the host after an overflow-forced desync (C4d) |
//!
//! # A trap's message is not the guest's
//!
//! Worth knowing, because it is the opposite of what you would guess: what the
//! host receives from a panicking guest is Wasmtime's *backtrace*, around 4 KB
//! of it. The guest's own panic text goes to the guest's stderr and never
//! crosses the boundary. A panicking guest therefore cannot flood the host
//! with a message of its choosing.
//!
//! The error a guest returns from `run` can, and is the only string in this
//! world whose length the guest controls outright — which is why `Flood`
//! returns rather than panics.

wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
    // The world now spans two packages: instar:kernel and the optional
    // instar:text capability. Without this, types from the second are an
    // error rather than generated bindings.
    generate_all,
});

use instar_ui_protocol::{
    BatchEncoder, NodeKey, WireAlign, WireEvent, WireLayout, WireSize, flags, limits, opcode,
};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::{CommitError, CommitResult, RuntimeError};
use crate::instar::kernel::kernel_ui;
use crate::instar::kernel::ops;
use crate::instar::text::text;
use crate::instar::text::text_types;

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

const ROOT: NodeKey = NodeKey::first(0);
const COLUMN: NodeKey = NodeKey::first(1);
const LABEL: NodeKey = NodeKey::first(2);
const COUNT: NodeKey = NodeKey::first(3);
const RESET: NodeKey = NodeKey::first(4);
const BULK: NodeKey = NodeKey::first(5);
const CRASH: NodeKey = NodeKey::first(6);
const GARBAGE: NodeKey = NodeKey::first(7);
const NONSENSE: NodeKey = NodeKey::first(8);
const GIANT: NodeKey = NodeKey::first(9);
const SILENT: NodeKey = NodeKey::first(10);
const FLOOD: NodeKey = NodeKey::first(11);
const FLOOD_COMMITS: NodeKey = NodeKey::first(12);
const SPIN: NodeKey = NodeKey::first(13);
const GROW: NodeKey = NodeKey::first(14);
const SLEEP: NodeKey = NodeKey::first(15);
/// Acquires a text buffer and two views of it, and keeps the handles.
const TEXT_ACQUIRE: NodeKey = NodeKey::first(16);
/// Drops one view handle, leaving the rest held.
const TEXT_RELEASE_VIEW: NodeKey = NodeKey::first(17);
/// Commits the interface with both text-view nodes and a crossed side table.
const TEXT_COMMIT: NodeKey = NodeKey::first(18);
/// Re-commits the same interface with the side table reversed.
const TEXT_REVERSE: NodeKey = NodeKey::first(19);
/// The label both text-commit buttons show.
///
/// Shared on purpose: the two commits must describe an identical interface so
/// that the only thing differing between them is the side table.
const TEXT_ATTACHED_LABEL: &str = "two text views attached";
/// Suspends in `next-edit` on the guest's first buffer and reports the exact
/// fields the host sends, for C2c's joined-seam claim.
const AWAIT_EDIT: NodeKey = NodeKey::first(20);
/// Calls `apply-edits` with a well-formed insertion at revision 0.
const APPLY_EDITS: NodeKey = NodeKey::first(21);
/// Calls `apply-edits` with an inverted range, at the correct revision.
const APPLY_EDITS_MALFORMED: NodeKey = NodeKey::first(22);
/// Calls `apply-edits` with a well-formed edit but a revision that cannot be
/// current.
const APPLY_EDITS_STALE: NodeKey = NodeKey::first(23);
/// Calls `create-buffer` with a payload exactly at the size ceiling.
const BOOTSTRAP_MAX_LEGAL: NodeKey = NodeKey::first(24);
/// Calls `create-buffer` with a payload over the ceiling but still liftable
/// within the hostcall-fuel budget.
const BOOTSTRAP_OVERSIZED_LIFTABLE: NodeKey = NodeKey::first(25);
/// Calls `create-buffer` with a payload past the hostcall-fuel budget.
const BOOTSTRAP_ABSURD: NodeKey = NodeKey::first(26);
/// Bootstraps a non-empty buffer and its two views, in place of the empty
/// one `TEXT_ACQUIRE` holds -- C4d needs real content and a real revision
/// history to converge, not an empty document.
const TEXT_ACQUIRE_NONEMPTY: NodeKey = NodeKey::first(27);
/// Calls `resynchronize` on the first held buffer and reports its length and
/// revision, for C4d's convergence claim.
const RESYNCHRONIZE: NodeKey = NodeKey::first(28);
/// The first text-view surface the commit buttons attach.
const TEXT_NODE_A: NodeKey = NodeKey::first(30);
/// The second text-view surface the commit buttons attach.
const TEXT_NODE_B: NodeKey = NodeKey::first(31);

/// How long the bulk operation runs. Long enough that it is unambiguously
/// still in flight while the gate measures a click round-trip against it.
const BULK_MILLIS: &str = "3000";

/// C4a: exactly at `instar-text`'s own size ceiling. Legal, and must succeed.
const BOOTSTRAP_MAX_LEGAL_BYTES: usize = 32 * 1024 * 1024;
/// Over the ceiling but comfortably under the hostcall-fuel budget: liftable,
/// but `instar-text` must still refuse it.
const BOOTSTRAP_OVERSIZED_LIFTABLE_BYTES: usize = 36 * 1024 * 1024;
/// Past the hostcall-fuel budget, and still comfortably under the guest's own
/// 64 MiB memory policy ceiling -- large enough that Wasmtime stops the lift
/// before any host text code runs, small enough that allocating it here
/// doesn't trip a different limit first.
const BOOTSTRAP_ABSURD_BYTES: usize = 56 * 1024 * 1024;

/// C4d's seed content: known, short, and distinct from anything a host-local
/// edit inserts, so a mismatch in the converged document is easy to place.
const NONEMPTY_SEED: &str = "seed";

/// Comfortably over the host's 32 KiB crash-surface cap, so the clamp is
/// exercised by a real trap rather than only by a unit test.
const FLOOD_LINES: usize = 4_000;

/// How many commits the flood button fires concurrently. Comfortably beyond
/// any queue bound, so the host's single-flight gate is what refuses them,
/// not channel capacity.
const CONCURRENT_COMMITS: usize = 512;

/// How much memory the Grow button asks for. Deliberately twice the Instar
/// default policy ceiling (64 MiB), so the request can only succeed if the
/// policy has drifted out from under the fixture.
const GROW_BYTES: usize = 128 * 1024 * 1024;

/// How long the Sleep button blocks inside a host operation. Far beyond any
/// shutdown grace, so a shutdown that waited on it would be visibly broken.
const SLEEP_MILLIS: &str = "30000";

fn container(padding: u16, gap: u16) -> WireLayout {
    WireLayout {
        // Span the parent's cross axis. This said `width: Fill` until the
        // wire stopped having a name that meant three things.
        align_self: Some(WireAlign::Stretch),
        padding,
        gap,
        ..WireLayout::default()
    }
}

/// A text view's on-screen size, stated explicitly rather than left to
/// content measurement.
///
/// `WireLayout::default()` measures by content, and an attached but empty
/// document has nothing yet for that measurement to hang a nonzero rect on --
/// a `TextView` node given the default layout lays out at `0x0`, which a real
/// pointer event can never hit. A fixed size is a wire-supplied fact any
/// guest may state, not a new host capability: it only exists so C2c's real
/// click has somewhere real to land.
fn text_view_layout() -> WireLayout {
    WireLayout {
        width: WireSize::Fixed(200),
        height: WireSize::Fixed(40),
        ..WireLayout::default()
    }
}

/// What the guest should do with its next commit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Commit the interface, as any guest would.
    Normal,
    /// Commit bytes that are not a batch at all.
    Garbage,
    /// Commit a batch that decodes and describes nonsense — two roots. The
    /// host must tell this apart from `Garbage`: one is a malformed message,
    /// the other is a well-formed message that means something impossible.
    Nonsense,
    /// Commit a batch past the protocol's size limit.
    Giant,
    /// Do not commit at all, ever again. Not an error and not a trap: a guest
    /// that simply stops speaking, which the host must not confuse with one
    /// that has died.
    Silent,
    /// Commit the two text-view surfaces with a crossed side table: node A
    /// names slot 1 (V7) and node B names slot 0 (V8).
    TextCommit,
    /// Re-commit the same tree with the side table reversed, keeping each
    /// node on the same view: node A names slot 0 (V7) and node B slot 1
    /// (V8).
    TextReverse,
}

struct Hostile {
    count: u32,
    bulk: u32,
    mode: Mode,
    /// Set to end `run` with this error on the next turn. Distinct from a
    /// trap: this string crosses the boundary intact, and its length is the
    /// guest's to choose.
    bail: Option<String>,
    /// Set to fire [`CONCURRENT_COMMITS`] commits at once, then commit a
    /// summary and terminate cleanly.
    flood: bool,
    /// Set to enter a non-yielding Wasm loop after the next commit.
    spin: bool,
    /// Set to attempt an allocation past the Instar memory policy.
    grow: bool,
    /// Set to suspend inside a 30s host operation.
    sleep: bool,
    /// Set to suspend inside `next-edit` on the first held buffer, after the
    /// next commit.
    await_edit: bool,
    /// Text capabilities this guest holds.
    ///
    /// Held, not used: B2e-1 proves acquisition and release, and there is no
    /// way to attach a view to the interface yet. What matters is that these
    /// handles exist, that dropping one tells the host, and that a generation
    /// ending releases whatever is left.
    buffers: Vec<text::TextBuffer>,
    views: Vec<text::TextView>,
    /// When set, replaces the label text for the next normal commit.
    label_override: Option<String>,
}

impl Hostile {
    /// The interface. Structure and text only — the protocol cannot express a
    /// rectangle, so even a hostile guest has no way to lie about geometry.
    fn encode(&self) -> Vec<u8> {
        self.encode_with_views(None)
    }

    /// The same interface with the two text-view surfaces appended.
    ///
    /// `crossed` chooses which slot each node names; the caller pairs that
    /// with the matching side-table order. Both commits describe the same
    /// tree, so the host's tree diff is empty and only the attachment
    /// resolution can tell them apart.
    fn encode_with_views(&self, crossed: Option<bool>) -> Vec<u8> {
        // Reset is meaningless at zero, and the host refuses to hit disabled
        // nodes -- a real behavioural difference, not decoration.
        let reset_flags = if self.count == 0 { 0 } else { flags::ENABLED };
        let extra_views = if crossed.is_some() { 2 } else { 0 };

        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, ROOT, 0, None, container(8, 0), 1)
            .node(
                opcode::NODE_COLUMN,
                COLUMN,
                0,
                None,
                container(0, 4),
                (27 + extra_views) as u16,
            )
            .node(
                opcode::NODE_TEXT,
                LABEL,
                0,
                Some(
                    self.label_override
                        .as_deref()
                        .unwrap_or(&format!("Clicked {} times, {} bulk", self.count, self.bulk)),
                ),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                COUNT,
                flags::ENABLED,
                Some("Press me"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                RESET,
                reset_flags,
                Some("Reset"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                BULK,
                flags::ENABLED,
                Some("Start bulk work"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                GARBAGE,
                flags::ENABLED,
                Some("Commit garbage"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                NONSENSE,
                flags::ENABLED,
                Some("Commit nonsense"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                GIANT,
                flags::ENABLED,
                Some("Commit too much"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                SILENT,
                flags::ENABLED,
                Some("Go silent"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                CRASH,
                flags::ENABLED,
                Some("Crash"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                FLOOD,
                flags::ENABLED,
                Some("Fail loudly"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                FLOOD_COMMITS,
                flags::ENABLED,
                Some("Flood 512 commits"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                SPIN,
                flags::ENABLED,
                Some("Spin forever"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                GROW,
                flags::ENABLED,
                Some("Grow past policy"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                TEXT_ACQUIRE,
                flags::ENABLED,
                Some("Acquire text"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                TEXT_RELEASE_VIEW,
                flags::ENABLED,
                Some("Release one view"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                TEXT_COMMIT,
                flags::ENABLED,
                Some("Commit text views"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                TEXT_REVERSE,
                flags::ENABLED,
                Some("Reverse text table"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                SLEEP,
                flags::ENABLED,
                Some("Sleep on host op"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                AWAIT_EDIT,
                flags::ENABLED,
                Some("Await edit"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                APPLY_EDITS,
                flags::ENABLED,
                Some("Apply edits"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                APPLY_EDITS_MALFORMED,
                flags::ENABLED,
                Some("Apply malformed edit"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                APPLY_EDITS_STALE,
                flags::ENABLED,
                Some("Apply stale revision"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                BOOTSTRAP_MAX_LEGAL,
                flags::ENABLED,
                Some("Bootstrap max legal"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                BOOTSTRAP_OVERSIZED_LIFTABLE,
                flags::ENABLED,
                Some("Bootstrap oversized"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                BOOTSTRAP_ABSURD,
                flags::ENABLED,
                Some("Bootstrap absurd"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                TEXT_ACQUIRE_NONEMPTY,
                flags::ENABLED,
                Some("Acquire non-empty text"),
                WireLayout::default(),
                0,
            )
            .node(
                opcode::NODE_BUTTON,
                RESYNCHRONIZE,
                flags::ENABLED,
                Some("Resynchronize"),
                WireLayout::default(),
                0,
            );
        if let Some(crossed) = crossed {
            let (first_slot, second_slot) = if crossed { (1, 0) } else { (0, 1) };
            encoder
                .text_view(TEXT_NODE_A, flags::ENABLED, first_slot, text_view_layout())
                .text_view(TEXT_NODE_B, flags::ENABLED, second_slot, text_view_layout());
        }
        encoder.finish()
    }

    /// A batch that parses cleanly and describes an impossible tree: a second
    /// root nested inside the first. The wire format permits this — it reports
    /// what the bytes say — and `instar-ui` is what refuses it.
    fn nonsense(&self) -> Vec<u8> {
        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, ROOT, 0, None, container(0, 0), 1)
            .node(opcode::NODE_ROOT, COLUMN, 0, None, container(0, 0), 0);
        encoder.finish()
    }

    /// A batch deliberately past [`limits::MAX_BATCH_BYTES`].
    ///
    /// Built by hand rather than through the encoder's node API, because the
    /// point is to hand the host more bytes than it will accept and see it
    /// refuse them at the wire layer — before any of it is interpreted.
    fn giant(&self) -> Vec<u8> {
        let mut batch = self.encode();
        batch.resize(limits::MAX_BATCH_BYTES + 1_024, 0);
        batch
    }

    /// Suspends until the main thread has applied this interface.
    ///
    /// A rejection is *not* an error here. The host refusing a batch is a
    /// normal outcome the guest is told about, and a guest that fell over on
    /// hearing "no" could not be used to test what happens after a rejection —
    /// which is most of what this guest is for.
    async fn commit(&mut self) -> Result<(), String> {
        // Two views are needed to say anything interesting about identity
        // versus position, so a text mode with fewer falls back to the
        // ordinary interface rather than committing slots that name nothing.
        let mode = match self.mode {
            Mode::TextCommit | Mode::TextReverse if self.views.len() < 2 => Mode::Normal,
            mode => mode,
        };

        let batch = match mode {
            Mode::Normal => self.encode(),
            Mode::Garbage => b"\xff\xff\xff this is not a batch".to_vec(),
            Mode::Nonsense => self.nonsense(),
            Mode::Giant => self.giant(),
            // Nothing is committed and nothing is awaited. The guest returns
            // to `next-event` and stays responsive; it simply never describes
            // an interface again.
            Mode::Silent => return Ok(()),
            Mode::TextCommit => self.encode_with_views(Some(true)),
            Mode::TextReverse => self.encode_with_views(Some(false)),
        };

        // Bad commits are one-shot: the guest misbehaves once and goes back to
        // normal. That makes the more interesting property testable — not just
        // "does the host survive a bad batch" but "does everything still work
        // afterwards", which is where a host that half-applied something would
        // finally show it. `Silent` is deliberately not reset; a guest that
        // has stopped speaking has stopped speaking.
        //
        // `TextCommit`/`TextReverse` are the one other exception, and sticky
        // rather than one-shot: an attached TextView is not a one-off
        // misbehaviour, it is what the interface *is* from here on, the same
        // way a real editor's TextView stays in its tree across unrelated UI
        // changes. Without this, the very next ordinary commit -- Await edit,
        // say -- would re-derive the tree from `encode()` with no text views
        // in it at all, silently detaching the node C2c depends on staying
        // attached.
        //
        // Assigned before the side table is borrowed below, because that
        // borrow lives across the await.
        self.mode = match mode {
            Mode::TextCommit | Mode::TextReverse => mode,
            _ => Mode::Normal,
        };

        // The side table, paired with the slots the batch just encoded so that
        // both text commits resolve to the *same* retained map:
        //
        //   TextCommit   node A -> slot 1, node B -> slot 0, table [V8, V7]
        //   TextReverse  node A -> slot 0, node B -> slot 1, table [V7, V8]
        //
        // Node A names V7 and node B names V8 either way. Every slot number
        // and every table position moved between the two commits and nothing
        // the host retains did, which is the whole claim.
        let views: Vec<&text::TextView> = match mode {
            Mode::TextCommit => self.views.iter().rev().collect(),
            Mode::TextReverse => self.views.iter().collect(),
            _ => Vec::new(),
        };

        match kernel_ui::commit(batch, views).await {
            Ok(_) => Ok(()),
            // A rejection is *not* an error here. The host refusing a batch is
            // a normal outcome the guest is told about, and a guest that fell
            // over on hearing "no" could not be used to test what happens
            // after a rejection — which is most of what this guest is for.
            Err(_) if matches!(mode, Mode::Garbage | Mode::Nonsense | Mode::Giant) => Ok(()),
            // A rejected *well-formed* commit is a different matter: the guest
            // asked for something reasonable and did not get it. The text
            // commits are well-formed by construction, so a refusal there is
            // the joined seam failing and must not be swallowed.
            Err(error) => Err(format!("a normal commit was refused: {error:?}")),
        }
    }

    /// Fires [`CONCURRENT_COMMITS`] commits concurrently and waits for every
    /// verdict. The join is hand-rolled because a guest fixture has no
    /// executor crate to depend on: the Component Model task polls each import
    /// future until its waitable is ready, which is exactly what the kernel's
    /// single-flight gate is being asked to survive.
    async fn flood_commits(&self) -> String {
        let mut pending: Vec<Option<PendingCommit>> = Vec::with_capacity(CONCURRENT_COMMITS);
        for _ in 0..CONCURRENT_COMMITS {
            pending.push(Some(Box::pin(kernel_ui::commit(self.encode(), Vec::new()))));
        }
        let outcome = JoinCommits {
            pending,
            applied: 0,
            in_progress: 0,
            other: 0,
        }
        .await;
        format!(
            "Flooded {} commits: applied {}, in-progress {}, other {}",
            CONCURRENT_COMMITS, outcome.applied, outcome.in_progress, outcome.other
        )
    }

    /// Allocates [`GROW_BYTES`] inside one linear memory.
    ///
    /// The Instar policy's `memory_bytes` ceiling is below this, so the
    /// allocation must fail as a guest trap. If the allocation succeeds, the
    /// guest commits that fact so a drifted policy fails the gate loudly
    /// instead of silently passing.
    async fn grow_memory(&mut self) -> Result<(), String> {
        let bytes = vec![0u8; GROW_BYTES];
        self.label_override = Some(format!("Grew {} bytes (policy drift!)", bytes.len()));
        self.commit().await
    }

    /// Suspends inside a host operation for [`SLEEP_MILLIS`].
    async fn sleep_on_host_op(&self) -> Result<(), String> {
        let id = ops::start("delay", SLEEP_MILLIS.as_bytes());
        match ops::await_op(id).await {
            Ok(_bytes) => Ok(()),
            Err(error) => Err(format!("sleep op failed: {error:?}")),
        }
    }

    /// Never returns and never calls a host import: a real non-yielding Wasm
    /// loop. Only the engine epoch mechanism can stop it.
    fn spin_forever(&self) -> ! {
        let mut counter: u64 = 0;
        loop {
            counter = counter.wrapping_add(1);
            core::hint::black_box(counter);
        }
    }

    fn handle(&mut self, event: WireEvent) {
        match event {
            WireEvent::Click { node } if node == COUNT => self.count += 1,
            WireEvent::Click { node } if node == RESET => self.count = 0,
            WireEvent::Click { node } if node == BULK => {
                // Started and deliberately *not* awaited. The point is that
                // real host work is in flight on the runtime thread while the
                // guest carries on committing, so a UI round-trip measured
                // against it is measured under genuine concurrent load.
                //
                // The operation therefore stays in the host's registry until
                // the generation is destroyed, which cancels it. That is
                // acceptable for a fixture and is exactly the shape the
                // teardown tests want to see.
                ops::start("delay", BULK_MILLIS.as_bytes());
                self.bulk += 1;
            }
            WireEvent::Click { node } if node == GARBAGE => self.mode = Mode::Garbage,
            WireEvent::Click { node } if node == NONSENSE => self.mode = Mode::Nonsense,
            WireEvent::Click { node } if node == GIANT => self.mode = Mode::Giant,
            WireEvent::Click { node } if node == SILENT => self.mode = Mode::Silent,
            WireEvent::Click { node } if node == CRASH => {
                panic!("guest trapping on purpose (the Crash button)");
            }
            WireEvent::Click { node } if node == FLOOD => {
                // Deliberately *not* a panic. A trap's message, as the host
                // receives it, is Wasmtime's backtrace — the guest's own panic
                // text goes to the guest's stderr and never crosses the
                // boundary, so a panicking guest cannot actually flood the
                // host with a message of its choosing.
                //
                // The error a guest returns from `run` can, and is the only
                // string in this world whose length the guest controls
                // outright. That is the unbounded input the crash surface has
                // to survive.
                self.bail = Some(format!(
                    "the guest failed with a very large message:\n{}",
                    (0..FLOOD_LINES)
                        .map(|n| format!("hostile frame {n} of {FLOOD_LINES}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            WireEvent::Click { node } if node == FLOOD_COMMITS => {
                self.flood = true;
            }
            WireEvent::Click { node } if node == SPIN => {
                self.label_override = Some("Spinning...".to_string());
                self.spin = true;
            }
            WireEvent::Click { node } if node == GROW => {
                self.label_override = Some("Growing...".to_string());
                self.grow = true;
            }
            WireEvent::Click { node } if node == SLEEP => {
                self.label_override = Some("Sleeping...".to_string());
                self.sleep = true;
            }
            // Text capabilities, reached exactly like any other interaction:
            // a click the host hit-tested and delivered. Nothing about these
            // is special to the wire format or to the host.
            WireEvent::Click { node } if node == TEXT_ACQUIRE => {
                match text::create_empty_buffer() {
                    Ok(buffer) => {
                        for _ in 0..2 {
                            match text::create_view(&buffer) {
                                Ok(view) => self.views.push(view),
                                Err(error) => {
                                    self.label_override = Some(format!("view failed: {error:?}"));
                                }
                            }
                        }
                        self.label_override = Some(format!("held {} view(s)", self.views.len()));
                        self.buffers.push(buffer);
                    }
                    Err(error) => {
                        self.label_override = Some(format!("buffer failed: {error:?}"));
                    }
                }
            }
            // The two commits that carry capabilities. Same interface, same
            // views, different slot numbers and a different table order --
            // so the host's retained map must come out identical.
            WireEvent::Click { node } if node == TEXT_COMMIT => {
                if self.views.len() < 2 {
                    self.label_override =
                        Some("need two views: press Acquire text first".to_string());
                } else {
                    self.label_override = Some(TEXT_ATTACHED_LABEL.to_string());
                    self.mode = Mode::TextCommit;
                }
            }
            WireEvent::Click { node } if node == TEXT_REVERSE => {
                if self.views.len() < 2 {
                    self.label_override =
                        Some("need two views: press Acquire text first".to_string());
                } else {
                    // Deliberately the *same* label as the commit above. The
                    // two commits have to describe the same interface for the
                    // permutation to mean anything: if the label differed, the
                    // tree diff would be non-empty and "only the table moved"
                    // would be untestable.
                    self.label_override = Some(TEXT_ATTACHED_LABEL.to_string());
                    self.mode = Mode::TextReverse;
                }
            }
            WireEvent::Click { node } if node == TEXT_RELEASE_VIEW => {
                // Dropping the handle is the release: the generated resource
                // destructor is what tells the host.
                self.views.pop();
                self.label_override = Some(format!("held {} view(s)", self.views.len()));
            }
            WireEvent::Click { node } if node == AWAIT_EDIT => {
                self.label_override = Some("awaiting edit...".to_string());
                self.await_edit = true;
            }
            // `apply-edits` is a plain `func`, not `async` -- the WIT freeze
            // for C3 is explicit that only `next-edit` suspends the guest
            // task, so these three report their outcome synchronously inside
            // `handle`, the same way `TEXT_ACQUIRE` does for `create-view`.
            WireEvent::Click { node } if node == APPLY_EDITS => {
                let edits = vec![text_types::TextEdit {
                    range_start: 0,
                    range_end: 0,
                    replacement: "hi".to_string(),
                }];
                self.label_override = Some(self.apply_edits_report(0, edits));
            }
            WireEvent::Click { node } if node == APPLY_EDITS_MALFORMED => {
                // Inverted: start after end. `instar-text`'s storage layer
                // would call this `InvertedRange`; the guest must see only
                // the coarse `invalid-edit`.
                let edits = vec![text_types::TextEdit {
                    range_start: 9,
                    range_end: 3,
                    replacement: "nope".to_string(),
                }];
                self.label_override = Some(self.apply_edits_report(0, edits));
            }
            WireEvent::Click { node } if node == APPLY_EDITS_STALE => {
                // The edit itself is well-formed; only the revision is wrong,
                // so a `conflict` here can only be attributed to the
                // revision check, not to the batch.
                let edits = vec![text_types::TextEdit {
                    range_start: 0,
                    range_end: 0,
                    replacement: "should not land".to_string(),
                }];
                self.label_override = Some(self.apply_edits_report(42, edits));
            }
            // C4a: `create-buffer` at three size tiers relative to the two
            // budgets that gate it -- `instar-text`'s own ceiling, and
            // Wasmtime's hostcall-fuel budget above that.
            WireEvent::Click { node } if node == BOOTSTRAP_MAX_LEGAL => {
                self.label_override = Some(self.bootstrap_report(BOOTSTRAP_MAX_LEGAL_BYTES));
            }
            WireEvent::Click { node } if node == BOOTSTRAP_OVERSIZED_LIFTABLE => {
                self.label_override =
                    Some(self.bootstrap_report(BOOTSTRAP_OVERSIZED_LIFTABLE_BYTES));
            }
            WireEvent::Click { node } if node == BOOTSTRAP_ABSURD => {
                // Deliberately no result handling: a payload this size is
                // expected to exceed Wasmtime's hostcall-fuel budget and trap
                // this generation while lifting the argument, before
                // `create-buffer`'s host implementation ever runs. There is
                // no outcome to report because the call itself is not
                // expected to return one -- the label below is a canary: if
                // it is ever observed, the trap did not happen and the fuel
                // assumption this button exists to prove needs revisiting.
                let contents = "x".repeat(BOOTSTRAP_ABSURD_BYTES);
                let _ = text::create_buffer(&contents);
                self.label_override = Some("bootstrap: absurd call returned".to_string());
            }
            // C4d: a non-empty buffer with real content and two views, in
            // place of `TEXT_ACQUIRE`'s empty one -- reused for everything
            // downstream (`TEXT_COMMIT`'s attachment, real host-local edits,
            // `RESYNCHRONIZE`) exactly as `TEXT_ACQUIRE`'s empty buffer
            // already is, so convergence is proven through the same real
            // machinery every other joined-seam test uses rather than a
            // parallel path built just for this claim.
            WireEvent::Click { node } if node == TEXT_ACQUIRE_NONEMPTY => {
                match text::create_buffer(NONEMPTY_SEED) {
                    Ok(buffer) => {
                        for _ in 0..2 {
                            match text::create_view(&buffer) {
                                Ok(view) => self.views.push(view),
                                Err(error) => {
                                    self.label_override = Some(format!("view failed: {error:?}"));
                                }
                            }
                        }
                        self.label_override = Some(format!("held {} view(s)", self.views.len()));
                        self.buffers.push(buffer);
                    }
                    Err(error) => {
                        self.label_override = Some(format!("buffer failed: {error:?}"));
                    }
                }
            }
            WireEvent::Click { node } if node == RESYNCHRONIZE => {
                self.label_override = Some(self.resynchronize_report());
            }
            // Not an error: the host may address nodes this version does not
            // act on.
            WireEvent::Click { .. } => {}
        }
    }
}

impl Hostile {
    /// Suspends in `next-edit` on the first held buffer and formats exactly
    /// what came back, for the test to read off the label.
    ///
    /// Folds every outcome -- edit, desync, or refusal -- into the label
    /// rather than treating any of them as a guest failure. That is the same
    /// discipline the other one-shot modes already use (`TEXT_ACQUIRE`'s
    /// `buffer failed: {error:?}` is the precedent): a real host-local edit
    /// arriving with the wrong fields shows up as a label mismatch the test
    /// reports clearly, rather than a guest that fell over on hearing
    /// anything other than exactly what it expected.
    async fn await_one_edit(&self) -> String {
        let Some(buffer) = self.buffers.first() else {
            return "no buffer to await".to_string();
        };
        match text::next_edit(buffer).await {
            Ok(text_types::EditNotification::Edits(edits)) => match edits.first() {
                Some(edit) => format!(
                    "edit: base={} result={} start={} end={} repl={}",
                    edit.base_revision,
                    edit.resulting_revision,
                    edit.edit.range_start,
                    edit.edit.range_end,
                    edit.edit.replacement,
                ),
                None => "edits: empty".to_string(),
            },
            Ok(text_types::EditNotification::Desynchronized(revision)) => {
                format!("desynchronized: {revision}")
            }
            Err(error) => format!("next-edit failed: {error:?}"),
        }
    }

    /// Calls `apply-edits` on the first held buffer and formats exactly what
    /// came back, the same discipline `await_one_edit` uses: every outcome —
    /// applied, conflict, or refused — is folded into the label rather than
    /// treated as a guest failure, so a wrong field shows up as a label
    /// mismatch the test reports clearly.
    fn apply_edits_report(&self, expected_revision: u64, edits: Vec<text_types::TextEdit>) -> String {
        let Some(buffer) = self.buffers.first() else {
            return "no buffer to apply against".to_string();
        };
        match text::apply_edits(buffer, expected_revision, &edits) {
            Ok(text_types::ApplyEditsResult::Applied(revision)) => {
                format!("apply: applied({revision})")
            }
            Ok(text_types::ApplyEditsResult::Conflict(revision)) => {
                format!("apply: conflict({revision})")
            }
            Err(error) => format!("apply: refused {error:?}"),
        }
    }

    /// Calls `create-buffer` with `size` bytes of content and formats the
    /// outcome. On success the handle is retained, the same way `TEXT_ACQUIRE`
    /// keeps what it creates, so the host-side buffer stays live for the test
    /// to inspect.
    fn bootstrap_report(&mut self, size: usize) -> String {
        let contents = "x".repeat(size);
        match text::create_buffer(&contents) {
            Ok(buffer) => {
                self.buffers.push(buffer);
                format!("bootstrap: ok {size} bytes")
            }
            Err(error) => format!("bootstrap: refused {error:?}"),
        }
    }

    /// Calls `resynchronize` on the first held buffer and reports its length
    /// and revision -- not the full contents, which can be arbitrarily large
    /// after a desync-triggering edit and would make a poor UI label. Length
    /// and revision are exactly what C4d's test needs to compare against the
    /// host's own buffer state for its convergence claim.
    fn resynchronize_report(&self) -> String {
        let Some(buffer) = self.buffers.first() else {
            return "no buffer to resynchronize".to_string();
        };
        match text::resynchronize(buffer) {
            Ok(snapshot) => format!(
                "resync: len={} revision={}",
                snapshot.contents.len(),
                snapshot.revision
            ),
            Err(error) => format!("resync: refused {error:?}"),
        }
    }
}

/// Outcome of the concurrent commit flood.
struct FloodOutcome {
    applied: u32,
    in_progress: u32,
    other: u32,
}

type PendingCommit = Pin<Box<dyn Future<Output = Result<CommitResult, CommitError>>>>;

/// Polls a set of commit futures together, collecting the verdict each one
/// produces. Completed futures are taken out so they are never polled twice.
struct JoinCommits {
    pending: Vec<Option<PendingCommit>>,
    applied: u32,
    in_progress: u32,
    other: u32,
}

impl Future for JoinCommits {
    type Output = FloodOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut remaining = 0usize;
        for slot in &mut this.pending {
            let Some(future) = slot.as_mut() else {
                continue;
            };
            match future.as_mut().poll(cx) {
                Poll::Ready(result) => {
                    match result {
                        Ok(_) => this.applied += 1,
                        Err(CommitError::CommitInProgress) => this.in_progress += 1,
                        Err(_) => this.other += 1,
                    }
                    *slot = None;
                }
                Poll::Pending => remaining += 1,
            }
        }
        if remaining == 0 {
            Poll::Ready(FloodOutcome {
                applied: this.applied,
                in_progress: this.in_progress,
                other: this.other,
            })
        } else {
            Poll::Pending
        }
    }
}

struct Component;

impl Guest for Component {
    async fn run() -> Result<(), String> {
        let mut hostile = Hostile {
            count: 0,
            bulk: 0,
            mode: Mode::Normal,
            bail: None,
            flood: false,
            spin: false,
            grow: false,
            sleep: false,
            await_edit: false,
            buffers: Vec::new(),
            views: Vec::new(),
            label_override: None,
        };
        hostile.commit().await?;

        loop {
            match kernel_runtime::next_event().await {
                Ok(payload) => {
                    let event = WireEvent::decode(&payload)
                        .map_err(|error| format!("undecodable host event: {error}"))?;
                    hostile.handle(event);
                    if let Some(error) = hostile.bail.take() {
                        return Err(error);
                    }
                    if std::mem::take(&mut hostile.flood) {
                        let summary = hostile.flood_commits().await;
                        hostile.label_override = Some(summary);
                        hostile.commit().await?;
                        return Ok(());
                    }
                    hostile.commit().await?;
                    // Each hostile mode is one-shot, and each engages after
                    // its state-changing commit so the host can observe it.
                    if std::mem::take(&mut hostile.spin) {
                        hostile.spin_forever();
                    }
                    if std::mem::take(&mut hostile.grow) {
                        hostile.grow_memory().await?;
                    }
                    if std::mem::take(&mut hostile.sleep) {
                        hostile.sleep_on_host_op().await?;
                    }
                    if std::mem::take(&mut hostile.await_edit) {
                        let outcome = hostile.await_one_edit().await;
                        hostile.label_override = Some(outcome);
                        hostile.commit().await?;
                    }
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);
