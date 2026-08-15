//! One measured interaction's stage timestamps, and the validation logic
//! that catches the four required mutant/fault scenarios. Every real sample
//! the harness records is run through [`StageTimes::validate`] before it is
//! allowed into a report; a sample that fails validation is a benchmark bug,
//! not a data point.

use std::time::Duration;

/// Elapsed time since one run's local zero instant (GATE mode: the
/// harness's own `Instant::now()`; DIAGNOSTIC mode: `bench_epoch()`, the
/// same epoch the guest's `probe.mark` calls read from). Never an absolute
/// wall-clock instant -- only deltas are ever compared or reported.
pub type Elapsed = Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct StageTimes {
    /// T0: native input accepted by Instar (harness feeds the WindowEvent).
    pub t0: Option<Elapsed>,
    /// T1: guest receives the targeted Surface event. Guest-reported
    /// (DIAGNOSTIC only).
    pub t1: Option<Elapsed>,
    /// T2: guest document transaction complete. Guest-reported
    /// (DIAGNOSTIC only).
    pub t2: Option<Elapsed>,
    /// T3: last TextLayout operation for this sample complete. Host-
    /// reported (DIAGNOSTIC only).
    pub t3: Option<Elapsed>,
    /// T4: Surface scene accepted into the retained tree. Host-reported
    /// (DIAGNOSTIC only).
    pub t4: Option<Elapsed>,
    /// T5: rasterization complete -- RGBA pixels available from
    /// `Presenter::render`. Never "presented to a compositor": see
    /// benchmarks/text-latency/README.md.
    pub t5: Option<Elapsed>,
    /// Strictly incremented once per `Presenter::render` call this sample's
    /// measurement performed. Exists so "stopped at scene acceptance
    /// instead of actual rasterization" is mechanically detectable: a
    /// sample declared visible (see `expects_render`) with `t5` set but
    /// `frames_rendered == 0` is a benchmark bug.
    pub frames_rendered: u64,
    /// The workload's own declaration of whether this interaction should
    /// have changed what is presented. Not inferred from message counts or
    /// event shape: `guests/scratchpad`'s dirty-presentation optimization
    /// means the same *kind* of input (a keydown vs. a keyup) can differ in
    /// whether it renders, and nothing on the host side can recover that
    /// after the fact -- see `gate::GateRun::measure_one`'s doc comment.
    /// `frames_rendered == 0` is only a bug when this is `true`.
    pub expects_render: bool,
}

impl StageTimes {
    /// Every ordering violation this benchmark's own mutant catalog names.
    /// Returns the first violation found, as a message suitable for a test
    /// assertion or a fatal harness error -- this must never be silently
    /// tolerated in a real run.
    pub fn validate(&self) -> Result<(), String> {
        let stages: [(&str, Option<Elapsed>); 6] = [
            ("T0", self.t0),
            ("T1", self.t1),
            ("T2", self.t2),
            ("T3", self.t3),
            ("T4", self.t4),
            ("T5", self.t5),
        ];
        let mut previous: Option<(&str, Elapsed)> = None;
        for (name, value) in stages {
            let Some(value) = value else { continue };
            if let Some((prev_name, prev_value)) = previous
                && value < prev_value
            {
                return Err(format!(
                    "{name} ({value:?}) precedes {prev_name} ({prev_value:?}); a benchmark \
                     sample's stages must be non-decreasing in wall-clock order"
                ));
            }
            previous = Some((name, value));
        }
        if self.expects_render && self.t5.is_some() && self.frames_rendered == 0 {
            return Err(
                "this sample was declared to expect a render, T5 is set, but \
                 frames_rendered is 0 -- it recorded a timestamp without an actual \
                 Presenter::render call ever completing; it measured scene acceptance, \
                 not rasterization"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// T0->T5 total, the number the gate is checked against. `None` if
    /// either endpoint is missing.
    pub fn total(&self) -> Option<Elapsed> {
        Some(self.t5? - self.t0?)
    }
}

#[cfg(test)]
mod mutant_tests {
    use super::*;

    /// Mutant: the benchmark accidentally starts timing after guest receipt
    /// instead of at native input -- i.e. T0 is wrong and lands after T1.
    #[test]
    fn t0_after_t1_is_rejected() {
        let broken = StageTimes {
            t0: Some(Duration::from_millis(5)),
            t1: Some(Duration::from_millis(2)),
            t5: Some(Duration::from_millis(6)),
            frames_rendered: 1,
            expects_render: true,
            ..Default::default()
        };
        let error = broken.validate().expect_err("T0 after T1 must be rejected");
        assert!(
            error.contains('T'),
            "error should name the violating stage: {error}"
        );
    }

    /// Mutant: the benchmark stops at scene acceptance instead of actual
    /// rasterization -- T5 gets a timestamp but no frame was ever rendered,
    /// for an interaction the workload declared should be visible.
    #[test]
    fn t5_without_a_rendered_frame_is_rejected_when_a_render_was_expected() {
        let broken = StageTimes {
            t0: Some(Duration::from_millis(0)),
            t4: Some(Duration::from_millis(3)),
            t5: Some(Duration::from_millis(3)),
            frames_rendered: 0,
            expects_render: true,
            ..Default::default()
        };
        let error = broken
            .validate()
            .expect_err("T5 with zero rendered frames must be rejected when a render was expected");
        assert!(error.contains("frames_rendered"));
    }

    /// Not a mutant: a workload-declared non-visible interaction (a key
    /// release, a passive pointer move) legitimately produces `t5` with
    /// `frames_rendered == 0` under dirty presentation. The validator must
    /// accept this -- it is not the same shape as the mutant above, and the
    /// distinction is exactly `expects_render`.
    #[test]
    fn zero_frames_is_accepted_when_no_render_was_expected() {
        let quiet = StageTimes {
            t0: Some(Duration::from_millis(0)),
            t5: Some(Duration::from_millis(1)),
            frames_rendered: 0,
            expects_render: false,
            ..Default::default()
        };
        quiet
            .validate()
            .expect("a declared non-visible sample with zero frames must pass");
    }

    /// A well-formed DIAGNOSTIC sample with every stage present and
    /// non-decreasing must pass -- the validator should not be so strict it
    /// rejects real data.
    #[test]
    fn a_well_ordered_full_sample_passes() {
        let good = StageTimes {
            t0: Some(Duration::from_micros(0)),
            t1: Some(Duration::from_micros(80)),
            t2: Some(Duration::from_micros(120)),
            t3: Some(Duration::from_micros(900)),
            t4: Some(Duration::from_micros(1100)),
            t5: Some(Duration::from_micros(2400)),
            frames_rendered: 1,
            expects_render: true,
        };
        good.validate().expect("well-ordered sample must pass");
        assert_eq!(good.total(), Some(Duration::from_micros(2400)));
    }

    /// A GATE-mode sample (only T0/T5 populated) must also pass -- the
    /// validator must not require stages GATE mode never measures.
    #[test]
    fn a_gate_mode_sample_with_only_t0_and_t5_passes() {
        let gate = StageTimes {
            t0: Some(Duration::from_micros(0)),
            t5: Some(Duration::from_micros(1800)),
            frames_rendered: 1,
            expects_render: true,
            ..Default::default()
        };
        gate.validate().expect("GATE-mode sample must pass");
    }
}
