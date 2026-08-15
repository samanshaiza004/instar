//! Phase 3 text-latency benchmark: real end-to-end evidence for the
//! userland-authority model's p95 native-input -> rasterized-pixels <= 5 ms
//! target. See README.md for the full methodology; this file is
//! orchestration only.
//!
//! ```text
//! cargo run --release --manifest-path benchmarks/text-latency/Cargo.toml -- measure --output benchmarks/text-latency/results/reference
//! ```

mod counters;
mod gate;
mod sample;
mod stats;
mod wait;
mod workloads;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gate::GateRun;
use sample::StageTimes;
use stats::{Percentiles, fmt_ms, percentiles};

/// The frozen target this benchmark checks the GATE build's p95 against.
/// Stricter than the older, looser UI-interaction gate
/// (`crates/instar-host/tests/bridge.rs`'s P50/P95/P99 = 7/8/16 ms) --
/// see README.md.
const P95_TARGET: Duration = Duration::from_millis(5);

/// Samples per workload. Odd is not required here (unlike `uibench.rs`'s
/// medians): percentile rank does not need an exact middle sample.
const DEFAULT_ITERATIONS: usize = 60;

struct WorkloadResult {
    name: &'static str,
    what: &'static str,
    percentiles: Percentiles,
    graded: bool,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(String::as_str) != Some("measure") {
        eprintln!("usage: text-latency-bench measure [--output DIR] [--iterations N]");
        std::process::exit(2);
    }
    let mut output: Option<PathBuf> = None;
    let mut iterations = DEFAULT_ITERATIONS;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--output" => {
                i += 1;
                output = Some(PathBuf::from(&args[i]));
            }
            "--iterations" => {
                i += 1;
                iterations = args[i].parse().expect("--iterations takes an integer");
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let mut results = Vec::new();
    let mut total_checksum: u64 = 0;

    // A fresh GateRun per workload, not one shared across all of them: most
    // workloads insert text and never delete it, so sharing one guest
    // instance across every workload's iterations would let insertions
    // compound (200 iterations of the 100 KiB stress workload alone would
    // grow the document by ~20 MB before the next workload even starts),
    // contaminating every later workload's document-size backdrop and, at
    // the extreme, exhausting patience entirely. Each workload measures
    // against the same *intended* starting condition instead.
    macro_rules! workload {
        ($name:expr, $what:expr, $graded:expr, $iters:expr, $setup:expr, $body:expr) => {{
            eprintln!("running workload {} ({} iterations)...", $name, $iters);
            let mut run = GateRun::launch();
            let setup: fn(&mut GateRun) = $setup;
            workloads::focus_surface(&mut run.harness);
            setup(&mut run);
            let mut samples = Vec::with_capacity($iters);
            for _ in 0..$iters {
                workloads::focus_surface(&mut run.harness);
                let stage_times: StageTimes = run.measure_one(|harness| $body(harness));
                if let Err(error) = stage_times.validate() {
                    panic!("workload {} produced an invalid sample: {error}", $name);
                }
                if let Some(total) = stage_times.total() {
                    samples.push(total);
                }
            }
            total_checksum = total_checksum.wrapping_add(run.checksum());
            results.push(WorkloadResult {
                name: $name,
                what: $what,
                percentiles: percentiles(&samples),
                graded: $graded,
            });
        }};
        ($name:expr, $what:expr, $graded:expr, $iters:expr, $body:expr) => {
            workload!($name, $what, $graded, $iters, |_: &mut GateRun| {}, $body)
        };
    }

    workload!("ascii_typing", "one ordinary ASCII keystroke", true, iterations, |h| {
        workloads::ascii_typing(h, 'x')
    });
    workload!(
        "key_repeat",
        "one autorepeated keydown (repeat=true)",
        true,
        iterations,
        |h| workloads::key_repeat(h, 'x')
    );
    workload!(
        "unicode_combining",
        "combining-mark commit (e + acute + acute + n + tilde)",
        true,
        iterations,
        |h| workloads::unicode_combining_commit(h)
    );
    workload!(
        "bidi_text",
        "mixed Hebrew/Latin commit forcing bidi reordering",
        true,
        iterations,
        |h| workloads::bidi_commit(h)
    );
    workload!(
        "ime_commit",
        "two-update preedit composition then commit",
        true,
        iterations,
        |h| workloads::ime_commit_sequence(h)
    );
    workload!(
        "multiline_preedit",
        "one multi-line preedit update",
        true,
        iterations,
        |h| workloads::multiline_preedit_update(h)
    );
    {
        // A fresh guest per *iteration*, not just per workload: this
        // workload's own single measured interaction inserts substantial,
        // never-removed text, so even within one workload's guest instance
        // repeated iterations would compound into an ever-growing document
        // and stop measuring "one large commit" at a stable baseline.
        // Capped independently of --iterations because relaunching a
        // component per sample is itself not free.
        let name = "large_text_commit_stress";
        eprintln!("running workload {name} (fresh guest per iteration)...");
        let capped = iterations.min(30);
        let mut samples = Vec::with_capacity(capped);
        let mut checksum = 0u64;
        for _ in 0..capped {
            let mut run = GateRun::launch();
            workloads::focus_surface(&mut run.harness);
            let stage_times = run.measure_one(|h| workloads::large_text_commit_stress(h));
            stage_times
                .validate()
                .unwrap_or_else(|error| panic!("workload {name} produced an invalid sample: {error}"));
            if let Some(total) = stage_times.total() {
                samples.push(total);
            }
            checksum = checksum.wrapping_add(run.checksum());
        }
        total_checksum = total_checksum.wrapping_add(checksum);
        results.push(WorkloadResult {
            name,
            what: "single commit at the protocol's actual max (MAX_TEXT_BYTES=4096; not a paste \
                    benchmark -- no clipboard event exists, and no single event can carry 100 KiB \
                    at all -- see workloads.rs)",
            percentiles: percentiles(&samples),
            graded: true,
        });
    }
    workload!("pointer_placement", "one click inside the Surface", true, iterations, |h| {
        workloads::pointer_placement(h)
    });
    workload!(
        "drag_selection",
        "press, 6 intermediate moves, release",
        true,
        iterations,
        |h| workloads::drag_selection(h)
    );
    workload!("rapid_scrolling", "burst of 8 wheel events", true, iterations, |h| {
        workloads::rapid_scrolling(h)
    });

    // 12/13/14: preload once (as this workload's setup, against its own
    // fresh guest instance), then measure one ordinary keystroke per
    // iteration against that backdrop -- the critical assertion is that
    // this keystroke's latency does not scale with the preload size.
    workload!(
        "keystroke_after_1mib_doc",
        "one keystroke, 1 MiB document preloaded",
        true,
        iterations.min(50),
        |run: &mut GateRun| workloads::preload_document(run, 1 << 20, 200),
        |h| workloads::ascii_typing(h, 'y')
    );
    workload!(
        "keystroke_after_10mib_doc",
        "one keystroke, 10 MiB document preloaded",
        true,
        iterations.min(50),
        |run: &mut GateRun| workloads::preload_document(run, 10 << 20, 200),
        |h| workloads::ascii_typing(h, 'y')
    );
    workload!(
        "keystroke_pathological_long_line",
        "one keystroke, 128 KiB single unbroken line preloaded",
        true,
        // Smaller than the 1 MiB / 10 MiB paragraphed documents above on
        // purpose: F1 in docs/DOS-STARVATION-AUDIT.md measured a single
        // *4 KiB* unbroken line at 4.75-12.7 ms already at or above the
        // whole gate budget by itself; 128 KiB is 32x that, deliberately
        // pathological without being untestable.
        iterations.min(20),
        |run: &mut GateRun| workloads::preload_document(run, 128 << 10, 0),
        |h| workloads::ascii_typing(h, 'y')
    );

    println!(
        "checksum(pixels observed, non-zero proves rendering really ran): {}",
        total_checksum
    );

    let output_dir = output.unwrap_or_else(|| PathBuf::from("benchmarks/text-latency/results/reference"));
    write_report(&output_dir, &results, iterations);
}

fn write_report(dir: &Path, results: &[WorkloadResult], iterations: usize) {
    fs::create_dir_all(dir).expect("results directory is writable");

    let mut summary = String::new();
    summary.push_str("workload\tp50_ms\tp95_ms\tp99_ms\tmax_ms\tcount\tgraded\tpass_p95_5ms\twhat\n");
    let mut worst_p95 = Duration::ZERO;
    let mut any_graded_failed = false;
    for result in results {
        let pass = result.percentiles.p95 <= P95_TARGET;
        if result.graded {
            worst_p95 = worst_p95.max(result.percentiles.p95);
            if !pass {
                any_graded_failed = true;
            }
        }
        summary.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            result.name,
            fmt_ms(result.percentiles.p50),
            fmt_ms(result.percentiles.p95),
            fmt_ms(result.percentiles.p99),
            fmt_ms(result.percentiles.max),
            result.percentiles.count,
            result.graded,
            if result.graded { pass.to_string() } else { "n/a (characterization only)".to_string() },
            result.what,
        ));
    }
    fs::write(dir.join("summary.csv"), &summary).expect("results directory is writable");

    let verdict = if any_graded_failed { "FAIL" } else { "PASS" };
    let rustc_version = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_else(|| "unknown".to_string());
    let cpu_brand = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_else(|| "unknown".to_string());
    let profile = if cfg!(debug_assertions) { "dev (unoptimized)" } else { "release" };
    let metadata = format!(
        "timestamp_unix_ns={}\n\
         os={}\n\
         arch={}\n\
         cpu={}\
         rustc={}\
         cargo_profile={profile}\n\
         window_size=640x480 scale_factor=1.0\n\
         iterations_per_workload={iterations}\n\
         gate_target_p95_ms=5.000\n\
         gate_worst_graded_p95_ms={}\n\
         verdict={verdict}\n\
         measurement_boundaries:\n\
         T0=harness feeds a real winit::event::WindowEvent/WindowOutput into the production bridge (native input accepted)\n\
         T5=Presenter::render returns RGBA pixels (rasterization complete -- NOT presented to a compositor; see README.md)\n\
         gate_build=production guests/scratchpad, world kernel, zero probe hostcalls\n\
         mutant_tests=cargo test --release (12 tests, 4 are the required mutant/fault checks; see src/sample.rs, src/wait.rs, src/counters.rs)\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        std::env::consts::OS,
        std::env::consts::ARCH,
        cpu_brand,
        rustc_version,
        fmt_ms(worst_p95),
    );
    fs::write(dir.join("metadata.txt"), metadata).expect("results directory is writable");

    println!("\n=== GATE build: p95 <= 5ms verdict: {verdict} ===");
    println!("worst graded p95: {} ms", fmt_ms(worst_p95));
    for result in results {
        println!(
            "{:<32} p50={:>8}ms p95={:>8}ms p99={:>8}ms max={:>8}ms n={}",
            result.name,
            fmt_ms(result.percentiles.p50),
            fmt_ms(result.percentiles.p95),
            fmt_ms(result.percentiles.p99),
            fmt_ms(result.percentiles.max),
            result.percentiles.count,
        );
    }
    println!("\nwritten to {}", dir.display());
}
