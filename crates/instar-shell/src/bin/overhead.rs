//! Overhead profiles A–D (WP9): what each layer of Instar actually costs.
//!
//! ```text
//! A  kernel + guest, settled            what running a component costs
//! B  A + host: layout, routing, scenes  what orchestration adds
//! C  B + renderer + font, one frame     what presentation adds
//! D  C driven through 100 click cycles  what activity costs, and whether
//!                                       it gives the memory back
//! ```
//!
//! ```text
//! cargo run --release --bin overhead
//! ```
//!
//! # Why one process with four checkpoints, and not four processes
//!
//! The first version of this ran each profile in its own process and compared
//! their resident sizes. The numbers were nonsense: the *same* profile
//! measured between 22 MB and 51 MB across three consecutive runs, and
//! profile C came out smaller than profile A. Nothing was wrong with the
//! program; cross-process RSS on a loaded machine mostly measures when the
//! kernel last reclaimed pages.
//!
//! Measuring one process as it grows fixes that. Each stage's cost is the
//! difference from the previous checkpoint, inside a single address space,
//! with no reclaim boundary in between — which is also the number the question
//! was actually asking for. The stages accumulate on purpose: B is A plus a
//! host, and cannot be had without it.
//!
//! # What these numbers are and are not
//!
//! `docs/PHASE-1.md`'s measurement policy: memory and startup are *discovery*
//! metrics. Nothing here is a target and no test asserts on any of it. The job
//! is to learn what the design costs, so a later change that doubles it is
//! visible.
//!
//! Absolute RSS is still the least trustworthy column and is reported for
//! context, not for comparison. The deltas and the idle counters are the
//! findings.
//!
//! # Idle is measured as work, not as CPU percentage
//!
//! A settled Instar should do *nothing*, and "0.0% CPU" from a sampler
//! measures the sampler's resolution as much as the program. So the idle
//! window reports what the runtime was actually asked to do: wakes delivered
//! to the main thread, and commits applied. Both must be zero. Gate 0 proves
//! the same property at the poll level; this checks it survives the whole
//! stack, with a window long enough that the predecessor's 10ms ticker would
//! have shown around 500.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use instar_host::bridge::{HostBridge, Wake};
use instar_host::{HostWindow, NodeKey};
use instar_paint::PhysicalSize;
use instar_shell::{Presenter, default_font};
use instar_window::{
    LogicalPoint, LogicalSize, PointerButton, PointerState, RawPointerEvent, WindowId,
    WindowMetricsChanged, WindowOutput,
};

const COUNTER: &[u8] = include_bytes!(env!("COUNTER_WASM"));

const WINDOW: WindowId = WindowId::from_raw(1);
const INCREMENT: NodeKey = NodeKey(4);

const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;

/// How long each settled stage is observed.
const IDLE_WINDOW: Duration = Duration::from_secs(3);

/// Cycles stage D drives before settling again.
const CYCLES: u32 = 100;

fn main() {
    let mut run = Run::new();

    // --- baseline: the process, before any Instar exists in it ---
    run.checkpoint("baseline", "the process before any Instar exists in it");

    // --- A: the kernel running a component ---
    //
    // No metrics are given to the host, so nothing is laid out, no scene is
    // lowered, and nothing is rendered: the commit is accepted against a
    // blocked window and deferred. What this costs is what hosting a
    // WebAssembly component costs.
    let started = Instant::now();
    let (mut bridge, wakes) = spawn(false);
    settle(&mut bridge);
    let startup = started.elapsed();
    run.note("A_startup_ms", format!("{:.1}", ms(startup)));
    run.note("A_component_bytes", COUNTER.len().to_string());
    run.idle("A", &mut bridge, &wakes);
    run.checkpoint("A", "kernel + guest, settled");

    // --- B: the host doing its job ---
    //
    // Metrics arrive, so the tree is laid out and a scene is lowered. The
    // difference from A is Taffy's arena, the LayoutSnapshot, and the
    // PaintScene: turning a described interface into a drawable one.
    let started = Instant::now();
    bridge.on_window_event(WindowOutput::MetricsChanged(metrics()));
    let laid_out = started.elapsed();
    run.note(
        "B_layout_us",
        format!("{:.1}", laid_out.as_secs_f64() * 1e6),
    );
    run.note(
        "B_paint_commands",
        bridge
            .host()
            .window(WINDOW)
            .and_then(HostWindow::scene)
            .map_or(0, |scene| scene.commands.len())
            .to_string(),
    );
    run.idle("B", &mut bridge, &wakes);
    run.checkpoint("B", "A + host: layout, routing, scene lowering");

    // --- C: a font and a rasterized frame ---
    //
    // The renderer's context, its scratch buffers, the glyph atlas, the font
    // file, and one RenderTarget at the window's size. Everything the windowed
    // shell holds except the window itself.
    //
    // The bridge is replaced rather than adapted: a glyph source is a
    // constructor argument, because a font arriving after the guest's opening
    // commit would mean the first frame silently had no labels.
    bridge.shutdown();
    let started = Instant::now();
    let (mut bridge, wakes) = spawn(true);
    bridge.on_window_event(WindowOutput::MetricsChanged(metrics()));
    settle(&mut bridge);
    let mut presenter = Presenter::new(PhysicalSize {
        width: WIDTH,
        height: HEIGHT,
    })
    .expect("the renderer starts");
    let first_frame = render(&bridge, &mut presenter);
    run.note("C_cold_start_ms", format!("{:.1}", ms(started.elapsed())));
    run.note("C_first_frame_ms", format!("{:.2}", ms(first_frame)));
    run.note(
        "C_frame_bytes",
        (WIDTH as usize * HEIGHT as usize * 4).to_string(),
    );
    run.idle("C", &mut bridge, &wakes);
    run.checkpoint("C", "B + font + Vello CPU, one frame rendered");

    // --- D: driven ---
    //
    // A hundred complete cycles: click, guest wakes, commits, host lays out
    // and lowers, renderer rasterizes. Then it settles again and is measured
    // in the same state C was.
    //
    // The question is not what the peak was. It is whether a program that has
    // *done* something looks like one that has not — because a runtime that
    // keeps a little of every interaction dies after an afternoon.
    let mut slowest_cycle = Duration::ZERO;
    let mut slowest_frame = Duration::ZERO;
    for n in 1..=CYCLES {
        let cycle = Instant::now();
        click(&mut bridge);
        let target = bridge.revision() + 1;
        let waited = Instant::now();
        while bridge.revision() < target && waited.elapsed() < Duration::from_secs(5) {
            bridge.wait(Duration::from_millis(10));
        }
        assert!(bridge.revision() >= target, "cycle {n} never committed");
        slowest_frame = slowest_frame.max(render(&bridge, &mut presenter));
        slowest_cycle = slowest_cycle.max(cycle.elapsed());
    }
    run.note("D_cycles", CYCLES.to_string());
    run.note("D_slowest_cycle_ms", format!("{:.2}", ms(slowest_cycle)));
    run.note("D_slowest_frame_ms", format!("{:.2}", ms(slowest_frame)));
    run.idle("D", &mut bridge, &wakes);
    run.checkpoint("D", "C driven through 100 cycles, then settled");

    bridge.shutdown();
    run.report();
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

// --- measurement ---

struct Checkpoint {
    name: &'static str,
    what: &'static str,
    rss_kib: Option<u64>,
    threads: Option<u64>,
}

struct Run {
    checkpoints: Vec<Checkpoint>,
    notes: Vec<(&'static str, String)>,
}

impl Run {
    fn new() -> Self {
        Self {
            checkpoints: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn checkpoint(&mut self, name: &'static str, what: &'static str) {
        self.checkpoints.push(Checkpoint {
            name,
            what,
            rss_kib: rss_kib(),
            threads: thread_count(),
        });
    }

    fn note(&mut self, key: &'static str, value: String) {
        self.notes.push((key, value));
    }

    /// Observes a settled stage. Both counters must come back zero.
    fn idle(&mut self, stage: &'static str, bridge: &mut HostBridge, wakes: &Wakes) {
        let wakes_before = wakes.0.load(Ordering::SeqCst);
        let commits_before = bridge.stats().applied_commits;

        let started = Instant::now();
        while started.elapsed() < IDLE_WINDOW {
            // Parks the thread rather than spinning it: an idle *host* is as
            // much the claim as an idle guest, and a busy-wait here would
            // measure this loop instead of the system.
            bridge.wait(Duration::from_millis(250));
        }

        let woken = wakes.0.load(Ordering::SeqCst) - wakes_before;
        let committed = bridge.stats().applied_commits - commits_before;
        self.notes.push((
            Box::leak(format!("{stage}_idle_wakes").into_boxed_str()),
            woken.to_string(),
        ));
        self.notes.push((
            Box::leak(format!("{stage}_idle_commits").into_boxed_str()),
            committed.to_string(),
        ));
    }

    /// Tab-separated: greppable, diffable, and pasteable into
    /// `docs/OVERHEAD.md` without a parser.
    fn report(&self) {
        println!("idle_window_s\t{}", IDLE_WINDOW.as_secs());
        for (key, value) in &self.notes {
            println!("{key}\t{value}");
        }
        println!();
        println!("stage\trss_kib\tdelta_kib\tthreads\twhat");
        let mut previous: Option<u64> = None;
        for checkpoint in &self.checkpoints {
            let delta = match (previous, checkpoint.rss_kib) {
                (Some(before), Some(now)) => (now as i64 - before as i64).to_string(),
                _ => "?".to_string(),
            };
            println!(
                "{}\t{}\t{}\t{}\t{}",
                checkpoint.name,
                checkpoint.rss_kib.map_or("?".into(), |v| v.to_string()),
                delta,
                checkpoint.threads.map_or("?".into(), |v| v.to_string()),
                checkpoint.what,
            );
            previous = checkpoint.rss_kib.or(previous);
        }
    }
}

/// Resident set size in KiB.
///
/// Deliberately not a crate: one `ps` call or one `/proc` read is the whole
/// requirement, and a tool that adds a dependency to the thing it measures has
/// a conflict of interest.
fn rss_kib() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // statm's second field is resident pages.
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        Some(pages * 4) // 4 KiB pages
    }
    #[cfg(target_os = "macos")]
    {
        ps_field("rss")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // Windows would need a Win32 call and therefore a dependency. These
        // are discovery metrics on a design that is identical across
        // platforms, so two platforms is enough to learn from.
        None
    }
}

fn thread_count() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("Threads:"))
            .and_then(|value| value.trim().parse().ok())
    }
    #[cfg(target_os = "macos")]
    {
        // `ps -M` lists one line per thread under a header. There is no `ps`
        // format field for the count on macOS, which is why this counts lines
        // rather than reading a number.
        let output = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let lines = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count() as u64;
        lines.checked_sub(1)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
fn ps_field(field: &str) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args([
            "-o",
            &format!("{field}="),
            "-p",
            &std::process::id().to_string(),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

// --- driving ---

fn metrics() -> WindowMetricsChanged {
    WindowMetricsChanged {
        window_id: WINDOW,
        logical_size: LogicalSize {
            width: WIDTH as f64,
            height: HEIGHT as f64,
        },
        physical_size: instar_window::PhysicalSize {
            width: WIDTH,
            height: HEIGHT,
        },
        scale_factor: 1.0,
    }
}

/// Counts main-thread wakes, standing in for `EventLoopProxy::send_event`.
#[derive(Default)]
struct Wakes(AtomicU64);

fn spawn(glyphs: bool) -> (HostBridge, Arc<Wakes>) {
    let wakes = Arc::new(Wakes::default());
    let counter = Arc::clone(&wakes);
    let wake: Wake = Arc::new(move || {
        counter.0.fetch_add(1, Ordering::SeqCst);
    });

    let bridge = if glyphs {
        let font = default_font().expect("the shipped face parses");
        HostBridge::spawn_with_glyphs(COUNTER.to_vec(), WINDOW, wake, Arc::new(font))
    } else {
        HostBridge::spawn(COUNTER.to_vec(), WINDOW, wake)
    }
    .expect("the counter guest starts");

    (bridge, wakes)
}

fn settle(bridge: &mut HostBridge) {
    let target = bridge.revision() + 1;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        bridge.wait(Duration::from_millis(25));
        if bridge.revision() >= target {
            return;
        }
    }
    panic!("the guest never committed");
}

/// Rasterizes the host's current scene, returning how long it took.
fn render(bridge: &HostBridge, presenter: &mut Presenter) -> Duration {
    let scene = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::scene)
        .expect("a ready window always has something to present");
    let started = Instant::now();
    presenter.render(scene).expect("the host's scene renders");
    started.elapsed()
}

fn click(bridge: &mut HostBridge) {
    let rect = bridge
        .host()
        .window(WINDOW)
        .and_then(HostWindow::layout)
        .and_then(|layout| layout.get(INCREMENT))
        .expect("the button is laid out");
    let (x, y) = (
        f64::from(rect.x + rect.width / 2),
        f64::from(rect.y + rect.height / 2),
    );
    for state in [PointerState::Pressed, PointerState::Released] {
        bridge.on_window_event(WindowOutput::Pointer(RawPointerEvent {
            window_id: WINDOW,
            logical_pos: LogicalPoint::new(x, y),
            button: PointerButton::Primary,
            state,
        }));
    }
}
