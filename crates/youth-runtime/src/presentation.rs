use youth_state::{ScheduleRecord, ScheduleStatus};
use youth_tree::{CountdownFormat, ScheduleRef, TimePrecision};

const UNAVAILABLE_DISPLAY: &str = "--:--";
const DUE_DISPLAY: &str = "00:00";

/// Resolves a host-owned countdown reference for presentation.
#[must_use]
pub fn resolve_countdown_display(
    schedule: ScheduleRef,
    precision: TimePrecision,
    format: CountdownFormat,
    record: Option<&ScheduleRecord>,
    now_epoch_millis: u64,
) -> String {
    match precision {
        TimePrecision::Seconds => {}
    }
    match format {
        CountdownFormat::MinutesSeconds => {}
    }

    let Some(record) = record.filter(|record| record.generation == schedule.generation) else {
        return UNAVAILABLE_DISPLAY.to_owned();
    };
    let remaining_millis = match record.status {
        ScheduleStatus::Running => record
            .deadline_millis
            .map_or(0, |deadline| deadline.saturating_sub(now_epoch_millis)),
        ScheduleStatus::Paused => record.remaining_millis.unwrap_or(0),
        ScheduleStatus::Due => return DUE_DISPLAY.to_owned(),
        ScheduleStatus::Cancelled => return UNAVAILABLE_DISPLAY.to_owned(),
    };
    format_minutes_seconds(remaining_millis.div_ceil(1_000))
}

/// Returns the next instant at which a running schedule's rounded display changes.
#[must_use]
pub fn next_display_boundary_epoch_millis(
    record: Option<&ScheduleRecord>,
    now_epoch_millis: u64,
) -> Option<u64> {
    let record = record.filter(|record| record.status == ScheduleStatus::Running)?;
    let remaining_millis = record.deadline_millis?.saturating_sub(now_epoch_millis);
    if remaining_millis == 0 {
        return None;
    }
    let millis_until_boundary = (remaining_millis - 1) % 1_000 + 1;
    Some(now_epoch_millis.saturating_add(millis_until_boundary))
}

fn format_minutes_seconds(remaining_seconds: u64) -> String {
    format!(
        "{:02}:{:02}",
        remaining_seconds / 60,
        remaining_seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use youth_state::DeliveryProtocol;

    fn record(status: ScheduleStatus) -> ScheduleRecord {
        ScheduleRecord {
            id: 7,
            generation: 3,
            status,
            creation_sequence: 1,
            armed_at_millis: Some(0),
            deadline_millis: Some(5_000),
            duration_millis: 5_000,
            remaining_millis: None,
            notification: None,
            required_protocol: DeliveryProtocol::V004,
        }
    }

    fn display(record: Option<&ScheduleRecord>, now_epoch_millis: u64) -> String {
        resolve_countdown_display(
            ScheduleRef {
                id: 7,
                generation: 3,
            },
            TimePrecision::Seconds,
            CountdownFormat::MinutesSeconds,
            record,
            now_epoch_millis,
        )
    }

    #[test]
    fn unavailable_records_use_fixed_fallback() {
        assert_eq!(display(None, 0), "--:--");

        let mut stale = record(ScheduleStatus::Running);
        stale.generation = 4;
        assert_eq!(display(Some(&stale), 0), "--:--");

        let cancelled = record(ScheduleStatus::Cancelled);
        assert_eq!(display(Some(&cancelled), 0), "--:--");
    }

    #[test]
    fn due_is_exactly_zero() {
        assert_eq!(display(Some(&record(ScheduleStatus::Due)), 0), "00:00");
    }

    #[test]
    fn running_uses_ceiling_rounding() {
        let running = record(ScheduleStatus::Running);
        assert_eq!(display(Some(&running), 3_000), "00:02");
        assert_eq!(display(Some(&running), 3_900), "00:02");
        assert_eq!(display(Some(&running), 4_900), "00:01");
        assert_eq!(display(Some(&running), 5_000), "00:00");
    }

    #[test]
    fn running_without_deadline_is_defensively_due() {
        let mut running = record(ScheduleStatus::Running);
        running.deadline_millis = None;
        assert_eq!(display(Some(&running), 0), "00:00");
    }

    #[test]
    fn paused_uses_frozen_remaining_value() {
        let mut paused = record(ScheduleStatus::Paused);
        paused.armed_at_millis = None;
        paused.deadline_millis = None;
        paused.remaining_millis = Some(61_001);
        assert_eq!(display(Some(&paused), 999_999), "01:02");
    }

    #[test]
    fn only_running_records_arm_a_boundary() {
        assert_eq!(next_display_boundary_epoch_millis(None, 0), None);
        for status in [
            ScheduleStatus::Paused,
            ScheduleStatus::Due,
            ScheduleStatus::Cancelled,
        ] {
            assert_eq!(
                next_display_boundary_epoch_millis(Some(&record(status)), 0),
                None
            );
        }
    }

    #[test]
    fn running_boundary_is_exact_for_whole_and_partial_seconds() {
        let running = record(ScheduleStatus::Running);
        assert_eq!(
            next_display_boundary_epoch_millis(Some(&running), 0),
            Some(1_000)
        );
        assert_eq!(
            next_display_boundary_epoch_millis(Some(&running), 500),
            Some(1_000)
        );
        assert_eq!(
            next_display_boundary_epoch_millis(Some(&running), 4_999),
            Some(5_000)
        );
        assert_eq!(
            next_display_boundary_epoch_millis(Some(&running), 5_000),
            None
        );
    }
}
