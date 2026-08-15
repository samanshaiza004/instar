//! The strict-advance wait: proof that a sample observed a genuinely new
//! Surface scene, not an already-satisfied signal left over from a previous
//! interaction.

use std::time::{Duration, Instant};

/// Polls `read_revision` until it returns something strictly greater than
/// `baseline`, or `patience` elapses. Returns the new revision, or `None` on
/// timeout.
///
/// Generic over the reader so the "already-completed frame satisfies the
/// wait" mutant can be tested against a plain closure, without spinning up
/// a real guest -- the same function is what the harness calls against
/// `RuntimeHarness::surface_revision` for real measurements.
pub fn wait_for_new_revision(
    baseline: Option<u64>,
    patience: Duration,
    poll_interval: Duration,
    mut read_revision: impl FnMut() -> Option<u64>,
) -> Option<u64> {
    let started = Instant::now();
    loop {
        if let Some(revision) = read_revision()
            && baseline.is_none_or(|baseline| revision > baseline)
        {
            return Some(revision);
        }
        if started.elapsed() >= patience {
            return None;
        }
        std::thread::sleep(poll_interval);
    }
}

#[cfg(test)]
mod mutant_tests {
    use super::*;

    /// Mutant: an already-completed frame (the revision never actually
    /// advances past the pre-event baseline) satisfies the wait. This must
    /// time out, not return immediately with the stale revision.
    #[test]
    fn a_revision_equal_to_the_baseline_does_not_satisfy_the_wait() {
        let result = wait_for_new_revision(
            Some(7),
            Duration::from_millis(40),
            Duration::from_millis(5),
            || Some(7), // never advances
        );
        assert_eq!(
            result, None,
            "a revision that never exceeds the pre-event baseline must time out, \
             not be accepted as a new frame"
        );
    }

    #[test]
    fn a_revision_that_advances_after_a_few_polls_is_accepted() {
        let mut calls = 0u32;
        let result = wait_for_new_revision(
            Some(7),
            Duration::from_secs(1),
            Duration::from_millis(1),
            move || {
                calls += 1;
                if calls < 3 { Some(7) } else { Some(8) }
            },
        );
        assert_eq!(result, Some(8));
    }

    #[test]
    fn no_baseline_accepts_the_first_revision_observed() {
        let result = wait_for_new_revision(None, Duration::from_millis(40), Duration::from_millis(5), || {
            Some(1)
        });
        assert_eq!(result, Some(1));
    }
}
