//! The GATE build's runner: production `guests/scratchpad`, unmodified,
//! `world kernel`, zero probe hostcalls. Only T0 (native input accepted)
//! and T5 (rasterization complete) are measured, both host-side, needing no
//! guest cooperation -- see benchmarks/text-latency/README.md for why this
//! is the number checked against p95 <= 5 ms and the DIAGNOSTIC build's
//! stage breakdown is not.

use std::time::{Duration, Instant};

use instar_shell::Presenter;
use instar_shell::test_harness::{RuntimeHarness, launch_component};
use instar_ui_protocol::NodeKey;
use instar_window::{LogicalSize, WindowId, WindowMetricsChanged};

use crate::sample::StageTimes;
use crate::wait::wait_for_new_revision;

pub const WINDOW: WindowId = WindowId::from_raw(1);
/// Matches `guests/scratchpad`'s own hardcoded `const SURFACE`
/// (`instar_ui_protocol::NodeKey::first(7)`) -- this benchmark cannot
/// discover it any other way, since the guest owns Surface identity.
pub const SURFACE: NodeKey = NodeKey::first(7);
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 480;

/// How long a single measured interaction is allowed to take before the
/// harness gives up and reports a failure. Generous relative to the 5 ms
/// target on purpose -- a slow sample is data (the max/p99 the report
/// shows), not a timeout the harness should hide by extending patience
/// until everything looks fast.
pub const PATIENCE: Duration = Duration::from_secs(5);

pub fn metrics() -> WindowMetricsChanged {
    WindowMetricsChanged {
        window_id: WINDOW,
        logical_size: LogicalSize {
            width: f64::from(WIDTH),
            height: f64::from(HEIGHT),
        },
        physical_size: instar_window::PhysicalSize {
            width: WIDTH,
            height: HEIGHT,
        },
        scale_factor: 1.0,
    }
}

pub struct GateRun {
    pub harness: RuntimeHarness,
    presenter: Presenter,
    /// Sums every rendered frame's pixel bytes so the optimizer cannot prove
    /// the render calls are dead and elide them -- the same discipline
    /// `uibench.rs` uses.
    checksum: u64,
}

impl GateRun {
    /// Launches the real, unmodified `guests/scratchpad` component and waits
    /// for its opening Surface scene, exactly like
    /// `crates/instar-shell/tests/scratchpad.rs` does.
    pub fn launch() -> Self {
        let component = std::fs::read(env!("SCRATCHPAD_WASM"))
            .expect("the Scratchpad GATE component is built by benchmarks/text-latency/build.rs");
        let mut harness = launch_component(component, metrics());
        let opening = wait_for_new_revision(None, PATIENCE, Duration::ZERO, || {
            harness.wait(Duration::from_millis(25));
            harness.surface_revision(SURFACE)
        });
        assert!(
            opening.is_some(),
            "Scratchpad never submitted its opening Surface scene within {PATIENCE:?}"
        );
        let presenter = Presenter::new(instar_paint::PhysicalSize {
            width: WIDTH,
            height: HEIGHT,
        })
        .expect("the headless presenter starts");
        Self {
            harness,
            presenter,
            checksum: 0,
        }
    }

    pub fn checksum(&self) -> u64 {
        self.checksum
    }

    /// Drives and measures one interaction. `inject` is responsible for
    /// calling exactly the `RuntimeHarness` methods that constitute one
    /// native input event (a keypress, an IME commit, a pointer sequence,
    /// ...); everything after that -- waiting for every one of those events
    /// to be fully processed (never stopping at the *first* resulting
    /// scene, see below), rasterizing the result, and recording T0/T5 -- is
    /// common to every workload.
    ///
    /// `guests/scratchpad`'s event loop calls `present()` exactly once per
    /// processed event, unconditionally, so the Surface revision advances
    /// by exactly one per message the host actually queued to the guest
    /// (`RuntimeHarness::guest_message_count`) -- counting messages queued
    /// by `inject`, not just "did any new revision appear," is what makes
    /// this correct for a multi-event interaction (a drag, a burst of wheel
    /// events, a preedit-then-commit sequence). Waiting for only the first
    /// new revision looked like it worked for single-event workloads, but
    /// silently left every later injected event to complete during the
    /// *next* measured interaction instead of this one -- and enough of
    /// that pressure was what overflowed the guest's bounded event queue
    /// and terminated the generation outright, the way this benchmark's own
    /// "already-completed frame satisfies the wait" mutant warns against.
    pub fn measure_one(&mut self, inject: impl FnOnce(&mut RuntimeHarness)) -> StageTimes {
        let baseline_revision = self.harness.surface_revision(SURFACE);
        let baseline_messages = self.harness.guest_message_count();
        let started = Instant::now();
        inject(&mut self.harness);
        let queued = self.harness.guest_message_count() - baseline_messages;
        assert!(
            queued > 0,
            "this workload's inject closure queued zero messages to the guest -- every \
             workload must produce at least one real event"
        );
        let target = baseline_revision.unwrap_or(0) + queued;
        let mut seen_effects = Vec::new();
        let new_revision = wait_for_new_revision(
            Some(target.saturating_sub(1)),
            PATIENCE,
            Duration::ZERO,
            || {
                seen_effects.extend(self.harness.wait(Duration::from_millis(2)));
                self.harness.surface_revision(SURFACE)
            },
        );
        for effect in &seen_effects {
            if let instar_host::HostEffect::GuestGone { error, .. } = effect {
                panic!(
                    "guest exited while waiting for {queued} queued event(s) to be fully \
                     processed: {error:?}"
                );
            }
        }
        assert!(
            new_revision.is_some_and(|revision| revision >= target),
            "only reached revision {:?} of target {target} ({queued} event(s) queued) within \
             {PATIENCE:?} -- the guest may have trapped or stalled (bridge stats: {:?})",
            new_revision,
            self.harness.bridge_stats(),
        );
        let scene = self
            .harness
            .scene()
            .expect("a scene must exist once a new revision was observed");
        let pixels = self
            .presenter
            .render(scene)
            .expect("the GATE component's own scene renders");
        self.checksum = self.checksum.wrapping_add(pixels.len() as u64);
        let t5 = started.elapsed();
        StageTimes {
            t0: Some(Duration::ZERO),
            t5: Some(t5),
            frames_rendered: 1,
            ..Default::default()
        }
    }
}
