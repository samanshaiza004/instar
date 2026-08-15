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

/// How long [`GateRun::settle`] is willing to wait for the guest/host to go
/// quiet before giving up and returning anyway. Short and best-effort on
/// purpose: settling absorbs setup/trailing effects between samples, it is
/// never part of a measured interval, and a workload that never quiets down
/// is a bug the *next* sample's `measure_one` will surface on its own terms
/// (a stale baseline, a queue overflow) rather than one `settle` should hang
/// the whole run trying to diagnose.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(500);
/// Consecutive empty polls before quiescence is declared.
const SETTLE_IDLE_POLLS: u32 = 3;
const SETTLE_POLL: Duration = Duration::from_millis(2);

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
    /// A running FNV-1a hash of every rendered frame's actual pixel bytes,
    /// not their length. Every 640x480 RGBA frame is the same *length*
    /// regardless of content, so length alone cannot show a frame really
    /// changed -- see benchmarks/text-latency/README.md.
    checksum: u64,
}

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
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
        let mut run = Self {
            harness,
            presenter,
            checksum: FNV_OFFSET_BASIS,
        };
        run.settle();
        run
    }

    pub fn checksum(&self) -> u64 {
        self.checksum
    }

    /// Drains pending guest/host activity to quiescence without timing any
    /// of it. Call this after setup work (a focus click, a preload chunk)
    /// and it is also called at the start and end of [`Self::measure_one`]
    /// -- so trailing effects from whatever ran before a sample never
    /// contaminate that sample's baseline, and trailing effects from the
    /// sample itself never bleed into the *next* one's baseline.
    ///
    /// This used to be "wait for exactly as many new Surface revisions as
    /// messages were queued," on the assumption that `guests/scratchpad`
    /// presented once per processed event unconditionally. It no longer
    /// does: dirty-presentation correctly skips `present()` for events that
    /// change nothing visible (a key release, a passive pointer move,
    /// focus-gained, `ImeEnabled`, metrics), so that assumption is simply
    /// wrong now, in both directions -- it can wait forever for a revision
    /// that dirty presentation correctly never produces, and it can let
    /// unsettled setup events satisfy part of an unrelated later sample.
    /// Quiescence (no host effects for a few consecutive short polls) is
    /// what this needs instead: it does not assume how many renders an
    /// interaction produces, including zero.
    pub fn settle(&mut self) {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        let mut idle_polls = 0u32;
        loop {
            let effects = self.harness.wait(SETTLE_POLL);
            for effect in &effects {
                if let instar_host::HostEffect::GuestGone { error, .. } = effect {
                    panic!("guest exited while settling: {error:?}");
                }
            }
            if effects.is_empty() {
                idle_polls += 1;
                if idle_polls >= SETTLE_IDLE_POLLS {
                    return;
                }
            } else {
                idle_polls = 0;
            }
            if Instant::now() >= deadline {
                return;
            }
        }
    }

    /// Fires `inject` and settles afterward, producing no sample. For setup
    /// (a focus click before a workload's measured iterations) and for
    /// companion events that are real but deliberately outside the timed
    /// interval (a key release after a measured keydown -- see
    /// `workloads::ascii_typing`'s doc comment for why release is not part
    /// of what this benchmark times).
    pub fn send_untimed(&mut self, inject: impl FnOnce(&mut RuntimeHarness)) {
        inject(&mut self.harness);
        self.settle();
    }

    /// Drives and measures one interaction. `inject` is responsible for
    /// calling exactly the `RuntimeHarness` methods that constitute the
    /// interaction being measured; `expects_render` is the workload's own
    /// declaration of whether that interaction should change what is
    /// presented -- not a count inferred from how many messages were
    /// queued, which cannot be recovered from the host side now that
    /// presentation is properly dirty-driven (see [`Self::settle`]).
    ///
    /// For a burst of several events belonging to one interaction (a drag,
    /// a rapid-scroll fling), this deliberately does not require one
    /// revision per event: if the guest coalesces several inputs into fewer
    /// presented frames, that is a legitimate optimization, not a
    /// correctness failure this benchmark should punish. T5 is the instant
    /// the *last* observed revision bump completed rendering -- the
    /// visible result of the whole interaction -- not the first partial
    /// update.
    pub fn measure_one(
        &mut self,
        expects_render: bool,
        inject: impl FnOnce(&mut RuntimeHarness),
    ) -> StageTimes {
        self.settle();
        let baseline = self.harness.surface_revision(SURFACE);
        let started = Instant::now();
        inject(&mut self.harness);

        let deadline = started + PATIENCE;
        let mut idle_polls = 0u32;
        let mut last_seen_revision = baseline;
        let mut last_change_at: Option<Duration> = None;
        loop {
            let effects = self.harness.wait(SETTLE_POLL);
            for effect in &effects {
                if let instar_host::HostEffect::GuestGone { error, .. } = effect {
                    panic!("guest exited while measuring an interaction: {error:?}");
                }
            }
            let current = self.harness.surface_revision(SURFACE);
            if current != last_seen_revision {
                last_seen_revision = current;
                last_change_at = Some(started.elapsed());
                idle_polls = 0;
            } else if effects.is_empty() {
                idle_polls += 1;
            } else {
                idle_polls = 0;
            }
            if idle_polls >= SETTLE_IDLE_POLLS || Instant::now() >= deadline {
                break;
            }
        }

        assert!(
            !expects_render || last_change_at.is_some(),
            "an interaction declared visible produced no new Surface scene within \
             {PATIENCE:?} (bridge stats: {:?})",
            self.harness.bridge_stats(),
        );

        let (t5, frames_rendered) = if let Some(elapsed) = last_change_at {
            let scene = self
                .harness
                .scene()
                .expect("a scene must exist once a new revision was observed");
            let pixels = self
                .presenter
                .render(scene)
                .expect("the GATE component's own scene renders");
            self.checksum = fnv1a(self.checksum, pixels);
            (elapsed, 1)
        } else {
            (started.elapsed(), 0)
        };
        StageTimes {
            t0: Some(Duration::ZERO),
            t5: Some(t5),
            frames_rendered,
            ..Default::default()
        }
    }
}
