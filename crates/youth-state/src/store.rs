use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, MAIN_DB, OptionalExtension, params};
use thiserror::Error;

use crate::{StateLimits, StateLocation, StateSummary, StateValue, logical_entry_bytes};

const SCHEMA_VERSION: u32 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const SCHEMA: &str = r#"
CREATE TABLE youth_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT, WITHOUT ROWID;
INSERT INTO youth_meta(key, value) VALUES ('schema-version', '1');

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
        self.require_transaction()?;
        self.connection.execute_batch("ROLLBACK")?;
        self.finish(false);
        Ok(self.metrics)
    }

    pub fn get(&mut self, key: &str) -> Result<Option<StateValue>, StateError> {
        self.attempt_call()?;
        self.require_transaction()?;
        self.validate_key(key)?;
        read_value(&self.connection, key)
    }

    pub fn set(&mut self, key: &str, value: StateValue) -> Result<(), StateError> {
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

fn verify_connection(connection: &Connection) -> Result<Verification, StateError> {
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    let integrity_ok = integrity == "ok";
    verify_schema_shape(connection)?;
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
    if !verification.usage_matches() {
        return Err(StateError::UsageMismatch);
    }
    Ok(())
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

fn verify_schema_shape(connection: &Connection) -> Result<(), StateError> {
    for table in ["youth_meta", "youth_state", "youth_usage"] {
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
}
