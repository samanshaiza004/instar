//! Pure durable-scheduler transitions.
//!
//! This lives in `youth-state` because `ScheduleRecord` and the atomic
//! persistence operations already live here. Keeping the transition function
//! in this leaf crate also guarantees that it cannot depend on the runtime's
//! threads, Tokio, winit, or Wasmtime.

use std::time::Duration;

use crate::{AppId, ElapsedReason, ScheduleRecord, ScheduleStatus};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WakeToken {
    pub app_id: AppId,
    pub schedule_id: u64,
    pub generation: u64,
}

impl WakeToken {
    #[must_use]
    pub fn new(app_id: AppId, schedule_id: u64, generation: u64) -> Self {
        Self {
            app_id,
            schedule_id,
            generation,
        }
    }

    #[must_use]
    pub fn for_record(app_id: AppId, record: &ScheduleRecord) -> Self {
        Self::new(app_id, record.id, record.generation)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerInput {
    Create {
        app_id: AppId,
        record: ScheduleRecord,
        now_epoch_millis: u64,
    },
    Pause {
        app_id: AppId,
        previous: ScheduleRecord,
        paused: ScheduleRecord,
    },
    Resume {
        app_id: AppId,
        previous: ScheduleRecord,
        resumed: ScheduleRecord,
        now_epoch_millis: u64,
    },
    Cancel {
        app_id: AppId,
        previous: ScheduleRecord,
        cancelled: ScheduleRecord,
    },
    ClockAdvanced {
        app_id: AppId,
        record: ScheduleRecord,
        now_epoch_millis: u64,
        delivery_pending: bool,
    },
    ProcessOpened {
        app_id: AppId,
        record: ScheduleRecord,
        now_epoch_millis: u64,
        delivery_pending: bool,
    },
    WakeReceived {
        token: WakeToken,
        authoritative: Option<ScheduleRecord>,
        now_epoch_millis: u64,
        delivery_pending: bool,
    },
    DeliveryCommitted {
        app_id: AppId,
        due: ScheduleRecord,
        cancelled: ScheduleRecord,
    },
    DeliveryRejected {
        app_id: AppId,
        due: ScheduleRecord,
        cancelled: ScheduleRecord,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerOutput {
    PersistMutation(ScheduleRecord),
    ArmWake {
        token: WakeToken,
        delay: Duration,
    },
    CancelWake(WakeToken),
    QueueElapsedDelivery {
        token: WakeToken,
        reason: ElapsedReason,
    },
    DiscardStaleWake(WakeToken),
}

#[must_use]
pub fn transition(input: SchedulerInput) -> Vec<SchedulerOutput> {
    match input {
        SchedulerInput::Create {
            app_id,
            record,
            now_epoch_millis,
        } => arm_or_queue(
            app_id,
            record,
            now_epoch_millis,
            false,
            ElapsedReason::Deadline,
        ),
        SchedulerInput::Pause {
            app_id,
            previous,
            paused,
        } => vec![
            SchedulerOutput::PersistMutation(paused),
            SchedulerOutput::CancelWake(WakeToken::for_record(app_id, &previous)),
        ],
        SchedulerInput::Resume {
            app_id,
            previous,
            resumed,
            now_epoch_millis,
        } => {
            let mut outputs = vec![SchedulerOutput::CancelWake(WakeToken::for_record(
                app_id.clone(),
                &previous,
            ))];
            outputs.extend(arm_or_queue(
                app_id,
                resumed,
                now_epoch_millis,
                false,
                ElapsedReason::Deadline,
            ));
            outputs
        }
        SchedulerInput::Cancel {
            app_id,
            previous,
            cancelled,
        }
        | SchedulerInput::DeliveryCommitted {
            app_id,
            due: previous,
            cancelled,
        }
        | SchedulerInput::DeliveryRejected {
            app_id,
            due: previous,
            cancelled,
        } => vec![
            SchedulerOutput::PersistMutation(cancelled),
            SchedulerOutput::CancelWake(WakeToken::for_record(app_id, &previous)),
        ],
        SchedulerInput::ClockAdvanced {
            app_id,
            record,
            now_epoch_millis,
            delivery_pending,
        } => arm_or_queue(
            app_id,
            record,
            now_epoch_millis,
            delivery_pending,
            ElapsedReason::Deadline,
        ),
        SchedulerInput::ProcessOpened {
            app_id,
            record,
            now_epoch_millis,
            delivery_pending,
        } => arm_or_queue(
            app_id,
            record,
            now_epoch_millis,
            delivery_pending,
            ElapsedReason::RecoveredOverdue,
        ),
        SchedulerInput::WakeReceived {
            token,
            authoritative,
            now_epoch_millis,
            delivery_pending,
        } => {
            let Some(record) = authoritative else {
                return vec![SchedulerOutput::DiscardStaleWake(token)];
            };
            if record.status != ScheduleStatus::Running
                || record.id != token.schedule_id
                || record.generation != token.generation
                || record
                    .deadline_millis
                    .is_none_or(|deadline| deadline > now_epoch_millis)
                || delivery_pending
            {
                vec![SchedulerOutput::DiscardStaleWake(token)]
            } else {
                due_outputs(token.app_id.clone(), record, ElapsedReason::Deadline)
            }
        }
    }
}

fn arm_or_queue(
    app_id: AppId,
    record: ScheduleRecord,
    now_epoch_millis: u64,
    delivery_pending: bool,
    reason: ElapsedReason,
) -> Vec<SchedulerOutput> {
    if record.status != ScheduleStatus::Running {
        return Vec::new();
    }
    let Some(deadline) = record.deadline_millis else {
        return Vec::new();
    };
    if deadline <= now_epoch_millis {
        if delivery_pending {
            Vec::new()
        } else {
            due_outputs(app_id, record, reason)
        }
    } else {
        vec![SchedulerOutput::ArmWake {
            token: WakeToken::for_record(app_id, &record),
            delay: Duration::from_millis(deadline - now_epoch_millis),
        }]
    }
}

fn due_outputs(
    app_id: AppId,
    mut record: ScheduleRecord,
    reason: ElapsedReason,
) -> Vec<SchedulerOutput> {
    let token = WakeToken::for_record(app_id, &record);
    record.status = ScheduleStatus::Due;
    vec![
        SchedulerOutput::PersistMutation(record),
        SchedulerOutput::CancelWake(token.clone()),
        SchedulerOutput::QueueElapsedDelivery { token, reason },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_id() -> AppId {
        AppId::parse("dev.youth.scheduler-test").unwrap()
    }

    fn token(record: &ScheduleRecord) -> WakeToken {
        WakeToken::for_record(app_id(), record)
    }

    fn running(deadline_millis: u64) -> ScheduleRecord {
        ScheduleRecord {
            id: 7,
            generation: 3,
            status: ScheduleStatus::Running,
            creation_sequence: 4,
            armed_at_millis: Some(100),
            deadline_millis: Some(deadline_millis),
            duration_millis: deadline_millis - 100,
            remaining_millis: None,
            notification: None,
            required_protocol: crate::DeliveryProtocol::V004,
        }
    }

    #[test]
    fn future_schedule_arms_exactly_one_wake() {
        let outputs = transition(SchedulerInput::Create {
            app_id: app_id(),
            record: running(500),
            now_epoch_millis: 100,
        });
        assert_eq!(
            outputs,
            vec![SchedulerOutput::ArmWake {
                token: WakeToken {
                    app_id: app_id(),
                    schedule_id: 7,
                    generation: 3,
                },
                delay: Duration::from_millis(400),
            }]
        );
    }

    #[test]
    fn early_wake_is_discarded() {
        let record = running(500);
        let token = token(&record);
        assert_eq!(
            transition(SchedulerInput::WakeReceived {
                token: token.clone(),
                authoritative: Some(record),
                now_epoch_millis: 499,
                delivery_pending: false,
            }),
            vec![SchedulerOutput::DiscardStaleWake(token)]
        );
    }

    #[test]
    fn due_wake_queues_exactly_one_delivery() {
        let record = running(500);
        let token = token(&record);
        let outputs = transition(SchedulerInput::WakeReceived {
            token,
            authoritative: Some(record),
            now_epoch_millis: 500,
            delivery_pending: false,
        });
        assert_eq!(
            outputs
                .iter()
                .filter(|output| { matches!(output, SchedulerOutput::QueueElapsedDelivery { .. }) })
                .count(),
            1
        );
    }

    #[test]
    fn pause_invalidates_the_previous_generation_and_resume_arms_a_new_one() {
        let previous = running(500);
        let mut paused = previous.clone();
        paused.generation += 1;
        paused.status = ScheduleStatus::Paused;
        paused.armed_at_millis = None;
        paused.deadline_millis = None;
        paused.remaining_millis = Some(400);
        let pause_outputs = transition(SchedulerInput::Pause {
            app_id: app_id(),
            previous: previous.clone(),
            paused: paused.clone(),
        });
        assert!(pause_outputs.contains(&SchedulerOutput::CancelWake(token(&previous))));

        let mut resumed = paused.clone();
        resumed.generation += 1;
        resumed.status = ScheduleStatus::Running;
        resumed.armed_at_millis = Some(1_000);
        resumed.deadline_millis = Some(1_400);
        resumed.remaining_millis = None;
        let resume_outputs = transition(SchedulerInput::Resume {
            app_id: app_id(),
            previous: paused,
            resumed: resumed.clone(),
            now_epoch_millis: 1_000,
        });
        assert!(resume_outputs.contains(&SchedulerOutput::ArmWake {
            token: token(&resumed),
            delay: Duration::from_millis(400),
        }));
    }

    #[test]
    fn cancelled_schedule_rejects_an_old_wake() {
        let running = running(500);
        let old_token = token(&running);
        let mut cancelled = running;
        cancelled.generation += 1;
        cancelled.status = ScheduleStatus::Cancelled;
        cancelled.armed_at_millis = None;
        cancelled.deadline_millis = None;
        assert_eq!(
            transition(SchedulerInput::WakeReceived {
                token: old_token.clone(),
                authoritative: Some(cancelled),
                now_epoch_millis: 1_000,
                delivery_pending: false,
            }),
            vec![SchedulerOutput::DiscardStaleWake(old_token)]
        );
    }
}
