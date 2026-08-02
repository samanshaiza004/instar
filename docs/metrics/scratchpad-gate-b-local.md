# Scratchpad Gate B local performance evidence

Date: 2026-08-02

This is reporting evidence, not a cross-platform latency threshold. The final
Gate B release requires hosted-platform results and a canonical component
manifest.

Environment:

- macOS Darwin 25.5.0, Apple arm64
- rustc 1.97.1 (`8bab26f4f`)
- Cargo 1.97.1 (`c980f4866`)
- optimized `release` test profile
- 1 MiB ASCII UTF-8 document, no BOM

Command:

```bash
cargo test --release -p youth-runtime --test text_document \
  one_mebibyte_document_opens_edits_saves_and_restarts -- --nocapture
```

Observed phases:

| Phase | Time |
| --- | ---: |
| Initial spawn + mount + snapshot-ready Editor | 787.511 ms |
| One-character host-local edit | 403.938 ms |
| Save request + exact comparison + replacement + completion turn | 199.645 ms |
| Fresh runtime spawn + mount from saved file | 424.979 ms |
| Whole scenario, including stop and assertions | 1.896 s |

The scenario wrote exactly 1,048,576 bytes, restarted, and recovered the exact
saved text. No individual measured operation crossed one second, so the Gate B
“no multi-second interactive operation” review passes on this machine. The
403.938 ms single edit is still conspicuous and remains performance evidence
for future editor-backend work; it is not described as production-quality
large-document latency.

The optimized native test link took several minutes and is deliberately
excluded from application latency. Criterion already records mutation,
host-local edit, presentation, accessibility, and snapshot work separately at
1 KiB, 64 KiB, and 1 MiB. CI thresholds remain deferred until normal variance
is measured on all supported hosts.
