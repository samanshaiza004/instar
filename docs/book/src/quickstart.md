# Quickstart

Developer Preview 0 builds a native Youth application from a standalone Rust
project. Install the CLI from the immutable preview tag, diagnose the machine,
and generate Tally:

```bash
cargo install youth-cli --git https://github.com/samanshaiza004/youth --tag developer-preview-0
youth doctor
youth new tally --id dev.saman.tally
cd tally
youth check
youth test
youth build --release
youth dev
```

`youth doctor` is safe in a headless session. Use `youth doctor --full` when a
native display is available and you want to verify window presentation.

`--tag developer-preview-0` pins the exact, frozen DP0 workflow this page
documents. Youth has no tagged CLI release newer than that yet (no `dist`
release has been cut — see [docs/DISTRIBUTION.md](../../DISTRIBUTION.md)),
so to try current platform features not covered by this page — the Editor
capability, modifier-aware shortcuts, `dist`-packaged install scripts — drop
`--tag developer-preview-0` to build `cargo-install`'s default (the `master`
branch tip) instead.

The generated project commits both `Youth.toml` and `Youth.lock`. Do not edit
the lock by hand. DP0 has no lock-upgrade command; a mismatch explains the
conflicting fields and asks you to regenerate or restore the locked input.

## Utility Suite releases

The completed external applications are maintained as sibling repositories:

- [Calculator](https://github.com/samanshaiza004/youth-calculator) probes layout,
  focus, keyboard commands, and formatting.
- [Timer](https://github.com/samanshaiza004/youth-timer) probes clocks,
  schedules, elapsed delivery, and recovery.
- [Todo](https://github.com/samanshaiza004/youth-todo) probes bounded dynamic
  collections, stable identities, structural updates, migration, and view
  convergence.

Todo’s release tag is `utility-todo-gate-d-release`. Its findings and metrics
are application evidence; they do not add list nodes, structured state, state
enumeration, or automatic tree diffing to Youth.
