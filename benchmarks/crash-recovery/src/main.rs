//! A standalone filesystem prototype for guest-controlled crash recovery.
//!
//! This binary is deliberately independent from the Instar workspace. It
//! measures storage and recovery behavior without proposing a production API.

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitStatus};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHECKPOINT_EDIT_INTERVAL: u64 = 2_048;
const CHECKPOINT_BYTE_INTERVAL: usize = 1 << 20;
const SYNC_INTERVAL: Duration = Duration::from_millis(5);
const SYNC_BYTES: usize = 64 << 10;
const JOURNAL_HEADER: usize = 16;
const CHECKPOINT_HEADER: usize = 24;
const CHECKPOINT_MAGIC: &[u8; 8] = b"ICRBv001";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Strategy {
    CheckpointAtomic,
    JournalAppend,
    JournalCheckpoint,
    PageCache,
    SyncPerEdit,
    SyncBatch,
}

impl Strategy {
    const ALL: [Self; 6] = [
        Self::CheckpointAtomic,
        Self::JournalAppend,
        Self::JournalCheckpoint,
        Self::PageCache,
        Self::SyncPerEdit,
        Self::SyncBatch,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::CheckpointAtomic => "checkpoint_atomic",
            Self::JournalAppend => "journal_append",
            Self::JournalCheckpoint => "journal_checkpoint",
            Self::PageCache => "page_cache",
            Self::SyncPerEdit => "sync_per_edit",
            Self::SyncBatch => "sync_batch",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|strategy| strategy.name() == value)
            .ok_or_else(|| format!("unknown strategy: {value}").into())
    }

    fn uses_journal(self) -> bool {
        !matches!(self, Self::CheckpointAtomic)
    }

    fn needs_final_flush(self) -> bool {
        matches!(self, Self::JournalAppend | Self::SyncBatch)
    }
}

#[derive(Clone, Copy, Debug)]
struct Workload {
    name: &'static str,
    payload: &'static [u8],
}

const WORKLOADS: [Workload; 5] = [
    Workload {
        name: "ascii_character",
        payload: b"a",
    },
    Workload {
        name: "multibyte_unicode",
        payload: "界".as_bytes(),
    },
    Workload {
        name: "ime_commit",
        payload: "日本語".as_bytes(),
    },
    Workload {
        name: "one_kib_edit",
        payload: &[b'x'; 1024],
    },
    Workload {
        name: "one_hundred_kib_paste",
        payload: &[b'x'; 100 * 1024],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CrashPhase {
    AfterWrite,
    AfterFlush,
}

impl CrashPhase {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "after_write" => Ok(Self::AfterWrite),
            "after_flush" => Ok(Self::AfterFlush),
            _ => Err(format!("unknown crash phase: {value}").into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::AfterWrite => "after_write",
            Self::AfterFlush => "after_flush",
        }
    }
}

#[derive(Debug)]
struct Event {
    strategy: &'static str,
    workload: &'static str,
    edit: u64,
    event: &'static str,
    elapsed_ns: u128,
    bytes: usize,
    sequence: u64,
}

#[derive(Default)]
struct EventLog {
    events: Vec<Event>,
}

impl EventLog {
    fn push(
        &mut self,
        strategy: Strategy,
        workload: Workload,
        edit: u64,
        event: &'static str,
        elapsed_ns: u128,
        bytes: usize,
        sequence: u64,
    ) {
        self.events.push(Event {
            strategy: strategy.name(),
            workload: workload.name,
            edit,
            event,
            elapsed_ns,
            bytes,
            sequence,
        });
    }
}

struct Session {
    strategy: Strategy,
    workload: Workload,
    root: PathBuf,
    journal: Option<File>,
    document: Vec<u8>,
    sequence: u64,
    journal_bytes: usize,
    edits_since_checkpoint: u64,
    bytes_since_checkpoint: usize,
    pending_sync_bytes: usize,
    last_sync: Instant,
}

impl Session {
    fn new(strategy: Strategy, workload: Workload, root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        let journal = if strategy.uses_journal() {
            Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(root.join("journal"))?,
            )
        } else {
            None
        };
        let mut session = Self {
            strategy,
            workload,
            root,
            journal,
            document: Vec::new(),
            sequence: 0,
            journal_bytes: 0,
            edits_since_checkpoint: 0,
            bytes_since_checkpoint: 0,
            pending_sync_bytes: 0,
            last_sync: Instant::now(),
        };
        if matches!(strategy, Strategy::CheckpointAtomic) {
            session.write_checkpoint(0, 0, &mut EventLog::default(), false)?;
        }
        Ok(session)
    }

    fn apply_edit(
        &mut self,
        edit: u64,
        crash: Option<(CrashPhase, u64)>,
        log: &mut EventLog,
    ) -> Result<()> {
        let operation_start = Instant::now();
        self.sequence = self.sequence.saturating_add(1);
        self.document.extend_from_slice(self.workload.payload);

        if self.strategy == Strategy::CheckpointAtomic {
            let checkpoint_bytes = checkpoint_bytes(self.sequence, &self.document);
            let write_start = Instant::now();
            let mut temp = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(self.root.join("checkpoint.tmp"))?;
            let written = timed_write_all(&mut temp, &checkpoint_bytes)?;
            let write_ns = write_start.elapsed().as_nanos();
            log.push(
                self.strategy,
                self.workload,
                edit,
                "write_return",
                write_ns,
                written,
                self.sequence,
            );
            log.push(
                self.strategy,
                self.workload,
                edit,
                "page_cache_accepted",
                write_ns,
                written,
                self.sequence,
            );
            maybe_crash(crash, CrashPhase::AfterWrite, edit);

            flush_file(
                &temp,
                self.strategy,
                self.workload,
                edit,
                self.sequence,
                log,
            )?;
            drop(temp);
            fs::rename(
                self.root.join("checkpoint.tmp"),
                self.root.join("checkpoint"),
            )?;
            let directory = File::open(&self.root)?;
            flush_file(
                &directory,
                self.strategy,
                self.workload,
                edit,
                self.sequence,
                log,
            )?;
            maybe_crash(crash, CrashPhase::AfterFlush, edit);
        } else {
            let mut record = Vec::with_capacity(JOURNAL_HEADER + self.workload.payload.len());
            record.extend_from_slice(&self.sequence.to_le_bytes());
            record.extend_from_slice(&(self.workload.payload.len() as u64).to_le_bytes());
            record.extend_from_slice(self.workload.payload);
            let journal = self
                .journal
                .as_mut()
                .expect("journal strategy has a journal");
            let write_start = Instant::now();
            let written = timed_write_all(journal, &record)?;
            let write_ns = write_start.elapsed().as_nanos();
            log.push(
                self.strategy,
                self.workload,
                edit,
                "write_return",
                write_ns,
                written,
                self.sequence,
            );
            log.push(
                self.strategy,
                self.workload,
                edit,
                "page_cache_accepted",
                write_ns,
                written,
                self.sequence,
            );
            self.journal_bytes += written;
            self.pending_sync_bytes += written;
            self.edits_since_checkpoint += 1;
            self.bytes_since_checkpoint += written;
            maybe_crash(crash, CrashPhase::AfterWrite, edit);

            let should_sync = match self.strategy {
                Strategy::SyncPerEdit => true,
                Strategy::SyncBatch => {
                    self.pending_sync_bytes >= SYNC_BYTES
                        || self.last_sync.elapsed() >= SYNC_INTERVAL
                }
                _ => false,
            };
            let mut flushed = false;
            if should_sync {
                self.flush_journal(edit, log)?;
                flushed = true;
                maybe_crash(crash, CrashPhase::AfterFlush, edit);
            }

            // The crash matrix asks for a controlled post-flush observation
            // even when this policy's normal threshold has not fired yet.
            if crash == Some((CrashPhase::AfterFlush, edit)) && !flushed {
                self.flush_journal(edit, log)?;
                maybe_crash(crash, CrashPhase::AfterFlush, edit);
            }

            if self.strategy == Strategy::JournalCheckpoint
                && (self.edits_since_checkpoint >= CHECKPOINT_EDIT_INTERVAL
                    || self.bytes_since_checkpoint >= CHECKPOINT_BYTE_INTERVAL)
            {
                self.flush_journal(edit, log)?;
                maybe_crash(crash, CrashPhase::AfterFlush, edit);
                self.write_checkpoint(self.sequence, edit, log, true)?;
            }
        }

        log.push(
            self.strategy,
            self.workload,
            edit,
            "operation_return",
            operation_start.elapsed().as_nanos(),
            self.workload.payload.len(),
            self.sequence,
        );
        Ok(())
    }

    fn flush_journal(&mut self, edit: u64, log: &mut EventLog) -> Result<()> {
        let journal = self
            .journal
            .as_ref()
            .expect("journal strategy has a journal");
        flush_file(
            journal,
            self.strategy,
            self.workload,
            edit,
            self.sequence,
            log,
        )?;
        self.pending_sync_bytes = 0;
        self.last_sync = Instant::now();
        Ok(())
    }

    fn write_checkpoint(
        &mut self,
        sequence: u64,
        edit: u64,
        log: &mut EventLog,
        reset_journal: bool,
    ) -> Result<()> {
        let bytes = checkpoint_bytes(sequence, &self.document);
        let write_start = Instant::now();
        let mut temp = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(self.root.join("checkpoint.tmp"))?;
        let written = timed_write_all(&mut temp, &bytes)?;
        let write_ns = write_start.elapsed().as_nanos();
        log.push(
            self.strategy,
            self.workload,
            edit,
            "checkpoint_write_return",
            write_ns,
            written,
            sequence,
        );
        log.push(
            self.strategy,
            self.workload,
            edit,
            "checkpoint_page_cache_accepted",
            write_ns,
            written,
            sequence,
        );
        flush_file(&temp, self.strategy, self.workload, edit, sequence, log)?;
        drop(temp);
        fs::rename(
            self.root.join("checkpoint.tmp"),
            self.root.join("checkpoint"),
        )?;
        let directory = File::open(&self.root)?;
        flush_file(
            &directory,
            self.strategy,
            self.workload,
            edit,
            sequence,
            log,
        )?;
        if reset_journal {
            let journal_path = self.root.join("journal");
            let journal = OpenOptions::new().write(true).open(&journal_path)?;
            journal.set_len(0)?;
            self.journal_bytes = 0;
            self.edits_since_checkpoint = 0;
            self.bytes_since_checkpoint = 0;
        }
        Ok(())
    }

    fn finish(&mut self, edit: u64, log: &mut EventLog) -> Result<()> {
        if self.strategy == Strategy::JournalCheckpoint && self.pending_sync_bytes > 0 {
            self.flush_journal(edit, log)?;
        }
        if self.strategy.needs_final_flush() && self.pending_sync_bytes > 0 {
            self.flush_journal(edit, log)?;
        }
        Ok(())
    }
}

fn timed_write_all(file: &mut File, bytes: &[u8]) -> io::Result<usize> {
    let mut offset = 0;
    while offset < bytes.len() {
        let written = file.write(&bytes[offset..])?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write returned zero",
            ));
        }
        offset += written;
    }
    Ok(offset)
}

fn flush_file(
    file: &File,
    strategy: Strategy,
    workload: Workload,
    edit: u64,
    sequence: u64,
    log: &mut EventLog,
) -> Result<()> {
    let start = Instant::now();
    file.sync_data()?;
    log.push(
        strategy,
        workload,
        edit,
        "flush_return",
        start.elapsed().as_nanos(),
        0,
        sequence,
    );
    Ok(())
}

fn maybe_crash(crash: Option<(CrashPhase, u64)>, phase: CrashPhase, edit: u64) {
    if crash == Some((phase, edit)) {
        process::abort();
    }
}

fn checkpoint_bytes(sequence: u64, document: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(CHECKPOINT_HEADER + document.len());
    bytes.extend_from_slice(CHECKPOINT_MAGIC);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&(document.len() as u64).to_le_bytes());
    bytes.extend_from_slice(document);
    bytes
}

fn recover(strategy: Strategy, root: &Path) -> Result<(u64, usize)> {
    let (mut sequence, mut document) = if strategy == Strategy::CheckpointAtomic {
        read_checkpoint(&root.join("checkpoint"))?
    } else if root.join("checkpoint").exists() {
        read_checkpoint(&root.join("checkpoint"))?
    } else {
        (0, Vec::new())
    };

    if strategy.uses_journal() {
        let mut journal = Vec::new();
        if root.join("journal").exists() {
            File::open(root.join("journal"))?.read_to_end(&mut journal)?;
        }
        let mut offset = 0;
        while offset + JOURNAL_HEADER <= journal.len() {
            let next_sequence = u64::from_le_bytes(journal[offset..offset + 8].try_into()?);
            let length = u64::from_le_bytes(journal[offset + 8..offset + 16].try_into()?) as usize;
            let end = offset
                .checked_add(JOURNAL_HEADER)
                .and_then(|value| value.checked_add(length))
                .ok_or("journal length overflow")?;
            if end > journal.len() {
                break;
            }
            if next_sequence > sequence {
                document.extend_from_slice(&journal[offset + JOURNAL_HEADER..end]);
                sequence = next_sequence;
            }
            offset = end;
        }
    }
    Ok((sequence, document.len()))
}

fn read_checkpoint(path: &Path) -> Result<(u64, Vec<u8>)> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.len() < CHECKPOINT_HEADER || &bytes[..8] != CHECKPOINT_MAGIC {
        return Err(format!("invalid checkpoint: {}", path.display()).into());
    }
    let sequence = u64::from_le_bytes(bytes[8..16].try_into()?);
    let length = u64::from_le_bytes(bytes[16..24].try_into()?) as usize;
    if CHECKPOINT_HEADER + length > bytes.len() {
        return Err(format!("truncated checkpoint: {}", path.display()).into());
    }
    Ok((
        sequence,
        bytes[CHECKPOINT_HEADER..CHECKPOINT_HEADER + length].to_vec(),
    ))
}

fn run_latency(output: &Path, latency_edits: u64) -> Result<()> {
    let mut log = EventLog::default();
    let root = temp_root("latency")?;
    for strategy in Strategy::ALL {
        for workload in WORKLOADS {
            let session_root = root.join(strategy.name()).join(workload.name);
            let mut session = Session::new(strategy, workload, session_root)?;
            let edits = latency_edits_for(workload, latency_edits);
            for edit in 1..=edits {
                session.apply_edit(edit, None, &mut log)?;
            }
            session.finish(edits + 1, &mut log)?;
        }
    }
    write_events(output.join("events.csv"), &log.events)?;
    write_summary(output.join("summary.csv"), &log.events)?;
    remove_dir(&root)?;
    Ok(())
}

fn run_recovery(output: &Path, counts: &[u64]) -> Result<()> {
    let mut rows = Vec::new();
    let root = temp_root("recovery")?;
    let workload = WORKLOADS[0];
    for strategy in Strategy::ALL {
        for &count in counts {
            let session_root = root.join(strategy.name()).join(count.to_string());
            let mut session = Session::new(strategy, workload, session_root.clone())?;
            let mut log = EventLog::default();
            if strategy == Strategy::CheckpointAtomic {
                // Recovery timing should measure opening one final checkpoint,
                // not spend the experiment rewriting all earlier checkpoints.
                session.document = workload.payload.repeat(count as usize);
                session.sequence = count;
                session.write_checkpoint(count, count, &mut log, false)?;
            } else {
                for edit in 1..=count {
                    session.apply_edit(edit, None, &mut log)?;
                }
            }
            session.finish(count + 1, &mut log)?;
            drop(session);
            let start = Instant::now();
            let (recovered, bytes) = recover(strategy, &session_root)?;
            let elapsed = start.elapsed().as_nanos();
            rows.push((strategy.name(), count, recovered, bytes, elapsed));
        }
    }
    write_recovery(output.join("recovery.csv"), &rows)?;
    remove_dir(&root)?;
    Ok(())
}

fn run_crash(output: &Path, crash_edits: u64) -> Result<()> {
    let mut rows = Vec::new();
    for strategy in Strategy::ALL {
        for phase in [CrashPhase::AfterWrite, CrashPhase::AfterFlush] {
            let root = temp_root("crash")?;
            let executable = env::current_exe()?;
            let status = Command::new(&executable)
                .arg("crash-child")
                .arg("--root")
                .arg(&root)
                .arg("--strategy")
                .arg(strategy.name())
                .arg("--workload")
                .arg(WORKLOADS[0].name)
                .arg("--edits")
                .arg(crash_edits.to_string())
                .arg("--phase")
                .arg(phase.name())
                .status()?;
            let (recovered, bytes) = recover(strategy, &root)?;
            rows.push((
                strategy.name(),
                phase.name(),
                crash_edits,
                recovered,
                bytes,
                status_label(status),
            ));
            remove_dir(&root)?;
        }
    }
    write_crash(output.join("crash.csv"), &rows)?;
    Ok(())
}

fn crash_child(
    root: &Path,
    strategy: Strategy,
    workload: Workload,
    edits: u64,
    phase: CrashPhase,
) -> Result<()> {
    let mut session = Session::new(strategy, workload, root.to_path_buf())?;
    let mut log = EventLog::default();
    for edit in 1..=edits {
        session.apply_edit(edit, Some((phase, edits)), &mut log)?;
    }
    Ok(())
}

fn write_events(path: PathBuf, events: &[Event]) -> Result<()> {
    let mut file = create_output(path)?;
    writeln!(
        file,
        "strategy,workload,edit,event,elapsed_ns,bytes,sequence"
    )?;
    for event in events {
        writeln!(
            file,
            "{},{},{},{},{},{},{}",
            event.strategy,
            event.workload,
            event.edit,
            event.event,
            event.elapsed_ns,
            event.bytes,
            event.sequence
        )?;
    }
    Ok(())
}

#[derive(Default)]
struct Stats {
    write: Vec<u128>,
    page_cache: Vec<u128>,
    flush: Vec<u128>,
    operation: Vec<u128>,
    bytes: usize,
    edits: u64,
}

fn write_summary(path: PathBuf, events: &[Event]) -> Result<()> {
    let mut grouped: BTreeMap<(&str, &str), Stats> = BTreeMap::new();
    for event in events {
        let stats = grouped.entry((event.strategy, event.workload)).or_default();
        match event.event {
            "write_return" | "checkpoint_write_return" => {
                stats.write.push(event.elapsed_ns);
                stats.bytes += event.bytes;
                stats.edits = stats.edits.max(event.edit);
            }
            "page_cache_accepted" | "checkpoint_page_cache_accepted" => {
                stats.page_cache.push(event.elapsed_ns)
            }
            "flush_return" => stats.flush.push(event.elapsed_ns),
            "operation_return" => stats.operation.push(event.elapsed_ns),
            _ => {}
        }
    }
    let mut file = create_output(path)?;
    writeln!(
        file,
        "strategy,workload,edits,bytes_written,write_p50_ns,write_p95_ns,write_p99_ns,page_cache_p50_ns,page_cache_p95_ns,page_cache_p99_ns,flush_count,flush_p50_ns,flush_p95_ns,flush_p99_ns,operation_p50_ns,operation_p95_ns,operation_p99_ns,payload_bytes_per_s,physical_write_bytes_per_s"
    )?;
    for ((strategy, workload), mut stats) in grouped {
        stats.write.sort_unstable();
        stats.page_cache.sort_unstable();
        stats.flush.sort_unstable();
        stats.operation.sort_unstable();
        let operation_bytes = WORKLOADS
            .iter()
            .find(|item| item.name == workload)
            .map(|item| item.payload.len() * stats.edits as usize)
            .unwrap_or_default();
        let write_total: u128 = stats.write.iter().sum();
        let operation_total: u128 = stats.operation.iter().sum();
        writeln!(
            file,
            "{strategy},{workload},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            stats.edits,
            stats.bytes,
            percentile(&stats.write, 50),
            percentile(&stats.write, 95),
            percentile(&stats.write, 99),
            percentile(&stats.page_cache, 50),
            percentile(&stats.page_cache, 95),
            percentile(&stats.page_cache, 99),
            stats.flush.len(),
            percentile(&stats.flush, 50),
            percentile(&stats.flush, 95),
            percentile(&stats.flush, 99),
            percentile(&stats.operation, 50),
            percentile(&stats.operation, 95),
            percentile(&stats.operation, 99),
            rate(operation_bytes, operation_total),
            rate(stats.bytes, write_total),
        )?;
    }
    Ok(())
}

fn percentile(values: &[u128], percentile: usize) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values[index.min(values.len() - 1)]
}

fn rate(bytes: usize, nanos: u128) -> u128 {
    if nanos == 0 {
        0
    } else {
        (bytes as u128 * 1_000_000_000) / nanos
    }
}

fn latency_edits_for(workload: Workload, requested: u64) -> u64 {
    let scaled = if workload.payload.len() >= 100 * 1024 {
        requested / 20
    } else if workload.payload.len() >= 1024 {
        requested / 2
    } else {
        requested
    };
    scaled.max(20)
}

fn write_recovery(path: PathBuf, rows: &[(&str, u64, u64, usize, u128)]) -> Result<()> {
    let mut file = create_output(path)?;
    writeln!(
        file,
        "strategy,requested_edits,recovered_edits,recovered_bytes,recovery_ns"
    )?;
    for (strategy, requested, recovered, bytes, elapsed) in rows {
        writeln!(file, "{strategy},{requested},{recovered},{bytes},{elapsed}")?;
    }
    Ok(())
}

fn write_crash(path: PathBuf, rows: &[(&str, &str, u64, u64, usize, String)]) -> Result<()> {
    let mut file = create_output(path)?;
    writeln!(
        file,
        "strategy,crash_phase,requested_edits,recovered_edits,recovered_bytes,child_status"
    )?;
    for (strategy, phase, requested, recovered, bytes, status) in rows {
        writeln!(
            file,
            "{strategy},{phase},{requested},{recovered},{bytes},{status}"
        )?;
    }
    Ok(())
}

fn create_output(path: PathBuf) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(File::create(path)?)
}

fn write_metadata(
    output: &Path,
    latency_edits: u64,
    recovery_counts: &[u64],
    crash_edits: u64,
) -> Result<()> {
    let mut file = create_output(output.join("metadata.txt"))?;
    writeln!(file, "timestamp_unix_ns={}", unix_nanos())?;
    writeln!(file, "os={}", command_output("uname", &["-a"]))?;
    writeln!(file, "arch={}", env::consts::ARCH)?;
    writeln!(file, "family={}", env::consts::FAMILY)?;
    writeln!(file, "latency_edits={latency_edits}")?;
    for workload in WORKLOADS {
        writeln!(
            file,
            "latency_edits_{}={}",
            workload.name,
            latency_edits_for(workload, latency_edits)
        )?;
    }
    writeln!(file, "recovery_edits={}", join_counts(recovery_counts))?;
    writeln!(file, "crash_edits={crash_edits}")?;
    writeln!(file, "checkpoint_edit_interval={CHECKPOINT_EDIT_INTERVAL}")?;
    writeln!(file, "checkpoint_byte_interval={CHECKPOINT_BYTE_INTERVAL}")?;
    writeln!(file, "sync_interval_ms={}", SYNC_INTERVAL.as_millis())?;
    writeln!(file, "sync_bytes={SYNC_BYTES}")?;
    writeln!(file)?;
    writeln!(file, "measurement_boundaries:")?;
    writeln!(
        file,
        "write_return=File::write returned; bytes were accepted by the kernel write path"
    )?;
    writeln!(
        file,
        "page_cache_accepted=separate event at the same observable write-return boundary; no second POSIX timestamp exists"
    )?;
    writeln!(file, "flush_return=File::sync_data returned")?;
    writeln!(
        file,
        "crash_after_write=child aborts after write return and before the next explicit flush"
    )?;
    writeln!(
        file,
        "crash_after_flush=child aborts after explicit sync_data returns"
    )?;
    writeln!(
        file,
        "crash_limit=process crash/restart observation, not a power-loss guarantee"
    )?;
    Ok(())
}

fn temp_root(label: &str) -> Result<PathBuf> {
    let root = env::temp_dir().join(format!(
        "instar-crash-recovery-{label}-{}-{}",
        process::id(),
        unix_nanos()
    ));
    fs::create_dir_all(&root)?;
    Ok(root)
}

fn remove_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn command_output(command: &str, args: &[&str]) -> String {
    Command::new(command)
        .args(args)
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

fn join_counts(counts: &[u64]) -> String {
    counts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn status_label(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("exit:{code}")
    } else {
        "signaled".to_string()
    }
}

fn parse_value(args: &[String], flag: &str) -> Result<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .ok_or_else(|| format!("missing {flag}").into())
}

fn parse_counts(value: &str) -> Result<Vec<u64>> {
    value
        .split(',')
        .map(|item| item.parse::<u64>().map_err(|error| error.into()))
        .collect()
}

fn workload_by_name(name: &str) -> Result<Workload> {
    WORKLOADS
        .into_iter()
        .find(|workload| workload.name == name)
        .ok_or_else(|| format!("unknown workload: {name}").into())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("measure");
    match command {
        "measure" => {
            let output = args
                .windows(2)
                .find(|pair| pair[0] == "--output")
                .map(|pair| PathBuf::from(&pair[1]))
                .unwrap_or_else(|| PathBuf::from("benchmarks/crash-recovery/results/latest"));
            let latency_edits = args
                .windows(2)
                .find(|pair| pair[0] == "--latency-edits")
                .map(|pair| pair[1].parse())
                .transpose()?
                .unwrap_or(1_000);
            let recovery_counts = args
                .windows(2)
                .find(|pair| pair[0] == "--recovery-edits")
                .map(|pair| parse_counts(&pair[1]))
                .transpose()?
                .unwrap_or_else(|| vec![1_000, 10_000, 100_000]);
            let crash_edits = args
                .windows(2)
                .find(|pair| pair[0] == "--crash-edits")
                .map(|pair| pair[1].parse())
                .transpose()?
                .unwrap_or(100);
            fs::create_dir_all(&output)?;
            write_metadata(&output, latency_edits, &recovery_counts, crash_edits)?;
            run_latency(&output, latency_edits)?;
            run_recovery(&output, &recovery_counts)?;
            run_crash(&output, crash_edits)?;
        }
        "crash" => {
            let output = PathBuf::from(parse_value(&args, "--output")?);
            let crash_edits: u64 = args
                .windows(2)
                .find(|pair| pair[0] == "--crash-edits")
                .map(|pair| pair[1].parse())
                .transpose()?
                .unwrap_or(100);
            fs::create_dir_all(&output)?;
            run_crash(&output, crash_edits)?;
        }
        "crash-child" => {
            let root = PathBuf::from(parse_value(&args, "--root")?);
            let strategy = Strategy::parse(&parse_value(&args, "--strategy")?)?;
            let workload = workload_by_name(&parse_value(&args, "--workload")?)?;
            let edits: u64 = parse_value(&args, "--edits")?.parse()?;
            let phase = CrashPhase::parse(&parse_value(&args, "--phase")?)?;
            crash_child(&root, strategy, workload, edits, phase)?;
        }
        _ => return Err(format!("unknown command: {command}").into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_round_trip_preserves_sequence_and_length() {
        let bytes = checkpoint_bytes(7, b"abc");
        let root = temp_root("test-checkpoint").unwrap();
        let path = root.join("checkpoint");
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        assert_eq!(read_checkpoint(&path).unwrap(), (7, b"abc".to_vec()));
        remove_dir(&root).unwrap();
    }

    #[test]
    fn recovery_ignores_an_incomplete_trailing_journal_record() {
        let root = temp_root("test-journal").unwrap();
        let mut journal = File::create(root.join("journal")).unwrap();
        journal.write_all(&1u64.to_le_bytes()).unwrap();
        journal.write_all(&3u64.to_le_bytes()).unwrap();
        journal.write_all(b"abc").unwrap();
        journal.write_all(&2u64.to_le_bytes()).unwrap();
        journal.write_all(&5u64.to_le_bytes()).unwrap();
        journal.write_all(b"xy").unwrap();
        drop(journal);
        assert_eq!(recover(Strategy::PageCache, &root).unwrap(), (1, 3));
        remove_dir(&root).unwrap();
    }

    #[test]
    fn latency_scaling_limits_large_checkpoint_workloads() {
        assert_eq!(latency_edits_for(WORKLOADS[0], 1_000), 1_000);
        assert_eq!(latency_edits_for(WORKLOADS[3], 1_000), 500);
        assert_eq!(latency_edits_for(WORKLOADS[4], 1_000), 50);
    }
}
