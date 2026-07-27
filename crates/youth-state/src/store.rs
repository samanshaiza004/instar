use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, MAIN_DB, OptionalExtension, params};
use thiserror::Error;

use crate::{
    SchedulerInput, SchedulerOutput, StateLimits, StateLocation, StateSummary, StateValue,
    WakeToken, logical_entry_bytes, transition,
};

const SCHEMA_VERSION: u32 = 3;
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const SCHEMA: &str = r#"
CREATE TABLE youth_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT, WITHOUT ROWID;
INSERT INTO youth_meta(key, value) VALUES ('schema-version', '3');
INSERT INTO youth_meta(key, value) VALUES ('next-schedule-id', '1');

CREATE TABLE youth_state (
    key           TEXT PRIMARY KEY,
    kind          INTEGER NOT NULL,
    integer_value INTEGER,
    text_value    TEXT,
    blob_value    BLOB,
    CHECK (kind BETWEEN 0 AND 3),
    CHECK (
        (kind = 0 AND integer_value IN (0, 1) AND text_value IS NULL AND blob_value IS NULL)
        OR (kind = 1 AND integer_value IS NOT NULL AND text_value IS NULL AND blob_value IS NULL)
        OR (kind = 2 AND integer_value IS NULL AND text_value IS NOT NULL AND blob_value IS NULL)
        OR (kind = 3 AND integer_value IS NULL AND text_value IS NULL AND blob_value IS NOT NULL)
    )
) STRICT, WITHOUT ROWID;

CREATE TABLE youth_usage (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    key_count     INTEGER NOT NULL CHECK (key_count >= 0),
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0)
) STRICT, WITHOUT ROWID;
INSERT INTO youth_usage(id, key_count, logical_bytes) VALUES (1, 0, 0);

CREATE TABLE youth_schedule (
    id                 INTEGER PRIMARY KEY,
    generation         INTEGER NOT NULL CHECK (generation > 0),
    status             INTEGER NOT NULL CHECK (status BETWEEN 0 AND 3),
    creation_sequence  INTEGER NOT NULL UNIQUE CHECK (creation_sequence > 0),
    armed_at_millis    INTEGER,
    deadline_millis    INTEGER,
    duration_millis    INTEGER NOT NULL CHECK (duration_millis >= 0),
    remaining_millis   INTEGER,
    notification_title TEXT,
    notification_body  TEXT,
    CHECK (
        (status IN (0, 2) AND armed_at_millis IS NOT NULL AND deadline_millis IS NOT NULL
            AND remaining_millis IS NULL AND deadline_millis >= armed_at_millis)
        OR (status = 1 AND armed_at_millis IS NULL AND deadline_millis IS NULL
            AND remaining_millis IS NOT NULL AND remaining_millis >= 0
            AND remaining_millis <= duration_millis)
        OR (status = 3 AND armed_at_millis IS NULL AND deadline_millis IS NULL
            AND remaining_millis IS NULL)
    ),
    CHECK (
        (notification_title IS NULL AND notification_body IS NULL)
        OR (notification_title IS NOT NULL AND notification_body IS NOT NULL)
    )
) STRICT, WITHOUT ROWID;

CREATE TABLE youth_pending_delivery (
    schedule_id      INTEGER NOT NULL,
    generation       INTEGER NOT NULL CHECK (generation > 0),
    deadline_millis  INTEGER NOT NULL CHECK (deadline_millis >= 0),
    creation_sequence INTEGER NOT NULL CHECK (creation_sequence > 0),
    PRIMARY KEY (schedule_id, generation),
    FOREIGN KEY (schedule_id) REFERENCES youth_schedule(id)
) STRICT, WITHOUT ROWID;
"#;

const MIGRATE_V1_TO_V2: &str = r#"
BEGIN IMMEDIATE;
CREATE TABLE youth_schedule (
    id                 INTEGER PRIMARY KEY,
    generation         INTEGER NOT NULL CHECK (generation > 0),
    status             INTEGER NOT NULL CHECK (status IN (0, 1)),
    armed_at_millis    INTEGER,
    deadline_millis    INTEGER,
    duration_millis    INTEGER NOT NULL CHECK (duration_millis >= 0),
    remaining_millis   INTEGER,
    notification_title TEXT,
    notification_body  TEXT,
    CHECK (
        (status = 0 AND armed_at_millis IS NOT NULL AND deadline_millis IS NOT NULL
            AND remaining_millis IS NULL AND deadline_millis >= armed_at_millis)
        OR (status = 1 AND armed_at_millis IS NULL AND deadline_millis IS NULL
            AND remaining_millis IS NOT NULL AND remaining_millis >= 0
            AND remaining_millis <= duration_millis)
    ),
    CHECK (
        (notification_title IS NULL AND notification_body IS NULL)
        OR (notification_title IS NOT NULL AND notification_body IS NOT NULL)
    )
) STRICT, WITHOUT ROWID;
INSERT INTO youth_meta(key, value) VALUES ('next-schedule-id', '1');
UPDATE youth_meta SET value = '2' WHERE key = 'schema-version';
COMMIT;
"#;

const MIGRATE_V2_TO_V3: &str = r#"
BEGIN IMMEDIATE;
ALTER TABLE youth_schedule RENAME TO youth_schedule_v2;
CREATE TABLE youth_schedule (
    id                 INTEGER PRIMARY KEY,
    generation         INTEGER NOT NULL CHECK (generation > 0),
    status             INTEGER NOT NULL CHECK (status BETWEEN 0 AND 3),
    creation_sequence  INTEGER NOT NULL UNIQUE CHECK (creation_sequence > 0),
    armed_at_millis    INTEGER,
    deadline_millis    INTEGER,
    duration_millis    INTEGER NOT NULL CHECK (duration_millis >= 0),
    remaining_millis   INTEGER,
    notification_title TEXT,
    notification_body  TEXT,
    CHECK (
        (status IN (0, 2) AND armed_at_millis IS NOT NULL AND deadline_millis IS NOT NULL
            AND remaining_millis IS NULL AND deadline_millis >= armed_at_millis)
        OR (status = 1 AND armed_at_millis IS NULL AND deadline_millis IS NULL
            AND remaining_millis IS NOT NULL AND remaining_millis >= 0
            AND remaining_millis <= duration_millis)
        OR (status = 3 AND armed_at_millis IS NULL AND deadline_millis IS NULL
            AND remaining_millis IS NULL)
    ),
    CHECK (
        (notification_title IS NULL AND notification_body IS NULL)
        OR (notification_title IS NOT NULL AND notification_body IS NOT NULL)
    )
) STRICT, WITHOUT ROWID;
INSERT INTO youth_schedule(
    id, generation, status, creation_sequence, armed_at_millis, deadline_millis,
    duration_millis, remaining_millis, notification_title, notification_body
)
SELECT id, generation, status, id, armed_at_millis, deadline_millis,
       duration_millis, remaining_millis, notification_title, notification_body
FROM youth_schedule_v2;
DROP TABLE youth_schedule_v2;
CREATE TABLE youth_pending_delivery (
    schedule_id       INTEGER NOT NULL,
    generation        INTEGER NOT NULL CHECK (generation > 0),
    deadline_millis   INTEGER NOT NULL CHECK (deadline_millis >= 0),
    creation_sequence INTEGER NOT NULL CHECK (creation_sequence > 0),
    PRIMARY KEY (schedule_id, generation),
    FOREIGN KEY (schedule_id) REFERENCES youth_schedule(id)
) STRICT, WITHOUT ROWID;
UPDATE youth_meta SET value = '3' WHERE key = 'schema-version';
COMMIT;
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestCallPhase {
    Idle,
    Mount,
    Handle,
    Resync,
}

impl GuestCallPhase {
    fn writable(self) -> bool {
        matches!(self, Self::Mount | Self::Handle)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub key_count: u32,
    pub logical_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TurnStateMetrics {
    pub calls: u32,
    pub writes: u32,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub committed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Verification {
    pub integrity_ok: bool,
    pub schema_version: u32,
    pub stored: Usage,
    pub computed: Usage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleStatus {
    Running,
    Paused,
    Due,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleRecord {
    pub id: u64,
    pub generation: u64,
    pub status: ScheduleStatus,
    pub creation_sequence: u64,
    pub armed_at_millis: Option<u64>,
    pub deadline_millis: Option<u64>,
    pub duration_millis: u64,
    pub remaining_millis: Option<u64>,
    pub notification: Option<(String, String)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingDelivery {
    pub schedule_id: u64,
    pub generation: u64,
    pub deadline_millis: u64,
    pub creation_sequence: u64,
}

impl Verification {
    #[must_use]
    pub fn usage_matches(&self) -> bool {
        self.stored == self.computed
    }
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("state database is unavailable")]
    Database(#[source] rusqlite::Error),
    #[error("state filesystem is unavailable")]
    Filesystem(#[source] std::io::Error),
    #[error("state database schema or contents are invalid: {0}")]
    Corrupt(&'static str),
    #[error("stored state usage does not match the state table")]
    UsageMismatch,
    #[error("state key is invalid")]
    InvalidKey,
    #[error("state value is invalid")]
    InvalidValue,
    #[error("state is read-only during this lifecycle phase")]
    ReadOnly,
    #[error("state is unavailable outside a controlled lifecycle call")]
    Idle,
    #[error("state quota exceeded")]
    QuotaExceeded,
    #[error("schedule duration is invalid")]
    InvalidScheduleDuration,
    #[error("schedule notification is invalid")]
    InvalidScheduleNotification,
    #[error("too many active schedules")]
    TooManySchedules,
    #[error("schedule does not exist")]
    UnknownSchedule,
    #[error("schedule generation is stale")]
    StaleScheduleGeneration,
    #[error("schedule is not in the required state")]
    InvalidScheduleState,
    #[error("a state transaction is already active")]
    TransactionActive,
    #[error("no state transaction is active")]
    NoTransaction,
    #[error("backup destination already exists")]
    BackupExists,
    #[error("state commit failed at an injected test failpoint")]
    InjectedCommitFailure,
}

impl StateError {
    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Database(rusqlite::Error::SqliteFailure(error, _))
                if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        )
    }
}

impl From<rusqlite::Error> for StateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub struct StateStore {
    connection: Connection,
    location: StateLocation,
    limits: StateLimits,
    phase: GuestCallPhase,
    transaction_active: bool,
    metrics: TurnStateMetrics,
    #[cfg(feature = "test-support")]
    fail_next_commit: bool,
}

impl std::fmt::Debug for StateStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StateStore")
            .field("location", &self.location)
            .field("phase", &self.phase)
            .field("transaction_active", &self.transaction_active)
            .finish_non_exhaustive()
    }
}

impl StateStore {
    pub fn open(location: StateLocation, limits: StateLimits) -> Result<Self, StateError> {
        let location_kind = match &location {
            StateLocation::Memory => "memory",
            StateLocation::File(_) => "file",
        };
        let _span = tracing::info_span!("state.open", location = location_kind).entered();
        let (connection, initialize) = match &location {
            StateLocation::Memory => (Connection::open_in_memory()?, true),
            StateLocation::File(path) => {
                let initialize = !path.exists();
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(StateError::Filesystem)?;
                }
                (Connection::open(path)?, initialize)
            }
        };
        configure(&connection)?;
        if initialize || is_empty(&connection)? {
            connection.execute_batch(SCHEMA)?;
        } else {
            migrate_if_needed(&connection)?;
            require_valid(&verify_connection(&connection)?)?;
        }
        Ok(Self {
            connection,
            location,
            limits,
            phase: GuestCallPhase::Idle,
            transaction_active: false,
            metrics: TurnStateMetrics::default(),
            #[cfg(feature = "test-support")]
            fail_next_commit: false,
        })
    }

    #[must_use]
    pub fn location(&self) -> &StateLocation {
        &self.location
    }

    #[must_use]
    pub const fn phase(&self) -> GuestCallPhase {
        self.phase
    }

    #[must_use]
    pub const fn transaction_active(&self) -> bool {
        self.transaction_active
    }

    #[must_use]
    pub const fn metrics(&self) -> TurnStateMetrics {
        self.metrics
    }

    pub fn begin(&mut self, phase: GuestCallPhase) -> Result<(), StateError> {
        let _span = tracing::info_span!("state.begin", phase = ?phase).entered();
        if self.transaction_active {
            return Err(StateError::TransactionActive);
        }
        if phase == GuestCallPhase::Idle {
            return Err(StateError::Idle);
        }
        let usage = read_usage(&self.connection)?;
        let begin = if phase == GuestCallPhase::Resync {
            "BEGIN"
        } else {
            "BEGIN IMMEDIATE"
        };
        self.connection.execute_batch(begin)?;
        self.phase = phase;
        self.transaction_active = true;
        self.metrics = TurnStateMetrics {
            bytes_before: usage.logical_bytes,
            bytes_after: usage.logical_bytes,
            ..TurnStateMetrics::default()
        };
        Ok(())
    }

    pub fn commit(&mut self) -> Result<TurnStateMetrics, StateError> {
        let _span = tracing::info_span!("state.commit").entered();
        self.require_transaction()?;
        if self.phase == GuestCallPhase::Resync {
            return Err(StateError::ReadOnly);
        }
        #[cfg(feature = "test-support")]
        if std::mem::take(&mut self.fail_next_commit) {
            let _ = self.connection.execute_batch("ROLLBACK");
            self.finish(false);
            return Err(StateError::InjectedCommitFailure);
        }
        if let Err(error) = self.connection.execute_batch("COMMIT") {
            let _ = self.connection.execute_batch("ROLLBACK");
            self.finish(false);
            return Err(error.into());
        }
        self.finish(true);
        Ok(self.metrics)
    }

    pub fn rollback(&mut self) -> Result<TurnStateMetrics, StateError> {
        let _span = tracing::info_span!("state.rollback").entered();
        self.require_transaction()?;
        self.connection.execute_batch("ROLLBACK")?;
        self.finish(false);
        Ok(self.metrics)
    }

    pub fn get(&mut self, key: &str) -> Result<Option<StateValue>, StateError> {
        let _span = tracing::info_span!("state.get").entered();
        self.attempt_call()?;
        self.require_transaction()?;
        self.validate_key(key)?;
        read_value(&self.connection, key)
    }

    pub fn set(&mut self, key: &str, value: StateValue) -> Result<(), StateError> {
        let _span = tracing::info_span!("state.set").entered();
        self.attempt_call()?;
        self.require_writable()?;
        self.validate_key(key)?;
        self.validate_value(&value)?;
        self.attempt_write()?;

        let old = read_value(&self.connection, key)?;
        let usage = read_usage(&self.connection)?;
        let old_bytes = old
            .as_ref()
            .map(|old| logical_entry_bytes(key, old))
            .transpose()
            .map_err(|_| StateError::QuotaExceeded)?
            .unwrap_or(0);
        let new_bytes = logical_entry_bytes(key, &value).map_err(|_| StateError::QuotaExceeded)?;
        let logical_bytes = usage
            .logical_bytes
            .checked_sub(old_bytes)
            .and_then(|bytes| bytes.checked_add(new_bytes))
            .ok_or(StateError::QuotaExceeded)?;
        let key_count = if old.is_some() {
            usage.key_count
        } else {
            usage
                .key_count
                .checked_add(1)
                .ok_or(StateError::QuotaExceeded)?
        };
        if logical_bytes > self.limits.max_total_bytes || key_count > self.limits.max_keys {
            return Err(StateError::QuotaExceeded);
        }

        let (kind, integer, text, bytes): (i64, Option<i64>, Option<&str>, Option<&[u8]>) =
            match &value {
                StateValue::Boolean(value) => (0, Some(i64::from(*value)), None, None),
                StateValue::Integer(value) => (1, Some(*value), None, None),
                StateValue::Text(value) => (2, None, Some(value.as_str()), None),
                StateValue::Bytes(value) => (3, None, None, Some(value.as_slice())),
            };
        self.connection.execute(
            "INSERT INTO youth_state(key, kind, integer_value, text_value, blob_value)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(key) DO UPDATE SET kind=excluded.kind,
               integer_value=excluded.integer_value, text_value=excluded.text_value,
               blob_value=excluded.blob_value",
            params![key, kind, integer, text, bytes],
        )?;
        write_usage(
            &self.connection,
            Usage {
                key_count,
                logical_bytes,
            },
        )?;
        self.metrics.bytes_after = logical_bytes;
        Ok(())
    }

    pub fn delete(&mut self, key: &str) -> Result<bool, StateError> {
        let _span = tracing::info_span!("state.delete").entered();
        self.attempt_call()?;
        self.require_writable()?;
        self.validate_key(key)?;
        let Some(old) = read_value(&self.connection, key)? else {
            return Ok(false);
        };
        self.attempt_write()?;
        let usage = read_usage(&self.connection)?;
        let removed = logical_entry_bytes(key, &old).map_err(|_| StateError::QuotaExceeded)?;
        let next = Usage {
            key_count: usage
                .key_count
                .checked_sub(1)
                .ok_or(StateError::Corrupt("usage key count underflow"))?,
            logical_bytes: usage
                .logical_bytes
                .checked_sub(removed)
                .ok_or(StateError::Corrupt("usage byte count underflow"))?,
        };
        self.connection
            .execute("DELETE FROM youth_state WHERE key = ?1", [key])?;
        write_usage(&self.connection, next)?;
        self.metrics.bytes_after = next.logical_bytes;
        Ok(true)
    }

    pub fn schedule_create(
        &mut self,
        now_epoch_millis: u64,
        duration_millis: u64,
        notification: Option<(String, String)>,
    ) -> Result<ScheduleRecord, StateError> {
        let _span = tracing::info_span!("state.schedule_create").entered();
        self.attempt_call()?;
        self.require_writable()?;
        if duration_millis < self.limits.min_schedule_millis
            || duration_millis > self.limits.max_schedule_millis
        {
            return Err(StateError::InvalidScheduleDuration);
        }
        if notification.as_ref().is_some_and(|(title, body)| {
            title.len() > self.limits.max_notification_title_bytes
                || body.len() > self.limits.max_notification_body_bytes
        }) {
            return Err(StateError::InvalidScheduleNotification);
        }
        self.attempt_write()?;
        let active: i64 = self.connection.query_row(
            "SELECT count(*) FROM youth_schedule WHERE status != 3",
            [],
            |row| row.get(0),
        )?;
        let active = usize::try_from(active)
            .map_err(|_| StateError::Corrupt("invalid active schedule count"))?;
        if active >= self.limits.max_active_schedules {
            return Err(StateError::TooManySchedules);
        }

        let id = read_next_schedule_id(&self.connection)?;
        let next_id = id
            .checked_add(1)
            .ok_or(StateError::Corrupt("schedule ID overflow"))?;
        write_next_schedule_id(&self.connection, next_id)?;
        let deadline_millis = now_epoch_millis
            .checked_add(duration_millis)
            .ok_or(StateError::InvalidScheduleDuration)?;
        let id_sql = to_sql_u64(id, "invalid schedule ID")?;
        let now_sql = to_sql_u64(now_epoch_millis, "invalid schedule time")?;
        let deadline_sql = to_sql_u64(deadline_millis, "invalid schedule deadline")?;
        let duration_sql = to_sql_u64(duration_millis, "invalid schedule duration")?;
        let (title, body) = notification.as_ref().map_or((None, None), |(title, body)| {
            (Some(title.as_str()), Some(body.as_str()))
        });
        self.connection.execute(
            "INSERT INTO youth_schedule(
                id, generation, status, creation_sequence, armed_at_millis, deadline_millis, duration_millis,
                remaining_millis, notification_title, notification_body
             ) VALUES (?1, 1, 0, ?1, ?2, ?3, ?4, NULL, ?5, ?6)",
            params![id_sql, now_sql, deadline_sql, duration_sql, title, body],
        )?;
        read_schedule(&self.connection, id)?
            .ok_or(StateError::Corrupt("newly created schedule is missing"))
    }

    pub fn schedule_pause(
        &mut self,
        now_epoch_millis: u64,
        id: u64,
        generation: u64,
    ) -> Result<ScheduleRecord, StateError> {
        let _span = tracing::info_span!("state.schedule_pause").entered();
        self.attempt_call()?;
        self.require_writable()?;
        let current = require_schedule(&self.connection, id, generation)?;
        if current.status != ScheduleStatus::Running {
            return Err(StateError::InvalidScheduleState);
        }
        self.attempt_write()?;
        let deadline = current
            .deadline_millis
            .ok_or(StateError::Corrupt("armed schedule has no deadline"))?;
        let remaining = deadline.saturating_sub(now_epoch_millis);
        let next_generation = generation
            .checked_add(1)
            .ok_or(StateError::Corrupt("schedule generation overflow"))?;
        self.connection.execute(
            "UPDATE youth_schedule
             SET generation = ?1, status = 1, armed_at_millis = NULL, deadline_millis = NULL,
                 remaining_millis = ?2
             WHERE id = ?3",
            params![
                to_sql_u64(next_generation, "invalid schedule generation")?,
                to_sql_u64(remaining, "invalid schedule remainder")?,
                to_sql_u64(id, "invalid schedule ID")?
            ],
        )?;
        read_schedule(&self.connection, id)?
            .ok_or(StateError::Corrupt("paused schedule is missing"))
    }

    pub fn schedule_resume(
        &mut self,
        now_epoch_millis: u64,
        id: u64,
        generation: u64,
    ) -> Result<ScheduleRecord, StateError> {
        let _span = tracing::info_span!("state.schedule_resume").entered();
        self.attempt_call()?;
        self.require_writable()?;
        let current = require_schedule(&self.connection, id, generation)?;
        if current.status != ScheduleStatus::Paused {
            return Err(StateError::InvalidScheduleState);
        }
        self.attempt_write()?;
        let remaining = current
            .remaining_millis
            .ok_or(StateError::Corrupt("paused schedule has no remainder"))?;
        let deadline = now_epoch_millis
            .checked_add(remaining)
            .ok_or(StateError::InvalidScheduleDuration)?;
        let next_generation = generation
            .checked_add(1)
            .ok_or(StateError::Corrupt("schedule generation overflow"))?;
        self.connection.execute(
            "UPDATE youth_schedule
             SET generation = ?1, status = 0, armed_at_millis = ?2, deadline_millis = ?3,
                 remaining_millis = NULL
             WHERE id = ?4",
            params![
                to_sql_u64(next_generation, "invalid schedule generation")?,
                to_sql_u64(now_epoch_millis, "invalid schedule time")?,
                to_sql_u64(deadline, "invalid schedule deadline")?,
                to_sql_u64(id, "invalid schedule ID")?
            ],
        )?;
        read_schedule(&self.connection, id)?
            .ok_or(StateError::Corrupt("resumed schedule is missing"))
    }

    pub fn schedule_cancel(&mut self, id: u64, generation: u64) -> Result<(), StateError> {
        let _span = tracing::info_span!("state.schedule_cancel").entered();
        self.attempt_call()?;
        self.require_writable()?;
        let current = require_schedule(&self.connection, id, generation)?;
        self.attempt_write()?;
        if current.status == ScheduleStatus::Cancelled {
            return Err(StateError::InvalidScheduleState);
        }
        let next_generation = generation
            .checked_add(1)
            .ok_or(StateError::Corrupt("schedule generation overflow"))?;
        self.connection.execute(
            "DELETE FROM youth_pending_delivery WHERE schedule_id = ?1",
            [to_sql_u64(id, "invalid schedule ID")?],
        )?;
        self.connection.execute(
            "UPDATE youth_schedule
             SET generation = ?1, status = 3, armed_at_millis = NULL, deadline_millis = NULL,
                 remaining_millis = NULL
             WHERE id = ?2",
            params![
                to_sql_u64(next_generation, "invalid schedule generation")?,
                to_sql_u64(id, "invalid schedule ID")?
            ],
        )?;
        Ok(())
    }

    pub fn schedules(&self) -> Result<Vec<ScheduleRecord>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT id, generation, status, creation_sequence, armed_at_millis, deadline_millis, duration_millis,
                    remaining_millis, notification_title, notification_body
             FROM youth_schedule ORDER BY id",
        )?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(decode_schedule(row)?);
        }
        Ok(records)
    }

    pub fn schedule(&self, id: u64) -> Result<Option<ScheduleRecord>, StateError> {
        read_schedule(&self.connection, id)
    }

    pub fn pending_deliveries(&self) -> Result<Vec<PendingDelivery>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT schedule_id, generation, deadline_millis, creation_sequence
             FROM youth_pending_delivery
             ORDER BY deadline_millis, creation_sequence",
        )?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(PendingDelivery {
                schedule_id: from_sql_u64(row.get(0)?, "invalid pending schedule ID")?,
                generation: from_sql_u64(row.get(1)?, "invalid pending generation")?,
                deadline_millis: from_sql_u64(row.get(2)?, "invalid pending deadline")?,
                creation_sequence: from_sql_u64(row.get(3)?, "invalid pending creation sequence")?,
            });
        }
        Ok(records)
    }

    pub fn reconcile_overdue(
        &mut self,
        now_epoch_millis: u64,
    ) -> Result<Vec<SchedulerOutput>, StateError> {
        self.require_scheduler_idle()?;
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = self.reconcile_overdue_inner(now_epoch_millis);
        finish_scheduler_transaction(&self.connection, result)
    }

    pub fn receive_wake(
        &mut self,
        token: WakeToken,
        now_epoch_millis: u64,
    ) -> Result<Vec<SchedulerOutput>, StateError> {
        self.require_scheduler_idle()?;
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let authoritative = read_schedule(&self.connection, token.schedule_id)?;
            let delivery_pending = pending_delivery_exists(&self.connection, token)?;
            let outputs = transition(SchedulerInput::WakeReceived {
                token,
                authoritative,
                now_epoch_millis,
                delivery_pending,
            });
            apply_scheduler_outputs(&self.connection, &outputs)?;
            Ok(outputs)
        })();
        finish_scheduler_transaction(&self.connection, result)
    }

    fn reconcile_overdue_inner(
        &self,
        now_epoch_millis: u64,
    ) -> Result<Vec<SchedulerOutput>, StateError> {
        let mut statement = self.connection.prepare(
            "SELECT id, generation, status, creation_sequence, armed_at_millis, deadline_millis,
                    duration_millis, remaining_millis, notification_title, notification_body
             FROM youth_schedule
             WHERE status = 0
             ORDER BY deadline_millis, creation_sequence",
        )?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push(decode_schedule(row)?);
        }
        let mut all_outputs = Vec::new();
        for record in records {
            let token = WakeToken::from(&record);
            let outputs = transition(SchedulerInput::ProcessOpened {
                record,
                now_epoch_millis,
                delivery_pending: pending_delivery_exists(&self.connection, token)?,
            });
            apply_scheduler_outputs(&self.connection, &outputs)?;
            all_outputs.extend(outputs);
        }
        Ok(all_outputs)
    }

    fn require_scheduler_idle(&self) -> Result<(), StateError> {
        if self.transaction_active {
            Err(StateError::TransactionActive)
        } else {
            Ok(())
        }
    }

    pub fn summary(&self) -> Result<StateSummary, StateError> {
        let usage = read_usage(&self.connection)?;
        Ok(StateSummary {
            schema_version: SCHEMA_VERSION,
            key_count: usage.key_count,
            logical_bytes: usage.logical_bytes,
        })
    }

    pub fn reset(&mut self) -> Result<(), StateError> {
        if self.transaction_active {
            return Err(StateError::TransactionActive);
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            self.connection.execute("DELETE FROM youth_state", [])?;
            self.connection
                .execute("DELETE FROM youth_pending_delivery", [])?;
            self.connection.execute("DELETE FROM youth_schedule", [])?;
            write_usage(&self.connection, Usage::default())?;
            self.connection.execute_batch("COMMIT")?;
            Ok::<_, StateError>(())
        })();
        if result.is_err() {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
        result
    }

    #[cfg(feature = "test-support")]
    pub fn fail_next_commit(&mut self) {
        self.fail_next_commit = true;
    }

    fn attempt_call(&mut self) -> Result<(), StateError> {
        self.metrics.calls = self
            .metrics
            .calls
            .checked_add(1)
            .ok_or(StateError::QuotaExceeded)?;
        if self.metrics.calls > self.limits.max_calls_per_turn {
            return Err(StateError::QuotaExceeded);
        }
        Ok(())
    }

    fn attempt_write(&mut self) -> Result<(), StateError> {
        self.metrics.writes = self
            .metrics
            .writes
            .checked_add(1)
            .ok_or(StateError::QuotaExceeded)?;
        if self.metrics.writes > self.limits.max_writes_per_turn {
            return Err(StateError::QuotaExceeded);
        }
        Ok(())
    }

    fn require_transaction(&self) -> Result<(), StateError> {
        if !self.transaction_active || self.phase == GuestCallPhase::Idle {
            Err(StateError::Idle)
        } else {
            Ok(())
        }
    }

    fn require_writable(&self) -> Result<(), StateError> {
        self.require_transaction()?;
        if self.phase.writable() {
            Ok(())
        } else {
            Err(StateError::ReadOnly)
        }
    }

    fn validate_key(&self, key: &str) -> Result<(), StateError> {
        if key.is_empty() || key.len() > self.limits.max_key_bytes {
            Err(StateError::InvalidKey)
        } else {
            Ok(())
        }
    }

    fn validate_value(&self, value: &StateValue) -> Result<(), StateError> {
        match value {
            StateValue::Text(value) if value.len() > self.limits.max_text_bytes => {
                Err(StateError::InvalidValue)
            }
            StateValue::Bytes(value) if value.len() > self.limits.max_bytes_bytes => {
                Err(StateError::InvalidValue)
            }
            _ => Ok(()),
        }
    }

    fn finish(&mut self, committed: bool) {
        self.metrics.committed = committed;
        if !committed {
            self.metrics.bytes_after = self.metrics.bytes_before;
        }
        self.phase = GuestCallPhase::Idle;
        self.transaction_active = false;
    }
}

pub fn verify_file(path: &Path) -> Result<Verification, StateError> {
    let connection = Connection::open(path)?;
    configure(&connection)?;
    verify_connection(&connection)
}

pub fn repair_usage(path: &Path, backup_path: &Path) -> Result<Verification, StateError> {
    if backup_path.exists() {
        return Err(StateError::BackupExists);
    }
    let connection = Connection::open(path)?;
    configure(&connection)?;
    let before = verify_connection(&connection)?;
    require_repairable(&before)?;
    connection.backup(MAIN_DB, backup_path, None)?;
    connection.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| {
        let current = verify_connection(&connection)?;
        require_repairable(&current)?;
        write_usage(&connection, current.computed)?;
        connection.execute_batch("COMMIT")?;
        Ok::<_, StateError>(())
    })();
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK");
        return result.map(|()| unreachable!());
    }
    verify_connection(&connection)
}

fn configure(connection: &Connection) -> Result<(), StateError> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "trusted_schema", "OFF")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn is_empty(connection: &Connection) -> Result<bool, StateError> {
    let tables: u32 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name LIKE 'youth_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(tables == 0)
}

fn migrate_if_needed(connection: &Connection) -> Result<(), StateError> {
    loop {
        let verification = verify_connection(connection)?;
        match verification.schema_version {
            SCHEMA_VERSION => return Ok(()),
            1 => {
                require_integrity_and_usage(&verification)?;
                if let Err(error) = connection.execute_batch(MIGRATE_V1_TO_V2) {
                    let _ = connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
            }
            2 => {
                require_integrity_and_usage(&verification)?;
                if let Err(error) = connection.execute_batch(MIGRATE_V2_TO_V3) {
                    let _ = connection.execute_batch("ROLLBACK");
                    return Err(error.into());
                }
            }
            _ => return Err(StateError::Corrupt("unsupported schema version")),
        }
    }
}

fn verify_connection(connection: &Connection) -> Result<Verification, StateError> {
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    let integrity_ok = integrity == "ok";
    let version: String = connection
        .query_row(
            "SELECT value FROM youth_meta WHERE key = 'schema-version'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(StateError::Corrupt("missing schema version"))?;
    let schema_version = version
        .parse::<u32>()
        .map_err(|_| StateError::Corrupt("invalid schema version"))?;
    verify_schema_shape(connection, schema_version)?;
    let stored = read_usage(connection)?;
    let computed = compute_usage(connection)?;
    Ok(Verification {
        integrity_ok,
        schema_version,
        stored,
        computed,
    })
}

fn require_valid(verification: &Verification) -> Result<(), StateError> {
    require_repairable(verification)?;
    require_integrity_and_usage(verification)
}

fn require_repairable(verification: &Verification) -> Result<(), StateError> {
    if !verification.integrity_ok {
        return Err(StateError::Corrupt("SQLite integrity check failed"));
    }
    if verification.schema_version != SCHEMA_VERSION {
        return Err(StateError::Corrupt("unsupported schema version"));
    }
    Ok(())
}

fn require_integrity_and_usage(verification: &Verification) -> Result<(), StateError> {
    if !verification.integrity_ok {
        return Err(StateError::Corrupt("SQLite integrity check failed"));
    }
    if !verification.usage_matches() {
        return Err(StateError::UsageMismatch);
    }
    Ok(())
}

fn verify_schema_shape(connection: &Connection, schema_version: u32) -> Result<(), StateError> {
    let tables: &[&str] = match schema_version {
        1 => &["youth_meta", "youth_state", "youth_usage"],
        2 => &["youth_meta", "youth_state", "youth_usage", "youth_schedule"],
        3 => &[
            "youth_meta",
            "youth_state",
            "youth_usage",
            "youth_schedule",
            "youth_pending_delivery",
        ],
        _ => return Err(StateError::Corrupt("unsupported schema version")),
    };
    for table in tables {
        let sql: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        let Some(sql) = sql else {
            return Err(StateError::Corrupt("required table is missing"));
        };
        let uppercase = sql.to_ascii_uppercase();
        if !uppercase.contains("STRICT") || !uppercase.contains("WITHOUT ROWID") {
            return Err(StateError::Corrupt(
                "state tables must be strict without rowids",
            ));
        }
    }
    Ok(())
}

fn read_next_schedule_id(connection: &Connection) -> Result<u64, StateError> {
    let value: String = connection
        .query_row(
            "SELECT value FROM youth_meta WHERE key = 'next-schedule-id'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(StateError::Corrupt("missing next schedule ID"))?;
    value
        .parse()
        .map_err(|_| StateError::Corrupt("invalid next schedule ID"))
}

fn write_next_schedule_id(connection: &Connection, id: u64) -> Result<(), StateError> {
    let changed = connection.execute(
        "UPDATE youth_meta SET value = ?1 WHERE key = 'next-schedule-id'",
        [id.to_string()],
    )?;
    if changed != 1 {
        return Err(StateError::Corrupt("missing next schedule ID"));
    }
    Ok(())
}

fn read_schedule(connection: &Connection, id: u64) -> Result<Option<ScheduleRecord>, StateError> {
    let mut statement = connection.prepare(
        "SELECT id, generation, status, creation_sequence, armed_at_millis, deadline_millis, duration_millis,
                remaining_millis, notification_title, notification_body
         FROM youth_schedule WHERE id = ?1",
    )?;
    let mut rows = statement.query([to_sql_u64(id, "invalid schedule ID")?])?;
    rows.next()?.map(decode_schedule).transpose()
}

fn require_schedule(
    connection: &Connection,
    id: u64,
    generation: u64,
) -> Result<ScheduleRecord, StateError> {
    let record = read_schedule(connection, id)?.ok_or(StateError::UnknownSchedule)?;
    if record.generation != generation {
        return Err(StateError::StaleScheduleGeneration);
    }
    Ok(record)
}

fn decode_schedule(row: &rusqlite::Row<'_>) -> Result<ScheduleRecord, StateError> {
    let id = from_sql_u64(row.get(0)?, "invalid schedule ID")?;
    let generation = from_sql_u64(row.get(1)?, "invalid schedule generation")?;
    let status = match row.get::<_, i64>(2)? {
        0 => ScheduleStatus::Running,
        1 => ScheduleStatus::Paused,
        2 => ScheduleStatus::Due,
        3 => ScheduleStatus::Cancelled,
        _ => return Err(StateError::Corrupt("invalid schedule status")),
    };
    let creation_sequence = from_sql_u64(row.get(3)?, "invalid schedule creation sequence")?;
    let armed_at_millis = row
        .get::<_, Option<i64>>(4)?
        .map(|value| from_sql_u64(value, "invalid schedule time"))
        .transpose()?;
    let deadline_millis = row
        .get::<_, Option<i64>>(5)?
        .map(|value| from_sql_u64(value, "invalid schedule deadline"))
        .transpose()?;
    let duration_millis = from_sql_u64(row.get(6)?, "invalid schedule duration")?;
    let remaining_millis = row
        .get::<_, Option<i64>>(7)?
        .map(|value| from_sql_u64(value, "invalid schedule remainder"))
        .transpose()?;
    let title: Option<String> = row.get(8)?;
    let body: Option<String> = row.get(9)?;
    let notification = match (title, body) {
        (None, None) => None,
        (Some(title), Some(body)) => Some((title, body)),
        _ => return Err(StateError::Corrupt("invalid schedule notification")),
    };
    Ok(ScheduleRecord {
        id,
        generation,
        status,
        creation_sequence,
        armed_at_millis,
        deadline_millis,
        duration_millis,
        remaining_millis,
        notification,
    })
}

fn pending_delivery_exists(connection: &Connection, token: WakeToken) -> Result<bool, StateError> {
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM youth_pending_delivery
         WHERE schedule_id = ?1 AND generation = ?2",
        params![
            to_sql_u64(token.schedule_id, "invalid schedule ID")?,
            to_sql_u64(token.generation, "invalid schedule generation")?
        ],
        |row| row.get(0),
    )?;
    Ok(count != 0)
}

fn apply_scheduler_outputs(
    connection: &Connection,
    outputs: &[SchedulerOutput],
) -> Result<(), StateError> {
    for output in outputs {
        match output {
            SchedulerOutput::PersistMutation(record) => {
                connection.execute(
                    "UPDATE youth_schedule
                     SET generation = ?1, status = ?2, creation_sequence = ?3,
                         armed_at_millis = ?4, deadline_millis = ?5, duration_millis = ?6,
                         remaining_millis = ?7, notification_title = ?8,
                         notification_body = ?9
                     WHERE id = ?10",
                    params![
                        to_sql_u64(record.generation, "invalid schedule generation")?,
                        schedule_status_sql(record.status),
                        to_sql_u64(record.creation_sequence, "invalid creation sequence")?,
                        record
                            .armed_at_millis
                            .map(|value| to_sql_u64(value, "invalid schedule time"))
                            .transpose()?,
                        record
                            .deadline_millis
                            .map(|value| to_sql_u64(value, "invalid schedule deadline"))
                            .transpose()?,
                        to_sql_u64(record.duration_millis, "invalid schedule duration")?,
                        record
                            .remaining_millis
                            .map(|value| to_sql_u64(value, "invalid schedule remainder"))
                            .transpose()?,
                        record.notification.as_ref().map(|value| value.0.as_str()),
                        record.notification.as_ref().map(|value| value.1.as_str()),
                        to_sql_u64(record.id, "invalid schedule ID")?,
                    ],
                )?;
            }
            SchedulerOutput::QueueElapsedDelivery(token) => {
                let record = read_schedule(connection, token.schedule_id)?
                    .ok_or(StateError::Corrupt("due schedule disappeared"))?;
                let deadline = record
                    .deadline_millis
                    .ok_or(StateError::Corrupt("due schedule has no deadline"))?;
                connection.execute(
                    "INSERT OR IGNORE INTO youth_pending_delivery(
                        schedule_id, generation, deadline_millis, creation_sequence
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        to_sql_u64(token.schedule_id, "invalid schedule ID")?,
                        to_sql_u64(token.generation, "invalid schedule generation")?,
                        to_sql_u64(deadline, "invalid schedule deadline")?,
                        to_sql_u64(record.creation_sequence, "invalid creation sequence")?,
                    ],
                )?;
            }
            SchedulerOutput::ArmWake { .. }
            | SchedulerOutput::CancelWake(_)
            | SchedulerOutput::DiscardStaleWake(_) => {}
        }
    }
    Ok(())
}

const fn schedule_status_sql(status: ScheduleStatus) -> i64 {
    match status {
        ScheduleStatus::Running => 0,
        ScheduleStatus::Paused => 1,
        ScheduleStatus::Due => 2,
        ScheduleStatus::Cancelled => 3,
    }
}

fn finish_scheduler_transaction<T>(
    connection: &Connection,
    result: Result<T, StateError>,
) -> Result<T, StateError> {
    match result {
        Ok(value) => {
            if let Err(error) = connection.execute_batch("COMMIT") {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            } else {
                Ok(value)
            }
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn to_sql_u64(value: u64, message: &'static str) -> Result<i64, StateError> {
    i64::try_from(value).map_err(|_| StateError::Corrupt(message))
}

fn from_sql_u64(value: i64, message: &'static str) -> Result<u64, StateError> {
    u64::try_from(value).map_err(|_| StateError::Corrupt(message))
}

fn compute_usage(connection: &Connection) -> Result<Usage, StateError> {
    let mut statement = connection.prepare(
        "SELECT key, kind, integer_value, text_value, blob_value FROM youth_state ORDER BY key",
    )?;
    let mut rows = statement.query([])?;
    let mut usage = Usage::default();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let value = decode_row_at(row, 1)?;
        usage.key_count = usage
            .key_count
            .checked_add(1)
            .ok_or(StateError::Corrupt("key count overflow"))?;
        usage.logical_bytes = usage
            .logical_bytes
            .checked_add(
                logical_entry_bytes(&key, &value)
                    .map_err(|_| StateError::Corrupt("logical byte overflow"))?,
            )
            .ok_or(StateError::Corrupt("logical byte overflow"))?;
    }
    Ok(usage)
}

fn read_value(connection: &Connection, key: &str) -> Result<Option<StateValue>, StateError> {
    let mut statement = connection.prepare(
        "SELECT kind, integer_value, text_value, blob_value FROM youth_state WHERE key = ?1",
    )?;
    let mut rows = statement.query([key])?;
    rows.next()?.map(decode_row).transpose()
}

fn decode_row(row: &rusqlite::Row<'_>) -> Result<StateValue, StateError> {
    decode_row_at(row, 0)
}

fn decode_row_at(row: &rusqlite::Row<'_>, offset: usize) -> Result<StateValue, StateError> {
    let kind: i64 = row.get(offset)?;
    let integer: Option<i64> = row.get(offset + 1)?;
    let text: Option<String> = row.get(offset + 2)?;
    let bytes: Option<Vec<u8>> = row.get(offset + 3)?;
    match (kind, integer, text, bytes) {
        (0, Some(0), None, None) => Ok(StateValue::Boolean(false)),
        (0, Some(1), None, None) => Ok(StateValue::Boolean(true)),
        (1, Some(value), None, None) => Ok(StateValue::Integer(value)),
        (2, None, Some(value), None) => Ok(StateValue::Text(value)),
        (3, None, None, Some(value)) => Ok(StateValue::Bytes(value)),
        _ => Err(StateError::Corrupt("invalid typed state row")),
    }
}

fn read_usage(connection: &Connection) -> Result<Usage, StateError> {
    let (keys, bytes): (i64, i64) = connection.query_row(
        "SELECT key_count, logical_bytes FROM youth_usage WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Usage {
        key_count: u32::try_from(keys).map_err(|_| StateError::Corrupt("invalid key count"))?,
        logical_bytes: u64::try_from(bytes)
            .map_err(|_| StateError::Corrupt("invalid logical byte count"))?,
    })
}

fn write_usage(connection: &Connection, usage: Usage) -> Result<(), StateError> {
    let keys = i64::from(usage.key_count);
    let bytes = i64::try_from(usage.logical_bytes).map_err(|_| StateError::QuotaExceeded)?;
    connection.execute(
        "UPDATE youth_usage SET key_count = ?1, logical_bytes = ?2 WHERE id = 1",
        params![keys, bytes],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SCHEMA_V1: &str = r#"
CREATE TABLE youth_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT, WITHOUT ROWID;
INSERT INTO youth_meta(key, value) VALUES ('schema-version', '1');
CREATE TABLE youth_state (
    key TEXT PRIMARY KEY,
    kind INTEGER NOT NULL,
    integer_value INTEGER,
    text_value TEXT,
    blob_value BLOB,
    CHECK (kind BETWEEN 0 AND 3),
    CHECK (
        (kind = 0 AND integer_value IN (0, 1) AND text_value IS NULL AND blob_value IS NULL)
        OR (kind = 1 AND integer_value IS NOT NULL AND text_value IS NULL AND blob_value IS NULL)
        OR (kind = 2 AND integer_value IS NULL AND text_value IS NOT NULL AND blob_value IS NULL)
        OR (kind = 3 AND integer_value IS NULL AND text_value IS NULL AND blob_value IS NOT NULL)
    )
) STRICT, WITHOUT ROWID;
CREATE TABLE youth_usage (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    key_count INTEGER NOT NULL CHECK (key_count >= 0),
    logical_bytes INTEGER NOT NULL CHECK (logical_bytes >= 0)
) STRICT, WITHOUT ROWID;
INSERT INTO youth_usage(id, key_count, logical_bytes) VALUES (1, 0, 0);
"#;

    fn memory() -> StateStore {
        StateStore::open(StateLocation::Memory, StateLimits::default()).unwrap()
    }

    #[test]
    fn typed_values_are_visible_inside_transaction_and_commit() {
        let mut store = memory();
        store.begin(GuestCallPhase::Mount).unwrap();
        let values = [
            ("boolean", StateValue::Boolean(true)),
            ("integer", StateValue::Integer(-7)),
            ("text", StateValue::Text("hello".into())),
            ("bytes", StateValue::Bytes(vec![0, 1, 255])),
        ];
        for (key, value) in &values {
            store.set(key, value.clone()).unwrap();
            assert_eq!(store.get(key).unwrap().as_ref(), Some(value));
        }
        let metrics = store.commit().unwrap();
        assert_eq!(metrics.writes, 4);
        assert!(metrics.committed);
        assert_eq!(store.summary().unwrap().key_count, 4);
    }

    #[test]
    fn rollback_reverts_state_and_usage() {
        let mut store = memory();
        store.begin(GuestCallPhase::Handle).unwrap();
        store.set("count", StateValue::Integer(1)).unwrap();
        store.rollback().unwrap();
        assert_eq!(store.summary().unwrap().logical_bytes, 0);
        store.begin(GuestCallPhase::Handle).unwrap();
        assert_eq!(store.get("count").unwrap(), None);
        store.rollback().unwrap();
    }

    #[test]
    fn delete_and_recreate_counts_two_writes_against_staged_state() {
        let mut store = memory();
        store.begin(GuestCallPhase::Mount).unwrap();
        store.set("count", StateValue::Integer(0)).unwrap();
        store.commit().unwrap();
        store.begin(GuestCallPhase::Handle).unwrap();
        assert!(store.delete("count").unwrap());
        assert!(!store.delete("count").unwrap());
        store.set("count", StateValue::Integer(1)).unwrap();
        assert_eq!(store.metrics().writes, 2);
        assert_eq!(store.metrics().bytes_after, 45);
        store.commit().unwrap();
    }

    #[test]
    fn invalid_value_is_a_call_but_not_a_write() {
        let limits = StateLimits {
            max_text_bytes: 1,
            ..StateLimits::default()
        };
        let mut store = StateStore::open(StateLocation::Memory, limits).unwrap();
        store.begin(GuestCallPhase::Handle).unwrap();
        assert!(matches!(
            store.set("key", StateValue::Text("no".into())),
            Err(StateError::InvalidValue)
        ));
        assert_eq!(store.metrics().calls, 1);
        assert_eq!(store.metrics().writes, 0);
        store.rollback().unwrap();
    }

    #[test]
    fn total_quota_failure_is_a_valid_write_attempt() {
        let limits = StateLimits {
            max_total_bytes: 1,
            ..StateLimits::default()
        };
        let mut store = StateStore::open(StateLocation::Memory, limits).unwrap();
        store.begin(GuestCallPhase::Handle).unwrap();
        assert!(matches!(
            store.set("key", StateValue::Integer(1)),
            Err(StateError::QuotaExceeded)
        ));
        assert_eq!(store.metrics().calls, 1);
        assert_eq!(store.metrics().writes, 1);
        assert_eq!(store.metrics().bytes_after, 0);
        store.rollback().unwrap();
    }

    #[test]
    fn resync_is_read_only() {
        let mut store = memory();
        store.begin(GuestCallPhase::Resync).unwrap();
        assert!(matches!(
            store.set("key", StateValue::Integer(1)),
            Err(StateError::ReadOnly)
        ));
        assert_eq!(store.metrics().calls, 1);
        store.rollback().unwrap();
    }

    #[test]
    fn file_state_persists() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        {
            let mut store =
                StateStore::open(StateLocation::File(path.clone()), StateLimits::default())
                    .unwrap();
            store.begin(GuestCallPhase::Mount).unwrap();
            store.set("count", StateValue::Integer(9)).unwrap();
            store.commit().unwrap();
        }
        let mut reopened =
            StateStore::open(StateLocation::File(path), StateLimits::default()).unwrap();
        reopened.begin(GuestCallPhase::Resync).unwrap();
        assert_eq!(reopened.get("count").unwrap(), Some(StateValue::Integer(9)));
        reopened.rollback().unwrap();
    }

    #[test]
    fn usage_mismatch_fails_open_and_can_be_repaired_after_backup() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let backup = directory.path().join("state.backup.sqlite3");
        {
            let mut store =
                StateStore::open(StateLocation::File(path.clone()), StateLimits::default())
                    .unwrap();
            store.begin(GuestCallPhase::Mount).unwrap();
            store.set("count", StateValue::Integer(1)).unwrap();
            store.commit().unwrap();
        }
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("UPDATE youth_usage SET logical_bytes = 0", [])
            .unwrap();
        drop(connection);
        assert!(matches!(
            StateStore::open(StateLocation::File(path.clone()), StateLimits::default()),
            Err(StateError::UsageMismatch)
        ));
        let verification = verify_file(&path).unwrap();
        assert!(!verification.usage_matches());
        assert!(repair_usage(&path, &backup).unwrap().usage_matches());
        assert!(backup.exists());
        assert!(StateStore::open(StateLocation::File(path), StateLimits::default()).is_ok());
    }

    #[test]
    fn schedule_create_pause_resume_and_cancel_round_trip() {
        let mut store = memory();
        store.begin(GuestCallPhase::Handle).unwrap();
        let created = store
            .schedule_create(1_000, 1_000, Some(("Title".into(), "Body".into())))
            .unwrap();
        assert_eq!(created.id, 1);
        assert_eq!(created.generation, 1);
        assert_eq!(created.status, ScheduleStatus::Running);
        assert_eq!(created.armed_at_millis, Some(1_000));
        assert_eq!(created.deadline_millis, Some(2_000));
        assert_eq!(created.duration_millis, 1_000);
        assert_eq!(created.remaining_millis, None);

        let paused = store.schedule_pause(1_600, created.id, 1).unwrap();
        assert_eq!(paused.generation, 2);
        assert_eq!(paused.status, ScheduleStatus::Paused);
        assert_eq!(paused.armed_at_millis, None);
        assert_eq!(paused.deadline_millis, None);
        assert_eq!(paused.remaining_millis, Some(400));

        let resumed = store.schedule_resume(5_000, created.id, 2).unwrap();
        assert_eq!(resumed.generation, 3);
        assert_eq!(resumed.status, ScheduleStatus::Running);
        assert_eq!(resumed.armed_at_millis, Some(5_000));
        assert_eq!(resumed.deadline_millis, Some(5_400));
        assert_eq!(resumed.duration_millis, 1_000);
        assert_eq!(resumed.remaining_millis, None);
        assert_eq!(resumed.notification, Some(("Title".into(), "Body".into())));

        store.schedule_cancel(created.id, 3).unwrap();
        let cancelled = store.schedule(created.id).unwrap().unwrap();
        assert_eq!(cancelled.status, ScheduleStatus::Cancelled);
        assert_eq!(cancelled.generation, 4);
        assert_eq!(store.metrics().calls, 4);
        assert_eq!(store.metrics().writes, 4);
        store.commit().unwrap();
    }

    #[test]
    fn stale_generations_and_invalid_states_are_rejected() {
        let mut store = memory();
        store.begin(GuestCallPhase::Handle).unwrap();
        let created = store.schedule_create(10, 1_000, None).unwrap();
        let paused = store
            .schedule_pause(110, created.id, created.generation)
            .unwrap();
        assert!(matches!(
            store.schedule_pause(120, created.id, created.generation),
            Err(StateError::StaleScheduleGeneration)
        ));
        assert!(matches!(
            store.schedule_resume(120, created.id, created.generation),
            Err(StateError::StaleScheduleGeneration)
        ));
        assert!(matches!(
            store.schedule_cancel(created.id, created.generation),
            Err(StateError::StaleScheduleGeneration)
        ));
        assert!(matches!(
            store.schedule_pause(120, paused.id, paused.generation),
            Err(StateError::InvalidScheduleState)
        ));
        let resumed = store
            .schedule_resume(200, paused.id, paused.generation)
            .unwrap();
        assert!(matches!(
            store.schedule_resume(210, resumed.id, resumed.generation),
            Err(StateError::InvalidScheduleState)
        ));
        assert!(matches!(
            store.schedule_cancel(999, 1),
            Err(StateError::UnknownSchedule)
        ));
        store.rollback().unwrap();
    }

    #[test]
    fn schedule_ids_are_not_reused_after_cancel() {
        let mut store = memory();
        store.begin(GuestCallPhase::Handle).unwrap();
        let first = store.schedule_create(0, 100, None).unwrap();
        store.schedule_cancel(first.id, first.generation).unwrap();
        let second = store.schedule_create(0, 100, None).unwrap();
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        store.commit().unwrap();
    }

    #[test]
    fn schedule_limits_return_specific_errors() {
        let limits = StateLimits {
            max_active_schedules: 1,
            min_schedule_millis: 100,
            max_schedule_millis: 200,
            max_notification_title_bytes: 3,
            max_notification_body_bytes: 4,
            ..StateLimits::default()
        };
        let mut store = StateStore::open(StateLocation::Memory, limits).unwrap();
        store.begin(GuestCallPhase::Handle).unwrap();
        assert!(matches!(
            store.schedule_create(0, 99, None),
            Err(StateError::InvalidScheduleDuration)
        ));
        assert!(matches!(
            store.schedule_create(0, 201, None),
            Err(StateError::InvalidScheduleDuration)
        ));
        assert!(matches!(
            store.schedule_create(0, 100, Some(("four".into(), "body".into()))),
            Err(StateError::InvalidScheduleNotification)
        ));
        assert!(matches!(
            store.schedule_create(0, 100, Some(("ok".into(), "large".into()))),
            Err(StateError::InvalidScheduleNotification)
        ));
        store.schedule_create(0, 100, None).unwrap();
        assert!(matches!(
            store.schedule_create(0, 100, None),
            Err(StateError::TooManySchedules)
        ));
        store.rollback().unwrap();
    }

    #[test]
    fn schedule_rollback_leaves_no_row_or_consumed_id() {
        let mut store = memory();
        store.begin(GuestCallPhase::Handle).unwrap();
        store.schedule_create(0, 100, None).unwrap();
        store.rollback().unwrap();
        assert!(store.schedules().unwrap().is_empty());

        store.begin(GuestCallPhase::Handle).unwrap();
        assert_eq!(store.schedule_create(0, 100, None).unwrap().id, 1);
        store.commit().unwrap();
    }

    #[test]
    fn schedule_mutations_are_rejected_during_resync() {
        let mut store = memory();
        store.begin(GuestCallPhase::Resync).unwrap();
        assert!(matches!(
            store.schedule_create(0, 100, None),
            Err(StateError::ReadOnly)
        ));
        assert_eq!(store.metrics().calls, 1);
        assert_eq!(store.metrics().writes, 0);
        store.rollback().unwrap();
    }

    #[test]
    fn schedules_are_readable_after_fresh_open_without_a_transaction() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        {
            let mut store =
                StateStore::open(StateLocation::File(path.clone()), StateLimits::default())
                    .unwrap();
            store.begin(GuestCallPhase::Handle).unwrap();
            store.schedule_create(1_000, 500, None).unwrap();
            store.commit().unwrap();
        }
        let reopened = StateStore::open(StateLocation::File(path), StateLimits::default()).unwrap();
        assert!(!reopened.transaction_active());
        assert_eq!(reopened.phase(), GuestCallPhase::Idle);
        assert_eq!(reopened.schedules().unwrap().len(), 1);
    }

    #[test]
    fn schedules_do_not_change_state_usage_or_verification() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let mut store =
            StateStore::open(StateLocation::File(path.clone()), StateLimits::default()).unwrap();
        store.begin(GuestCallPhase::Handle).unwrap();
        store.set("count", StateValue::Integer(7)).unwrap();
        store.commit().unwrap();
        let before = store.summary().unwrap();
        store.begin(GuestCallPhase::Handle).unwrap();
        store.schedule_create(1_000, 500, None).unwrap();
        store.commit().unwrap();
        assert_eq!(store.summary().unwrap(), before);
        drop(store);
        let verification = verify_file(&path).unwrap();
        assert!(verification.usage_matches());
        assert_eq!(verification.stored.key_count, 1);
        assert_eq!(verification.stored.logical_bytes, 45);
    }

    #[test]
    fn repeated_overdue_reconciliation_has_one_pending_delivery() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let mut store =
            StateStore::open(StateLocation::File(path.clone()), StateLimits::default()).unwrap();
        store.begin(GuestCallPhase::Handle).unwrap();
        store.schedule_create(1_000, 100, None).unwrap();
        store.commit().unwrap();
        drop(store);
        let mut first =
            StateStore::open(StateLocation::File(path.clone()), StateLimits::default()).unwrap();
        first.reconcile_overdue(1_100).unwrap();
        drop(first);
        let mut second =
            StateStore::open(StateLocation::File(path), StateLimits::default()).unwrap();
        second.reconcile_overdue(1_100).unwrap();
        assert_eq!(second.pending_deliveries().unwrap().len(), 1);
        assert_eq!(
            second.schedule(1).unwrap().unwrap().status,
            ScheduleStatus::Due
        );
    }

    #[test]
    fn due_deliveries_are_ordered_by_deadline_then_creation_sequence() {
        let mut store = memory();
        store.begin(GuestCallPhase::Handle).unwrap();
        let later = store.schedule_create(1_000, 300, None).unwrap();
        let first_tie = store.schedule_create(1_000, 200, None).unwrap();
        let second_tie = store.schedule_create(1_000, 200, None).unwrap();
        store.commit().unwrap();
        store.reconcile_overdue(1_300).unwrap();
        let ids: Vec<_> = store
            .pending_deliveries()
            .unwrap()
            .iter()
            .map(|delivery| delivery.schedule_id)
            .collect();
        assert_eq!(ids, vec![first_tie.id, second_tie.id, later.id]);
    }

    #[test]
    fn wake_rechecks_authoritative_state_before_queuing() {
        let mut store = memory();
        store.begin(GuestCallPhase::Handle).unwrap();
        let created = store.schedule_create(1_000, 100, None).unwrap();
        store.commit().unwrap();
        let token = WakeToken::from(&created);
        assert_eq!(
            store.receive_wake(token, 1_099).unwrap(),
            vec![SchedulerOutput::DiscardStaleWake(token)]
        );
        let due = store.receive_wake(token, 1_100).unwrap();
        assert_eq!(
            due.iter()
                .filter(|output| matches!(output, SchedulerOutput::QueueElapsedDelivery(_)))
                .count(),
            1
        );
        assert_eq!(
            store.receive_wake(token, 1_100).unwrap(),
            vec![SchedulerOutput::DiscardStaleWake(token)]
        );
        assert_eq!(store.pending_deliveries().unwrap().len(), 1);
    }

    #[test]
    fn scheduler_storage_operates_without_any_guest_instance() {
        let mut store = memory();
        store.begin(GuestCallPhase::Handle).unwrap();
        store.schedule_create(0, 100, None).unwrap();
        store.commit().unwrap();
        store.reconcile_overdue(100).unwrap();
        assert_eq!(store.pending_deliveries().unwrap().len(), 1);
    }

    #[test]
    fn version_one_database_migrates_in_place_with_state_and_usage_intact() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA_V1).unwrap();
        connection
            .execute(
                "INSERT INTO youth_state(
                    key, kind, integer_value, text_value, blob_value
                 ) VALUES ('count', 1, 9, NULL, NULL)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE youth_usage SET key_count = 1, logical_bytes = 45 WHERE id = 1",
                [],
            )
            .unwrap();
        drop(connection);

        let mut migrated =
            StateStore::open(StateLocation::File(path.clone()), StateLimits::default()).unwrap();
        assert_eq!(migrated.summary().unwrap().schema_version, 3);
        assert!(migrated.schedules().unwrap().is_empty());
        migrated.begin(GuestCallPhase::Resync).unwrap();
        assert_eq!(migrated.get("count").unwrap(), Some(StateValue::Integer(9)));
        migrated.rollback().unwrap();
        drop(migrated);

        let verification = verify_file(&path).unwrap();
        assert_eq!(verification.schema_version, 3);
        assert!(verification.integrity_ok);
        assert!(verification.usage_matches());
        assert_eq!(verification.stored.key_count, 1);
        assert_eq!(verification.stored.logical_bytes, 45);
    }

    #[test]
    fn version_two_database_migrates_in_place_with_schedules_intact() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA_V1).unwrap();
        connection.execute_batch(MIGRATE_V1_TO_V2).unwrap();
        connection
            .execute(
                "INSERT INTO youth_schedule(
                    id, generation, status, armed_at_millis, deadline_millis, duration_millis,
                    remaining_millis, notification_title, notification_body
                 ) VALUES (1, 2, 0, 1000, 1100, 100, NULL, NULL, NULL)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE youth_meta SET value = '2' WHERE key = 'next-schedule-id'",
                [],
            )
            .unwrap();
        drop(connection);
        let migrated = StateStore::open(StateLocation::File(path), StateLimits::default()).unwrap();
        assert_eq!(migrated.summary().unwrap().schema_version, 3);
        let schedule = migrated.schedule(1).unwrap().unwrap();
        assert_eq!(schedule.status, ScheduleStatus::Running);
        assert_eq!(schedule.creation_sequence, 1);
        assert!(migrated.pending_deliveries().unwrap().is_empty());
    }
}
