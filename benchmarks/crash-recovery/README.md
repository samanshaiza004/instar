# Crash-recovery prototype

This is an isolated filesystem experiment. It does not use Instar production
crates and does not add a persistence API. Run it from the repository root:

```text
cargo run --release --manifest-path benchmarks/crash-recovery/Cargo.toml -- measure \
  --output benchmarks/crash-recovery/results/reference
```

The default run measures five payloads through six deliberately explicit
policies and replays 1,000, 10,000, and 100,000 one-byte edits. A shorter local
run can lower the counts:

```text
cargo run --release --manifest-path benchmarks/crash-recovery/Cargo.toml -- measure \
  --latency-edits 200 --recovery-edits 1000,10000 --output /tmp/instar-recovery
```

The output contains:

- `metadata.txt`: host, configuration, and the exact semantics of each mode;
- `events.csv`: one row per write, page-cache acceptance point, flush, and
  completed edit operation;
- `summary.csv`: p50/p95/p99 write, page-cache, flush, and operation latency;
- `recovery.csv`: replay time and recovered edit count;
- `crash.csv`: restart observations after a process crash before and after a
  flush.

The modes are intentionally named by both storage shape and acknowledgement
policy:

1. `checkpoint_atomic`: rewrite the whole checkpoint to a temporary file,
   data-flush it, rename it, and data-flush the directory on every edit;
2. `journal_append`: append framed edits and flush once at the end of the run;
3. `journal_checkpoint`: append edits, then checkpoint every 2,048 edits or
   1 MiB of pending journal data;
4. `page_cache`: append edits and never flush during the run;
5. `sync_per_edit`: append an edit and data-flush it before returning;
6. `sync_batch`: append edits and data-flush when 5 ms have elapsed or 64 KiB
   are pending, whichever comes first.

`journal_append` and `page_cache` deliberately share the same append path.
That makes the cost of the journal format visible independently from whether a
caller asks for a final flush. `sync_per_edit` is the strict journal
acknowledgement case; it is also the direct measurement of the per-edit flush
cost.

`write_return` is measured around the raw `File::write` calls and means the
write syscall returned. `page_cache_accepted` is recorded as a separate event
at that same boundary: POSIX does not expose a second observable timestamp
between a successful write return and acceptance into the kernel page cache.
`flush_return` surrounds `sync_data`, which is the explicit data-flush event.
For `checkpoint_atomic`, the temporary-file flush and directory flush are both
recorded.

The crash test uses an intentionally abrupt child-process abort. It exercises
process-crash recovery, not power-loss durability: bytes left in the OS page
cache can survive a process restart. Only the `after_flush` observations are
treated as having crossed the benchmark's explicit durability boundary.

The result directory is raw experiment output and should be kept with the
machine and filesystem description when comparing systems.
