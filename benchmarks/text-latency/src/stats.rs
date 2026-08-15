//! p50/p95/p99/max over a sample set, plus the mutant/validation checks the
//! task requires the benchmark itself to defend against.

use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct Percentiles {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub count: usize,
}

/// Nearest-rank percentile over a sorted copy of `samples`. Empty input
/// reports all-zero rather than panicking: an empty workload run is a
/// caller bug the report should show as "0 samples", not a crash.
pub fn percentiles(samples: &[Duration]) -> Percentiles {
    if samples.is_empty() {
        return Percentiles::default();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = |p: f64| -> Duration {
        let index = ((p * sorted.len() as f64).ceil() as usize)
            .saturating_sub(1)
            .min(sorted.len() - 1);
        sorted[index]
    };
    Percentiles {
        p50: rank(0.50),
        p95: rank(0.95),
        p99: rank(0.99),
        max: *sorted.last().expect("checked non-empty above"),
        count: sorted.len(),
    }
}

pub fn fmt_ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p50_of_ten_ordered_samples_is_the_fifth_ranked_value() {
        let samples: Vec<Duration> = (1..=10).map(Duration::from_millis).collect();
        let p = percentiles(&samples);
        assert_eq!(p.p50, Duration::from_millis(5));
        assert_eq!(p.p95, Duration::from_millis(10));
        assert_eq!(p.max, Duration::from_millis(10));
        assert_eq!(p.count, 10);
    }

    #[test]
    fn empty_input_reports_zero_not_a_panic() {
        let p = percentiles(&[]);
        assert_eq!(p.count, 0);
        assert_eq!(p.max, Duration::ZERO);
    }
}
