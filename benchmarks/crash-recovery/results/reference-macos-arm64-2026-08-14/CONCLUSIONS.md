# Reference run conclusions

Machine: Apple MacBook Air, `aarch64`, Darwin 25.5.0, APFS filesystem. This
is one development/reference system, not a cross-machine claim.

## Answer to the 5 ms question

No strict recovery acknowledgement fits on the <=5 ms typing critical path in
this run. The strict `sync_per_edit` journal measured, for a one-byte ASCII
edit, p50 operation latency `7.82 ms`, p95 `24.51 ms`, and p99 `44.93 ms`.
Its data-flush p50 was `7.36 ms`, p95 `21.15 ms`, and p99 `44.02 ms`. The
multi-byte and IME payloads were similar. Atomic whole-checkpoint rewrite was
slower still: ASCII p50 operation latency `31.17 ms`, p95 `93.97 ms`, and p99
`204.69 ms`.

The write-only path is much cheaper. For ASCII, append-journal write p50 was
`5.42 us`; page-cache-only operation p50 was `6.08 us`. Those numbers mean
the write syscall returned and the kernel accepted the bytes into its page
cache. They do not mean the bytes survived power loss.

## Realistic policy

An asynchronous bounded-loss policy is the practical choice exposed by this
prototype:

- append the edit to a journal and return after page-cache acceptance;
- flush in the background every 5 ms or 64 KiB, whichever comes first;
- periodically checkpoint the journal, here every 2,048 edits or 1 MiB;
- expose the policy as bounded loss on process or machine failure, rather than
  claiming strict durability for each keystroke.

The batch policy kept ordinary ASCII edits cheap: p50 `6.83 us`, p95
`82.71 us`, and p99 `5.69 ms`; only 10 of 1,000 edits triggered a flush in
that sample. It is not a strict <=5 ms guarantee: 1 KiB edits reached p99
`15.22 ms`, and 100 KiB pastes reached p50 `11.54 ms` and p95 `41.71 ms`
because the byte threshold forces a flush. The threshold should therefore be
treated as a scheduling policy, not as an acknowledgement deadline.

The periodic-checkpoint mode reduced 100k one-byte recovery to `0.694 ms`,
versus `3.022 ms` for an uncheckpointed journal, while writing `16.38 MB`
instead of the journal's `~5.12 MB` for the 50-paste latency sample. Its
100 KiB paste p95 operation latency was `107.55 ms`, so checkpoint work must
stay off the typing path.

## Recovery and crash observations

At 100k one-byte edits, recovery measured:

| mode | recovery |
| --- | ---: |
| atomic checkpoint | `0.129 ms` |
| append journal | `3.022 ms` |
| journal + periodic checkpoint | `0.694 ms` |
| page-cache journal | `4.569 ms` |
| per-edit sync journal | `12.931 ms` |
| batched sync journal | `12.116 ms` |

The crash child was killed after 100 edits. The atomic checkpoint recovered
99 edits after the write-only crash and 100 after the complete publish flush.
Every journal mode observed 100 edits after both crash points on this system.
That is expected for a process crash while the OS page cache remains alive;
the pre-flush result is not evidence of power-loss durability. The benchmark
does not claim to emulate sudden power removal.

The raw event file keeps separate `write_return`, `page_cache_accepted`, and
`flush_return` rows. POSIX provides no second observable timestamp between a
successful write return and page-cache acceptance, so those two events have
the same measured boundary by design.
