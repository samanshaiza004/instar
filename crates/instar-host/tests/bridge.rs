//! WP7B1 acceptance gate: the runtime/main-thread bridge, plus WP8's breadth.
//!
//! Every test here runs a real `wasm32-wasip2` guest in a real
//! `instar-kernel` generation on a real second thread. Nothing between the
//! click and the committed tree is simulated — the only thing missing is
//! winit, and only because an event loop needs a display server. Its role in
//! the design is a queue plus a wake, and both of those are exercised
//! directly.
//!
//! The ten properties `docs/PHASE-1.md` requires, and where each is:
//!
//! | # | Property | Test |
//! |---|---|---|
//! | 1 | click -> event -> async commit -> accepted -> re-suspend | [`a_click_round_trips_and_the_guest_re_suspends`] |
//! | 2 | an invalid commit is rejected without mutating the tree | [`an_invalid_commit_changes_nothing`] |
//! | 3 | 100 rapid activations preserve order | [`a_hundred_rapid_activations_arrive_in_order`] |
//! | 4 | a full main->runtime queue never blocks winit | [`a_full_command_queue_never_blocks_the_winit_thread`] |
//! | 5 | runtime->main wake works while the loop is in `Wait` | [`a_runtime_wake_reaches_a_parked_main_thread`] |
//! | 6 | a pending bulk operation does not delay a UI commit | [`bulk_work_in_flight_does_not_delay_a_ui_commit`] |
//! | 7 | a trap while a commit awaits produces no later mutation | [`a_trap_stops_an_in_flight_commit_from_mutating_anything`] |
//! | 8 | teardown while a commit awaits resolves cleanly | [`teardown_with_a_commit_in_flight_resolves_promptly`] |
//! | 9 | an old-generation commit is rejected before decoding | [`an_old_generation_commit_is_rejected_before_decoding`] |
//! | 10 | 1,000 cycles leave queues and operation counts at baseline | [`a_thousand_click_cycles_return_to_baseline`] |
//!
//! WP8 adds the rest of what a guest is permitted to do wrong — garbage,
//! well-formed nonsense, oversized batches, going silent, and failing with
//! more text than the crash surface will hold. Those live at the bottom of the
//! file, and each asserts the same thing after the refusal: that everything
//! still works.
//!
//! # Promptness, not eventual completion
//!
//! Wasmtime warns that a future inside `run_concurrent` can go unpolled for an
//! extended period even after its waker fires. "Eventually completes" is
//! therefore not the property that matters: a round-trip resolving in three
//! seconds would pass an eventual test and is a broken UI. Every wait here has
//! a measured bound, and [`PROMPT`] is the one that matters — the click-to-
//! committed-tree round-trip, asserted under concurrent load.
//!
//! [`PROMPT`] is a ceiling on brokenness and is asserted — in release builds
//! only, for the reason [`assert_prompt`] gives. The *distribution* —
//! p50/p95/p99, collected by [`Latencies`] — is reported and not asserted, and
//! will stay that way until there are numbers from a real windowed host to
//! calibrate against. See its docs for why.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use instar_host::bridge::{HostBridge, HostUserEvent, QUEUE_CAPACITY, TerminalOutcome, Wake};
use instar_host::{HostEffect, HostWindow, PresentationState};
use instar_kernel::bridge::{CommitRejection, commit_request};
use instar_kernel::runtime::{EVENT_QUEUE_CAPACITY, GenerationId};
use instar_ui::protocol::{BatchEncoder, WireAlign, WireLayout, flags, opcode};
use instar_ui::{NodeKey, NodeKind};
use instar_window::{
    LogicalPoint, LogicalSize, PhysicalSize, PointerButton, PointerState, RawPointerEvent,
    WindowId, WindowMetricsChanged, WindowOutput,
};

const WINDOW: WindowId = WindowId::from_raw(1);

/// The node keys the fixture uses. Duplicated here rather than shared, because
/// a host learns them from the wire and nothing else.
const LABEL: NodeKey = NodeKey::first(2);
const COUNT: NodeKey = NodeKey::first(3);
const BULK: NodeKey = NodeKey::first(5);
const CRASH: NodeKey = NodeKey::first(6);
const GARBAGE: NodeKey = NodeKey::first(7);
const NONSENSE: NodeKey = NodeKey::first(8);
const GIANT: NodeKey = NodeKey::first(9);
const SILENT: NodeKey = NodeKey::first(10);
const FLOOD: NodeKey = NodeKey::first(11);
const FLOOD_COMMITS: NodeKey = NodeKey::first(12);

/// The bound a click-to-committed-tree round-trip must stay inside.
///
/// Chosen as a *ceiling on brokenness*, not as a performance target: a UI that
/// takes this long to answer a click is already bad, so a failure here is
/// unambiguous rather than flaky. Measured values on a developer machine are
/// two orders of magnitude below it.
const PROMPT: Duration = Duration::from_millis(250);

/// The outer bound for anything that should not need a second at all.
const PATIENCE: Duration = Duration::from_secs(5);

/// Round-trip latencies, reported but not asserted on.
///
/// Deliberately separate from [`PROMPT`]. A ceiling on brokenness is a
/// correctness property and belongs in CI; a distribution is a *measurement*,
/// and tightening CI against numbers taken on an unloaded developer machine
/// with no windowing system attached would be inventing a target before the
/// baseline exists — the same mistake `docs/PHASE-1.md`'s measurement policy
/// rules out for memory and startup. These print; they do not fail.
#[derive(Debug)]
struct Latencies(Vec<Duration>);

impl Latencies {
    fn with_capacity(n: usize) -> Self {
        Self(Vec::with_capacity(n))
    }

    fn record(&mut self, elapsed: Duration) {
        self.0.push(elapsed);
    }

    /// Nearest-rank: the smallest sample at or above the percentile. No
    /// interpolation, so every number reported is one that actually happened.
    fn percentile(&self, sorted: &[Duration], p: f64) -> Duration {
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    }

    /// Asserts the warm-click targets. Release only; debug asserts nothing
    /// about time. `PROMPT` bounds the max as an outlier and deadlock guard,
    /// not a target — it sits far above p50 on purpose, and it is the max
    /// rather than a percentile because a p99 bound would let ten clicks in a
    /// thousand take arbitrarily long, and a UI that ignores every hundredth
    /// click is broken in exactly the way the tail is where you would look.
    ///
    /// One body with `cfg!`, not a `#[cfg]`-gated pair — see [`assert_prompt`].
    ///
    /// Release *and* opt-in: see [`latency_gate_armed`]. The distribution is
    /// printed either way.
    fn assert_targets(&self) {
        if cfg!(debug_assertions) || !latency_gate_armed() {
            return;
        }
        let mut sorted = self.0.clone();
        sorted.sort_unstable();
        for (name, measured, target) in [
            ("p50", self.percentile(&sorted, 50.0), P50),
            ("p95", self.percentile(&sorted, 95.0), P95),
            ("p99", self.percentile(&sorted, 99.0), P99),
            ("max", self.slowest(), PROMPT),
        ] {
            assert!(
                measured <= target,
                "{name} was {measured:?}, over the {target:?} target -- \
                 progress that degrades over a session is not prompt progress"
            );
        }
    }

    fn slowest(&self) -> Duration {
        self.0.iter().copied().max().unwrap_or_default()
    }

    /// Prints the distribution. `cargo test -- --nocapture` shows it; a green
    /// run stays quiet.
    fn report(&self, label: &str) {
        let mut sorted = self.0.clone();
        sorted.sort_unstable();
        println!(
            "{label}: n={} p50={:?} p95={:?} p99={:?} max={:?}",
            sorted.len(),
            self.percentile(&sorted, 50.0),
            self.percentile(&sorted, 95.0),
            self.percentile(&sorted, 99.0),
            self.slowest(),
        );
        // Said out loud, every time, so a gate that is off cannot drift into
        // being a gate nobody remembers exists.
        if cfg!(debug_assertions) {
            println!("  (debug build: timings are not asserted, and not comparable)");
        } else if !latency_gate_armed() {
            println!("  (reported only; set INSTAR_LATENCY_GATE=1 on an idle host to assert)");
        }
    }
}

/// The latency gate — **release only**.
///
/// Debug timings measure rustc's optimization level more than Instar's design.
/// A single debug reading of 386ms once sent an investigation hunting an
/// architectural defect that release measured at 5ms; the real bug was there,
/// but the number that raised the alarm was 75x the number that mattered.
///
/// So the two builds assert different things, permanently:
///
/// ```text
/// debug    completion, ordering, cancellation, no-hang, boundedness
/// release  latency: p50 <= 5ms, p95 <= 8ms, p99 <= 16ms, max <= 250ms
/// ```
///
/// The split is `cfg!` rather than `#[cfg]` on purpose: the assertion must
/// **compile** in every build even when it does not run. A `#[cfg]`-gated pair
/// of bodies is only type-checked in the build it belongs to, and because CI
/// builds debug, this function's release body sat here calling *itself* —
/// asserting nothing, and overflowing the stack on the first release run —
/// with nothing able to notice. A gate that hides a body from the compiler
/// hides its bugs too.
fn assert_prompt(elapsed: Duration, what: &str) {
    if cfg!(debug_assertions) || !latency_gate_armed() {
        return;
    }
    assert!(
        elapsed < PROMPT,
        "{what} took {elapsed:?}, over the {PROMPT:?} bound"
    );
}

/// Whether the latency bounds are being *asserted* rather than merely reported.
///
/// ```text
/// INSTAR_LATENCY_GATE=1 cargo test --release -p instar-host --test bridge -- --nocapture
/// ```
///
/// # Why an opt-in and not just "release"
///
/// A latency distribution is a measurement, and a measurement is only worth
/// asserting on a host that is doing nothing else. On the machine this was
/// written on — an ordinary desktop with a browser and two editors open — the
/// same suite reports p95 5.2ms idle and 17.9ms while a build runs. Asserting
/// against the second number teaches everyone to ignore a red suite, which
/// costs more than the gate is worth.
///
/// This is deliberately **not** the `#[cfg(any())]` it replaced. That was an
/// assertion switched off in a way nothing could see: it did not compile, it
/// left `PROMPT` dead, and it left the release body free to rot into an
/// infinite recursion. This compiles in every profile, runs on request, says
/// so when it does not, and prints the distribution unconditionally so the
/// numbers are never hidden — only the *judgement* is deferred to a run that
/// can support one.
fn latency_gate_armed() -> bool {
    std::env::var_os("INSTAR_LATENCY_GATE").is_some_and(|value| value != "0")
}

/// Warm-click latency targets. Asserted in release only, same reasoning as
/// above; defined unconditionally so the assertion that reads them always
/// compiles.
///
/// # `P50` has headroom now, and the others always did
///
/// Until Stage 2 none of these were asserted — [`assert_prompt`]'s release
/// body was dead, and nothing runs `--release` anyway (CI does not). Arming
/// them exposed one target set with no margin:
///
/// ```text
///        recorded   was     now    headroom
/// p50      4.94ms    5ms     7ms    1.01x -> 1.42x
/// p95      5.75ms    8ms     8ms    1.39x
/// p99     11.5 ms   16ms    16ms    1.39x
/// max    105    ms  250ms   250ms   2.4x     (PROMPT)
/// ```
///
/// 5ms was the Stage 1 measurement rounded up to the next integer, not a
/// stricter policy for p50 — its neighbours all sit near 1.4x. Six idle
/// release runs, interleaved between this commit and its parent, measured p50
/// at 4.86–4.98ms: between 0.4% and 2.9% below the bound. A ceiling on
/// brokenness that a healthy machine grazes is a flake generator, and under
/// any background load at all this one went to 6.66ms.
///
/// Those same runs are why the change is a calibration rather than a
/// concession: parent and child were indistinguishable (p50 4.95ms either
/// way), so the margin was always this thin and nothing in Stage 2 spent it.
const P50: Duration = Duration::from_millis(7);
const P95: Duration = Duration::from_millis(8);
const P99: Duration = Duration::from_millis(16);

fn component() -> Vec<u8> {
    std::fs::read(env!("HOSTILE_WASM")).expect("the hostile guest is built by build.rs")
}

fn metrics(scale: f64) -> WindowMetricsChanged {
    WindowMetricsChanged {
        window_id: WINDOW,
        logical_size: LogicalSize {
            width: 400.0,
            height: 400.0,
        },
        physical_size: PhysicalSize {
            width: (400.0 * scale) as u32,
            height: (400.0 * scale) as u32,
        },
        scale_factor: scale,
    }
}

/// Counts main-thread wakes, standing in for `EventLoopProxy::send_event`.
#[derive(Default)]
struct Wakes(AtomicU64);

impl Wakes {
    fn count(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn spawn() -> (HostBridge, Arc<Wakes>) {
    let wakes = Arc::new(Wakes::default());
    let counter = Arc::clone(&wakes);
    let wake: Wake = Arc::new(move || {
        counter.0.fetch_add(1, Ordering::SeqCst);
    });
    let bridge = HostBridge::spawn(component(), WINDOW, wake).expect("the guest starts");
    (bridge, wakes)
}

/// A bridge with metrics set and the guest's opening interface applied.
fn ready() -> (HostBridge, Arc<Wakes>) {
    let (mut bridge, wakes) = spawn();
    bridge.on_window_event(WindowOutput::MetricsChanged(metrics(1.0)));
    await_commit(&mut bridge).expect("the guest commits its opening interface");
    (bridge, wakes)
}

/// Waits for the host to refuse one more commit, returning how long it took.
///
/// A rejection is not a commit sequence, so [`await_commit`] would sit here until it
/// timed out — which is itself the shape of the bug this distinguishes: a host
/// that quietly *applied* a bad batch would satisfy `await_commit` and fail
/// this.
fn await_rejection(bridge: &mut HostBridge) -> Option<Duration> {
    let target = bridge.stats().rejected_commits + 1;
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        bridge.wait(Duration::from_millis(50));
        if bridge.stats().rejected_commits >= target {
            return Some(started.elapsed());
        }
    }
    None
}

/// Waits for the guest to be reported gone, returning the error it died with.
fn await_guest_gone(bridge: &mut HostBridge) -> Option<String> {
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        for effect in bridge.wait(Duration::from_millis(50)) {
            if let HostEffect::GuestGone { error, .. } = effect {
                return error;
            }
        }
    }
    panic!("the guest never reported that it was gone");
}

/// Waits for exactly one more applied commit, returning how long it took.
///
/// Returns `None` on timeout rather than panicking, so callers can say what
/// they were waiting for.
fn await_commit(bridge: &mut HostBridge) -> Option<Duration> {
    let target = bridge.commit_sequence() + 1;
    let started = Instant::now();
    while started.elapsed() < PATIENCE {
        bridge.wait(Duration::from_millis(50));
        if bridge.commit_sequence() >= target {
            return Some(started.elapsed());
        }
    }
    None
}

/// The centre of whatever the host placed at `key`, in logical coordinates.
fn centre(bridge: &HostBridge, key: NodeKey) -> (f64, f64) {
    let rect = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::layout)
        .and_then(|layout| layout.get(key))
        .unwrap_or_else(|| panic!("{key} should have host-computed geometry"));
    (
        f64::from(rect.x + rect.width / 2),
        f64::from(rect.y + rect.height / 2),
    )
}

/// The text the guest most recently committed for its label node.
fn label(bridge: &HostBridge) -> String {
    match bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::tree)
        .and_then(|tree| tree.find(LABEL))
        .map(|node| &node.kind)
    {
        Some(NodeKind::Text { text }) => text.clone(),
        other => panic!("the fixture's label should be a text node, found {other:?}"),
    }
}

fn pointer(state: PointerState, x: f64, y: f64) -> WindowOutput {
    WindowOutput::Pointer(RawPointerEvent {
        window_id: WINDOW,
        logical_pos: LogicalPoint::new(x, y),
        button: PointerButton::Primary,
        state,
    })
}

/// A full press-and-release over `key`. Returns nothing: the resulting
/// `SendToGuest` is consumed by the bridge and queued for the runtime thread.
fn click(bridge: &mut HostBridge, key: NodeKey) {
    let (x, y) = centre(bridge, key);
    bridge.on_window_event(pointer(PointerState::Pressed, x, y));
    bridge.on_window_event(pointer(PointerState::Released, x, y));
}

/// A valid batch the fixture would never send, so applying it is visible.
fn foreign_batch(text: &str) -> Vec<u8> {
    let fill = WireLayout {
        align_self: Some(WireAlign::Stretch),
        ..WireLayout::default()
    };
    let mut encoder = BatchEncoder::new();
    encoder
        .node(opcode::NODE_ROOT, NodeKey::first(0), 0, None, fill, 1)
        .node(opcode::NODE_COLUMN, NodeKey::first(1), 0, None, fill, 1)
        .node(
            opcode::NODE_TEXT,
            LABEL,
            flags::ENABLED,
            Some(text),
            WireLayout::default(),
            0,
        );
    encoder.finish()
}

// --- 1. The round-trip ---

/// The loop WP7B1 exists to close, with two threads in the middle of it:
///
/// ```text
/// main: click -> UiAction -> RuntimeCommand::DeliverEvent
/// runtime: guest wakes -> commit(batch).await  (suspended)
/// main: HostUserEvent::UiCommit -> apply -> layout -> reply
/// runtime: guest resumes -> back to next-event  (suspended, costing nothing)
/// ```
#[test]
fn a_click_round_trips_and_the_guest_re_suspends() {
    let (mut bridge, _wakes) = ready();
    assert_eq!(label(&bridge), "Clicked 0 times, 0 bulk");

    click(&mut bridge, COUNT);
    let elapsed = await_commit(&mut bridge).expect("the click produces a commit");
    assert_eq!(label(&bridge), "Clicked 1 times, 0 bulk");
    assert_prompt(elapsed, "a click round-trip");

    // A second click proves the first one ended where it should: back in
    // `next-event`. A guest that had not re-suspended could not answer.
    click(&mut bridge, COUNT);
    await_commit(&mut bridge).expect("the guest went back to waiting for events");
    assert_eq!(label(&bridge), "Clicked 2 times, 0 bulk");

    assert_eq!(bridge.stats().rejected_commits, 0);
    assert_eq!(bridge.stats().stale_commits, 0);
}

// --- 2. Rejection ---

/// A malformed commit must not blank the window, and must not leave the guest
/// waiting.
#[test]
fn an_invalid_commit_changes_nothing() {
    let (mut bridge, _wakes) = ready();
    let before = label(&bridge);
    let commit_sequence = bridge.commit_sequence();

    let (request, reply) = commit_request(bridge.generation(), b"not a batch at all".to_vec());
    let effects = bridge.on_user_event(HostUserEvent::UiCommit {
        generation: bridge.generation(),
        request,
    });

    assert!(effects.is_empty(), "nothing to redraw: nothing changed");
    assert_eq!(
        label(&bridge),
        before,
        "the previous interface still stands"
    );
    assert_eq!(
        bridge.commit_sequence(),
        commit_sequence,
        "no commit sequence was spent on it"
    );
    assert_eq!(bridge.stats().rejected_commits, 1);
    assert!(
        matches!(reply.blocking_recv(), Ok(Err(CommitRejection::Invalid(_)))),
        "the guest must be told why, not left suspended"
    );
}

// --- 3. Ordering ---

/// Order is the one thing a queue must not get wrong. The fixture's counter
/// makes a reordering visible: the counts would not be monotonic.
///
/// # Why this samples rather than enumerates
///
/// It used to demand the exact sequence `1..=100`, one label per 50ms `wait`.
/// That conflated ordering with throughput and was wrong twice over. `wait`
/// pumps the whole queue, so two commits landing inside one window are seen as
/// one — a *correct* run reports a gap at 10 and looks like a dropped click.
/// And 100 debug round-trips do not reliably fit in `PATIENCE`, so the same
/// test also failed for being slow, which is a property this file deliberately
/// does not assert in debug.
///
/// What is actually being tested survives sampling intact. A reordering makes
/// some observed count *decrease*, and under-sampling cannot hide that — it
/// drops observations, it does not reorder them. So: every count seen is
/// strictly greater than the last, all 100 activations land, and none is
/// dropped.
#[test]
fn a_hundred_rapid_activations_arrive_in_order() {
    const CLICKS: u64 = 100;
    /// Completion, not latency. Generous on purpose: a slow debug build is
    /// not a queue defect, and `PATIENCE` is calibrated for single operations.
    const BURST: Duration = Duration::from_secs(60);

    let (mut bridge, _wakes) = ready();
    let (x, y) = centre(&bridge, COUNT);
    let base = bridge.commit_sequence();

    // Queued as fast as the winit thread can produce them, with no pumping in
    // between -- which is exactly the burst a held-down key or a fast mouse
    // produces, and the case where a naive implementation interleaves.
    for _ in 0..CLICKS {
        bridge.on_window_event(pointer(PointerState::Pressed, x, y));
        bridge.on_window_event(pointer(PointerState::Released, x, y));
    }

    let mut counts = Vec::new();
    let mut last_sequence = base;
    let started = Instant::now();
    while bridge.commit_sequence() - base < CLICKS && started.elapsed() < BURST {
        bridge.wait(Duration::from_millis(50));
        if bridge.commit_sequence() > last_sequence {
            last_sequence = bridge.commit_sequence();
            counts.push(clicked_count(&label(&bridge)));
        }
    }

    assert_eq!(
        bridge.commit_sequence() - base,
        CLICKS,
        "every activation should be delivered and applied within {BURST:?}"
    );
    assert!(
        counts.windows(2).all(|pair| pair[0] < pair[1]),
        "counts must never go backwards; a reordered queue is what would make \
         them: {counts:?}"
    );
    assert_eq!(
        counts.last().copied(),
        Some(CLICKS),
        "the last interface observed should be the last activation applied"
    );
    assert_eq!(bridge.stats().dropped_commands, 0, "100 fits in 256");
}

/// The count out of a fixture label like `Clicked 7 times, 0 bulk`.
fn clicked_count(label: &str) -> u64 {
    label
        .strip_prefix("Clicked ")
        .and_then(|rest| rest.split_once(' '))
        .and_then(|(count, _)| count.parse().ok())
        .unwrap_or_else(|| panic!("unexpected fixture label {label:?}"))
}

// --- 4. Back-pressure ---

/// The winit thread must never wait for queue capacity. Here the runtime
/// thread is deliberately starved — nothing is pumped, so the guest stays
/// suspended on its first unanswered commit and stops draining commands.
#[test]
fn a_full_command_queue_never_blocks_the_winit_thread() {
    let (mut bridge, _wakes) = ready();
    let (x, y) = centre(&bridge, COUNT);

    // Comfortably more than the command queue plus the guest's inbox, so the
    // drop is a consequence of both bounds holding rather than of one of them
    // quietly absorbing the excess.
    let overflow = (QUEUE_CAPACITY + EVENT_QUEUE_CAPACITY) * 3;
    let started = Instant::now();
    for _ in 0..overflow {
        bridge.on_window_event(pointer(PointerState::Pressed, x, y));
        bridge.on_window_event(pointer(PointerState::Released, x, y));
    }
    let elapsed = started.elapsed();

    assert_prompt(elapsed, "a click round-trip");
    assert!(
        bridge.stats().dropped_commands > 0,
        "with {overflow} events against a {QUEUE_CAPACITY}-slot command queue \
         feeding a {EVENT_QUEUE_CAPACITY}-slot guest inbox, and nothing draining \
         either, some must have been dropped -- if none were, the backlog is \
         going somewhere unbounded"
    );
}

// --- 5. The wake ---

/// A main thread parked waiting for OS events has to be woken by the runtime
/// thread, or a commit sits in the queue until the user happens to move the
/// mouse. This is what `EventLoopProxy` is for, and the wake callback stands
/// in for it here.
#[test]
fn a_runtime_wake_reaches_a_parked_main_thread() {
    let (mut bridge, wakes) = ready();
    let before = wakes.count();

    click(&mut bridge, COUNT);

    // Park. Nothing else will disturb this thread: the only thing that can end
    // the wait is the runtime thread pushing the commit and waking us.
    let started = Instant::now();
    let effects = bridge.wait(PATIENCE);
    let elapsed = started.elapsed();

    assert_eq!(
        effects,
        vec![HostEffect::Render { window: WINDOW }],
        "the parked thread should come back holding the applied commit's frame"
    );
    assert_prompt(elapsed, "a click round-trip");
    assert!(
        wakes.count() > before,
        "the runtime thread must signal the wake, not rely on the main thread \
         looking of its own accord"
    );
}

// --- 6. Concurrent load, and the promptness bound ---

/// Wasmtime is explicit that a future inside `run_concurrent` may go unpolled
/// for a while after its waker fires. So the property is not "the commit
/// eventually lands" — it is that it lands *promptly* while genuine host work
/// is in flight on the same runtime thread.
#[test]
fn bulk_work_in_flight_does_not_delay_a_ui_commit() {
    let (mut bridge, _wakes) = ready();
    let baseline = bridge.live_operations();

    click(&mut bridge, BULK);
    await_commit(&mut bridge).expect("starting bulk work still commits");
    assert_eq!(
        bridge.live_operations(),
        baseline + 1,
        "the fixture's bulk button should leave a real operation in flight"
    );

    // Three seconds of host work is running on the runtime thread. A UI
    // round-trip must not queue behind it.
    for expected in 1..=5 {
        click(&mut bridge, COUNT);
        let elapsed = await_commit(&mut bridge).expect("a click commits under load");
        assert_prompt(elapsed, "a click round-trip");
        assert_eq!(label(&bridge), format!("Clicked {expected} times, 1 bulk"));
    }

    assert_eq!(
        bridge.live_operations(),
        baseline + 1,
        "the bulk operation should still be running, not have been drained by \
         the round-trips that overtook it"
    );
}

// --- 7. A dead guest cannot still mutate ---

/// A commit is in flight when its generation dies. The batch is already on the
/// main thread's queue; applying it afterwards would show an interface no
/// running guest believes in.
#[test]
fn a_trap_stops_an_in_flight_commit_from_mutating_anything() {
    let (mut bridge, _wakes) = ready();
    let generation = bridge.generation();

    // A commit from the live generation, held back rather than delivered --
    // the state a batch is in between leaving the runtime thread and being
    // applied.
    let (request, reply) = commit_request(generation, foreign_batch("applied after the trap"));

    click(&mut bridge, CRASH);
    let started = Instant::now();
    let mut ending = None;
    while ending.is_none() && started.elapsed() < PATIENCE {
        for effect in bridge.wait(Duration::from_millis(50)) {
            if let HostEffect::GuestGone { error, .. } = effect {
                ending = Some(error);
            }
        }
    }
    let error = ending
        .expect("the trapping guest should be reported")
        .expect("a panicking guest is a trap, not a clean exit");
    assert!(
        error.contains("crash button") || error.contains("trap") || error.contains("unreachable"),
        "the trap should carry the guest's own reason, got: {error}"
    );

    let before = label(&bridge);
    let effects = bridge.on_user_event(HostUserEvent::UiCommit {
        generation,
        request,
    });

    assert!(effects.is_empty(), "a dead generation asks for no frames");
    assert_eq!(
        label(&bridge),
        before,
        "a batch from a generation that no longer exists must never reach the tree"
    );
    assert_eq!(bridge.stats().stale_commits, 1);
    assert_eq!(
        reply.blocking_recv(),
        Ok(Err(CommitRejection::StaleGeneration)),
        "and the sender is told so, rather than the request being dropped silently"
    );
}

// --- 8. Teardown mid-commit ---

/// The nastiest shutdown: the guest is suspended inside `commit`, and the
/// main thread never answers. Waiting for it to return would hang forever, so
/// shutdown falls back to dropping the `Store` — which Wasmtime documents as
/// the only way to reclaim a suspended concurrent task.
#[test]
fn teardown_with_a_commit_in_flight_resolves_promptly() {
    let (mut bridge, _wakes) = ready();

    click(&mut bridge, COUNT);
    // Deliberately no pump. The guest is now parked inside `commit`, and its
    // request is sitting on a queue nobody is reading.
    std::thread::sleep(Duration::from_millis(50));

    let started = Instant::now();
    bridge.shutdown();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "shutdown took {elapsed:?}; a guest suspended on an unanswered commit \
         must be torn down, not waited on"
    );
}

// --- 9. Screening happens before decoding ---

/// Ordering, made observable. The batch below is undecodable, so a host that
/// decoded first would answer `Invalid` — the parser would fail before the
/// generation was ever consulted. Answering `StaleGeneration` is only possible
/// if the check genuinely came first.
#[test]
fn an_old_generation_commit_is_rejected_before_decoding() {
    let (mut bridge, _wakes) = ready();
    let old = bridge.generation();

    // Retire the generation the way a clean guest exit does: report the
    // terminal outcome, then let the next pump observe it before ordinary work.
    bridge.report_terminal(TerminalOutcome::GuestExited { generation: old });
    assert_eq!(
        bridge.pump(),
        vec![HostEffect::GuestGone {
            generation: old,
            error: None,
        }],
        "the exit is observed exactly once and produces the guest-gone effect"
    );
    assert!(
        bridge.pump().is_empty(),
        "a second pump must not re-observe the terminal outcome"
    );
    assert_eq!(
        bridge.generation(),
        GenerationId(0),
        "with no guest running, no generation is current"
    );

    let (request, reply) = commit_request(old, b"\xff\xff\xff undecodable".to_vec());
    let before = label(&bridge);
    bridge.on_user_event(HostUserEvent::UiCommit {
        generation: old,
        request,
    });

    assert_eq!(
        reply.blocking_recv(),
        Ok(Err(CommitRejection::StaleGeneration)),
        "a host that decoded first would have answered Invalid: these bytes do \
         not parse. StaleGeneration is proof the generation was checked first"
    );
    assert_eq!(bridge.stats().stale_commits, 1);
    assert_eq!(
        bridge.stats().rejected_commits,
        0,
        "the parser was never reached, so nothing was spent on a dead guest"
    );
    assert_eq!(label(&bridge), before);
}

// --- 10. Nothing accumulates ---

/// A thousand complete cycles. The interesting assertion is not that they all
/// work — it is that afterwards the bridge looks exactly like it did at the
/// start, because a bridge that leaks a little per click is a bridge that
/// dies after an afternoon.
#[test]
fn a_thousand_click_cycles_return_to_baseline() {
    const CYCLES: u64 = 1_000;

    let (mut bridge, _wakes) = ready();
    let operations = bridge.live_operations();
    assert_eq!(bridge.queued_commands(), 0, "baseline: nothing queued");

    let mut latencies = Latencies::with_capacity(CYCLES as usize);
    for n in 1..=CYCLES {
        click(&mut bridge, COUNT);
        let elapsed = await_commit(&mut bridge)
            .unwrap_or_else(|| panic!("cycle {n} of {CYCLES} did not complete"));
        latencies.record(elapsed);
    }
    latencies.report("click-to-committed-tree");

    assert_eq!(
        label(&bridge),
        format!("Clicked {CYCLES} times, 0 bulk"),
        "every cycle should have landed exactly once"
    );
    // Bounds the tail with the max rather than a percentile; see
    // `Latencies::assert_targets`.
    latencies.assert_targets();

    let stats = bridge.stats();
    assert_eq!(
        stats.applied_commits,
        CYCLES + 1,
        "one commit per click, plus the opening interface the guest committed \
         before any click existed"
    );
    assert_eq!(stats.stale_commits, 0);
    assert_eq!(stats.rejected_commits, 0);
    assert_eq!(stats.dropped_commands, 0);
    assert_eq!(
        bridge.live_operations(),
        operations,
        "no operation should outlive the click that started it"
    );
    assert_eq!(
        bridge.queued_commands(),
        0,
        "the command queue should be empty again, not slowly filling"
    );
    assert!(
        bridge.pump().is_empty(),
        "and nothing should be left waiting on the runtime->main queue"
    );
}

// --- HARDEN-2: single-flight commits ---

/// A guest fires 512 commits at once and then terminates. Exactly one commit
/// may become outstanding; the other 511 must be refused by the kernel before
/// a request is created, and the guest's own sequential summary commit proves
/// the single-flight slot released after the first resolved.
#[test]
fn a_flood_of_concurrent_commits_allows_one_and_then_terminates() {
    let (mut bridge, _wakes) = ready();
    let baseline_sequence = bridge.commit_sequence();
    let baseline_applied = bridge.stats().applied_commits;

    click(&mut bridge, FLOOD_COMMITS);
    let ending = await_guest_gone(&mut bridge);
    assert_eq!(
        ending, None,
        "the flood guest should terminate cleanly after its summary commit"
    );

    let summary = label(&bridge);
    assert_eq!(
        summary, "Flooded 512 commits: applied 1, in-progress 511, other 0",
        "the guest should observe exactly one accepted commit and 511 refusals"
    );
    assert_eq!(
        bridge.commit_single_flight_rejections(),
        511,
        "the kernel must count every in-progress refusal"
    );
    assert_eq!(
        bridge.commit_sequence() - baseline_sequence,
        2,
        "the first flood commit plus the sequential summary commit both apply"
    );
    assert_eq!(
        bridge.stats().applied_commits - baseline_applied,
        2,
        "no other flood attempt may reach the host"
    );
    assert_eq!(bridge.stats().rejected_commits, 0);
    assert_eq!(bridge.stats().stale_commits, 0);
    assert_eq!(bridge.stats().dropped_commits, 0);
    assert_eq!(bridge.stats().dropped_commands, 0);
    assert_eq!(bridge.queued_commands(), 0);
    assert!(
        bridge.pump().is_empty(),
        "terminal state and the ordinary queue are fully drained"
    );
    bridge.shutdown();
}

// --- WP8: the rest of what a guest is permitted to do wrong ---
//
// The ten tests above are WP7B1's gate. These are the breadth WP8 adds: every
// failure mode the protocol allows, driven by clicking a button, with the
// same assertion behind each one — the host refuses it, says so, keeps the
// last good interface, and still works afterwards.
//
// "Still works afterwards" is the part worth having. A host that survives a
// bad batch but is subtly poisoned by it passes a test that stops at the
// rejection.

/// Bytes that are not a batch at all: rejected at the wire layer, before
/// anything is interpreted.
#[test]
fn a_guest_committing_garbage_is_refused_and_keeps_working() {
    let (mut bridge, _wakes) = ready();
    let before = label(&bridge);
    let commit_sequence = bridge.commit_sequence();

    click(&mut bridge, GARBAGE);
    await_rejection(&mut bridge).expect("the host should refuse undecodable bytes");

    assert_eq!(bridge.stats().rejected_commits, 1);
    assert_eq!(
        bridge.commit_sequence(),
        commit_sequence,
        "a refused batch is not a commit sequence"
    );
    assert_eq!(
        label(&bridge),
        before,
        "the last good interface still stands"
    );

    // The part that matters: the guest is not poisoned, and neither is the host.
    click(&mut bridge, COUNT);
    await_commit(&mut bridge).expect("the guest still answers clicks");
    assert_eq!(label(&bridge), "Clicked 1 times, 0 bulk");
    assert_eq!(bridge.stats().rejected_commits, 1, "and nothing else broke");
}

/// A batch that parses cleanly and describes an impossible tree — two roots.
///
/// Distinct from garbage on purpose. The wire layer reports what the bytes
/// say; `instar-ui` decides whether that is a sensible interface. A host that
/// only checked parsing would apply this one.
#[test]
fn a_batch_that_parses_but_means_nothing_is_refused_too() {
    let (mut bridge, _wakes) = ready();
    let before = label(&bridge);

    click(&mut bridge, NONSENSE);
    await_rejection(&mut bridge).expect("the host should refuse a nested root");

    assert_eq!(bridge.stats().rejected_commits, 1);
    assert_eq!(label(&bridge), before);

    click(&mut bridge, COUNT);
    await_commit(&mut bridge).expect("the guest still answers clicks");
    assert_eq!(label(&bridge), "Clicked 1 times, 0 bulk");
}

/// A batch past the protocol's `MAX_BATCH_BYTES`.
///
/// The host must refuse it on size rather than attempt to decode a megabyte
/// of guest-chosen bytes to find out whether it is any good.
#[test]
fn an_oversized_batch_is_refused_on_size() {
    let (mut bridge, _wakes) = ready();
    let before = label(&bridge);

    click(&mut bridge, GIANT);
    let elapsed = await_rejection(&mut bridge).expect("the host should refuse an oversized batch");

    assert_prompt(elapsed, "a click round-trip");
    assert_eq!(bridge.stats().rejected_commits, 1);
    assert_eq!(label(&bridge), before);
}

/// A guest that simply stops describing an interface.
///
/// Not a trap, not an exit, not an error — and the host must not treat it as
/// any of those. There is nothing to report and nothing to clean up; the last
/// interface stays on screen and the guest stays alive.
#[test]
fn a_guest_that_goes_silent_is_not_mistaken_for_one_that_died() {
    let (mut bridge, _wakes) = ready();
    let before = label(&bridge);
    let commit_sequence = bridge.commit_sequence();

    click(&mut bridge, SILENT);

    // Give it every chance to do something wrong.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(500) {
        for effect in bridge.wait(Duration::from_millis(50)) {
            if let HostEffect::GuestGone { .. } = effect {
                panic!("a guest that stopped committing was reported as gone");
            }
        }
    }

    assert_eq!(
        bridge.commit_sequence(),
        commit_sequence,
        "nothing was committed"
    );
    assert_eq!(
        label(&bridge),
        before,
        "and the last interface still stands"
    );
    assert_eq!(
        bridge.stats().rejected_commits,
        0,
        "nothing was refused either"
    );
    assert_eq!(
        bridge.generation(),
        bridge.generation(),
        "the generation is still current"
    );
    assert!(bridge.live_operations() == 0);
}

/// The crash surface's own boundedness, end to end.
///
/// The unit tests clamp a string. This clamps a real trap, from a real guest,
/// that really panicked with far more text than the surface will hold — and
/// checks that the complete diagnostic still reaches the caller for the log.
#[test]
fn a_trap_with_an_enormous_message_still_leaves_a_bounded_crash_surface() {
    let (mut bridge, _wakes) = ready();

    click(&mut bridge, FLOOD);
    let error = await_guest_gone(&mut bridge).expect("a trap reports an error");

    assert!(
        error.len() > instar_host::present::MAX_CRASH_MESSAGE_BYTES,
        "the fixture should trap with more text than the surface will hold, \
         got {} bytes",
        error.len()
    );

    let PresentationState::Crashed { message, .. } = bridge.host().presentation() else {
        panic!("a trap should have crashed the presentation");
    };
    assert!(
        message.len() <= instar_host::present::MAX_CRASH_MESSAGE_BYTES + 64,
        "a {}-byte trap was retained as {} bytes",
        error.len(),
        message.len()
    );
    assert!(
        bridge
            .host()
            .window(WINDOW)
            .and_then(HostWindow::scene)
            .is_some(),
        "and it still produces a frame"
    );
}

/// One warm click, traced.
///
/// Not a gate — an instrument. A duration cannot answer "did one changed label
/// rebuild one layout, or all of them?", and that distinction decides whether
/// the cost is a cache-lifetime bug, font selection, or the layout and raster
/// work already known to be O(tree).
///
/// Run with `--release --nocapture`; in debug the numbers measure rustc's
/// optimization level more than Instar's design.
#[test]
fn trace_one_warm_click() {
    let (mut bridge, _wakes) = ready();

    // Warm everything: a first click pays whatever one-time costs exist, and
    // the trace is about the steady state.
    click(&mut bridge, COUNT);
    await_commit(&mut bridge).expect("warm-up click commits");

    let before = bridge.host().text_stats();
    click(&mut bridge, COUNT);
    let elapsed = await_commit(&mut bridge).expect("the traced click commits");
    let after = bridge.host().text_stats();

    let text_nodes = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::tree)
        .map(|tree| {
            tree.iter()
                .filter(|node| {
                    matches!(
                        node.kind,
                        instar_ui::NodeKind::Text { .. } | instar_ui::NodeKind::Button { .. }
                    )
                })
                .count()
        })
        .unwrap_or(0);

    println!("\n--- one warm click ---");
    println!("text-bearing nodes  {text_nodes}");
    println!("round trip          {elapsed:?}");
    println!("rebuilt             {}", after.rebuilt - before.rebuilt);
    println!(
        "relinebroken        {}",
        after.relinebroken - before.relinebroken
    );
    println!("reused              {}", after.reused - before.reused);
    println!("extracted           {}", after.extracted - before.extracted);
    println!(
        "\nexactly one label changed, so `rebuilt` should be 1. If it is \
         {text_nodes}, the cache is not surviving the commit boundary."
    );
}
