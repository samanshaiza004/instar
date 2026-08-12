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
    BatchEncoder, NodeKey, WireAlign, WireEvent, WireLayout, flags, limits, opcode,
};

use crate::instar::kernel::kernel_runtime;
use crate::instar::kernel::kernel_types::{CommitError, CommitResult, RuntimeError};
use crate::instar::kernel::kernel_ui;
use crate::instar::kernel::ops;
use crate::instar::text::text;

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

/// How long the bulk operation runs. Long enough that it is unambiguously
/// still in flight while the gate measures a click round-trip against it.
const BULK_MILLIS: &str = "3000";

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
        // Reset is meaningless at zero, and the host refuses to hit disabled
        // nodes -- a real behavioural difference, not decoration.
        let reset_flags = if self.count == 0 { 0 } else { flags::ENABLED };

        let mut encoder = BatchEncoder::new();
        encoder
            .node(opcode::NODE_ROOT, ROOT, 0, None, container(8, 0), 1)
            .node(opcode::NODE_COLUMN, COLUMN, 0, None, container(0, 4), 16)
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
                SLEEP,
                flags::ENABLED,
                Some("Sleep on host op"),
                WireLayout::default(),
                0,
            );
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
        let batch = match self.mode {
            Mode::Normal => self.encode(),
            Mode::Garbage => b"\xff\xff\xff this is not a batch".to_vec(),
            Mode::Nonsense => self.nonsense(),
            Mode::Giant => self.giant(),
            // Nothing is committed and nothing is awaited. The guest returns
            // to `next-event` and stays responsive; it simply never describes
            // an interface again.
            Mode::Silent => return Ok(()),
        };
        let mode = self.mode;

        // Bad commits are one-shot: the guest misbehaves once and goes back to
        // normal. That makes the more interesting property testable — not just
        // "does the host survive a bad batch" but "does everything still work
        // afterwards", which is where a host that half-applied something would
        // finally show it. `Silent` is deliberately not reset; a guest that
        // has stopped speaking has stopped speaking.
        self.mode = Mode::Normal;

        match kernel_ui::commit(batch).await {
            Ok(_) => Ok(()),
            // A rejection is *not* an error here. The host refusing a batch is
            // a normal outcome the guest is told about, and a guest that fell
            // over on hearing "no" could not be used to test what happens
            // after a rejection — which is most of what this guest is for.
            Err(_) if mode != Mode::Normal => Ok(()),
            // A rejected *well-formed* commit is a different matter: the guest
            // asked for something reasonable and did not get it.
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
            pending.push(Some(Box::pin(kernel_ui::commit(self.encode()))));
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
                        self.label_override =
                            Some(format!("held {} view(s)", self.views.len()));
                        self.buffers.push(buffer);
                    }
                    Err(error) => {
                        self.label_override = Some(format!("buffer failed: {error:?}"));
                    }
                }
            }
            WireEvent::Click { node } if node == TEXT_RELEASE_VIEW => {
                // Dropping the handle is the release: the generated resource
                // destructor is what tells the host.
                self.views.pop();
                self.label_override = Some(format!("held {} view(s)", self.views.len()));
            }
            // Not an error: the host may address nodes this version does not
            // act on.
            WireEvent::Click { .. } => {}
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
                }
                Err(RuntimeError::Shutdown) => return Ok(()),
                Err(RuntimeError::Internal(message)) => return Err(message),
            }
        }
    }
}

export!(Component);
