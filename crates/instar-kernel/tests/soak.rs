//! WP4 soak: does the generation-teardown rule actually close Gate 0's leak
//! mode, or merely move it?
//!
//! Gate 0 found that abandoning a *started* guest task retains its runtime
//! bookkeeping for the life of its `Store`, and that abandoning repeatedly on
//! one `Store` accumulates it linearly (`docs/GATE-0.md`, Finding 5). The rule
//! adopted in response is that a guest's lifetime boundary is its `Store` plus
//! instance: to stop a guest you destroy the whole generation.
//!
//! That is a *claim*, and this file is what makes it falsifiable. It does the
//! exact thing Gate 0 showed to be leaky — enter a suspended guest, then stop
//! it — a thousand times, differing only in that it destroys the generation
//! instead of abandoning the task. If the workaround were cosmetic, the
//! resident set would climb the same way the concurrent-state table did.
//!
//! Marked `#[ignore]` because it takes ~1-2 minutes: run it with
//! `cargo test -p instar-kernel --test soak -- --ignored --nocapture`.
//! CI runs it on Linux only (see `.github/workflows/gate0.yml`).

use std::time::Duration;

use instar_kernel::runtime::{Runtime, guest_component_bytes};

const CYCLES: usize = 1_000;

/// Resident set size in kilobytes, or `None` where unsupported.
///
/// Deliberately shells out rather than pulling in a dependency: the kernel's
/// dependency list is a thing docs/PHASE-1.md constrains, and a soak test is
/// not a good reason to widen it.
fn resident_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p"])
            .arg(std::process::id().to_string())
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// 1,000 × (instantiate generation → enter suspended guest → destroy the whole
/// `Store`) must leave host bookkeeping bounded and memory flat.
#[tokio::test]
#[ignore = "soak test; run with --ignored"]
async fn generation_churn_stays_bounded() {
    let bytes = guest_component_bytes().expect("kernel guest fixture built by build.rs");
    let mut runtime = Runtime::new(&bytes).expect("runtime builds");
    let kernel = runtime.kernel();

    // Warm up before taking the memory baseline: the first few generations
    // fault in code and grow allocator arenas, and charging that to the loop
    // would make a flat trend look like growth.
    for _ in 0..25 {
        let mut generation = runtime.new_generation().await.expect("generation");
        let handle = generation.handle();
        {
            let mut run = std::pin::pin!(generation.run());
            tokio::select! {
                biased;
                _ = &mut run => panic!("guest exited during warmup"),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            handle.send("op:delay:60000").ok();
            tokio::select! {
                biased;
                _ = &mut run => panic!("guest exited during warmup"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        runtime.destroy_generation(generation);
    }

    let baseline_ops = kernel.live_operations();
    let baseline_rss = resident_kb();
    let mut samples: Vec<(usize, u64)> = Vec::new();

    for cycle in 0..CYCLES {
        let mut generation = runtime.new_generation().await.expect("generation");
        let handle = generation.handle();

        {
            let mut run = std::pin::pin!(generation.run());

            // Enter the guest and let it park in `next-event`.
            tokio::select! {
                biased;
                result = &mut run => panic!("guest exited early in cycle {cycle}: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(15)) => {}
            }

            // Leave a long operation genuinely in flight, so every cycle tears
            // down a generation that still owns host-side work. Without this
            // the soak would only exercise the easy case.
            handle.send("op:delay:60000").ok();
            tokio::select! {
                biased;
                result = &mut run => panic!("guest exited early in cycle {cycle}: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }

        // The whole point: destroy the generation rather than abandon its task.
        runtime.destroy_generation(generation);

        if cycle % 100 == 99
            && let Some(rss) = resident_kb()
        {
            samples.push((cycle + 1, rss));
            println!(
                "cycle {:>4}: rss {:>8} kB, live ops {}",
                cycle + 1,
                rss,
                kernel.live_operations()
            );
        }
    }

    // 1. Host bookkeeping returned to baseline.
    assert_eq!(
        kernel.live_operations(),
        baseline_ops,
        "operation registry did not return to baseline after {CYCLES} generations \
         -- host-owned work is outliving its generation"
    );

    // 2. No commit ever crossed a generation boundary.
    assert_eq!(
        kernel.stale_commits_rejected(),
        0,
        "a superseded generation committed after teardown"
    );

    // 3. Every generation got a distinct id, and the final one is what we ran.
    assert_eq!(
        kernel.current_generation().0 as usize,
        CYCLES + 25,
        "every cycle should have produced exactly one new generation"
    );

    // 4. Memory did not grow linearly. The Gate 0 leak mode was strictly
    //    linear in the number of abandoned tasks, so a linear fit is the
    //    signal to look for -- not an absolute ceiling, which would just be a
    //    proxy for allocator behaviour.
    if samples.len() < 2 {
        eprintln!("RSS unavailable on this platform; skipped the memory assertion");
        return;
    }

    // Measure the trend across the sampled cycles rather than against
    // `baseline_rss`. The baseline is taken right after warmup, which is a
    // high-water mark -- comparing to it lets any result look like "no growth"
    // simply because the allocator gave pages back, which would make this
    // assertion unfalsifiable. First sample to last is the honest slope.
    let (first_cycle, first_rss) = samples[0];
    let (last_cycle, last_rss) = *samples.last().expect("samples is non-empty");
    let spanned_cycles = last_cycle - first_cycle;
    let growth_kb = last_rss as i64 - first_rss as i64;
    let growth_per_cycle = growth_kb as f64 / spanned_cycles as f64;

    println!(
        "warmup baseline {} kB; cycle {first_cycle} = {first_rss} kB -> cycle {last_cycle} = \
         {last_rss} kB ({growth_kb:+} kB over {spanned_cycles} cycles, \
         {growth_per_cycle:+.3} kB/cycle)",
        baseline_rss.map_or_else(|| "?".to_string(), |kb| kb.to_string()),
    );

    // A leaked Store per cycle would cost far more than this; the threshold is
    // loose enough to tolerate allocator noise and fragmentation, tight enough
    // that a genuine per-generation leak cannot hide under it.
    assert!(
        growth_per_cycle < 8.0,
        "resident set grew {growth_per_cycle:+.3} kB per generation across {spanned_cycles} \
         cycles ({first_rss} kB -> {last_rss} kB). That is the shape of a per-generation \
         leak: destroying the Store is supposed to reclaim everything the generation \
         owned. Samples: {samples:?}"
    );
}
