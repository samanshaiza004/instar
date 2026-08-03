# Quickstart

Create a native Youth application from a standalone Rust project. Install the
CLI, diagnose the machine, and generate Tally:

```bash
cargo install youth-cli --git https://github.com/samanshaiza004/youth
youth doctor
youth new tally
cd tally
youth check
youth test
youth build --release
youth dev
```

`youth doctor` is safe in a headless session. Use `youth doctor --full` when a
native display is available and you want to verify window presentation.

The project name supplies a deterministic default ID: `tally` becomes
`dev.youth.tally`. An app ID is a stable host identity used to select durable
state and other app-owned resources; it is not a UI label or a filesystem
path. Use `--id`, for example `youth new tally --id dev.saman.tally`, when you
need a different identity. Keep it stable after sharing or shipping an app.

New projects use the current `youth:app@0.0.9` contract. This protocol adds a
wire-node `grow` field for forthcoming responsive layout; current host layout
behavior is unchanged. `Youth.toml` is the
developer-owned project description; `Youth.lock` records the exact SDK Git
revision, protocol, vendored WIT hash, CLI version, and template version.
Commit both files. Rust bindings come from the pinned SDK; the vendored WIT
is an inspectable contract snapshot.

The generated project commits both `Youth.toml` and `Youth.lock`. Do not edit
the lock by hand. Youth does not silently upgrade it: if the manifest, SDK,
protocol, CLI, or WIT snapshot no longer agree, `youth check` reports the
conflicting fields and asks you to regenerate or restore the locked input.

`youth check` validates the manifest, lock, WIT snapshot, component imports,
and host compatibility. `youth test` runs the project's `.youth-test` files
through the real headless runtime. `youth build --release` writes a validated
component to `dist/<app-id>.wasm`; `youth dev` rebuilds and restarts it while
retaining the project's state directory.

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
