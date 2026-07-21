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

The generated project commits both `Youth.toml` and `Youth.lock`. Do not edit
the lock by hand. DP0 has no lock-upgrade command; a mismatch explains the
conflicting fields and asks you to regenerate or restore the locked input.
