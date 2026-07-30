# Durable state guide

Read state while constructing the view and read or write it while handling an
event:

```rust
let count = context.state().integer("count")?.unwrap_or(0);
context.state().set_integer("count", count + 1)?;
```

Missing state is normal; Tally treats a missing `count` as zero. State writes
and semantic updates commit as one host transaction. Restarting `youth dev`
keeps the project state root from `[development].state`.

State is typed and quota-limited. Logical usage for each committed entry is:

```text
UTF-8 key bytes + encoded value bytes + 32 bytes fixed overhead
```

Booleans use one encoded byte, integers use eight, and text and blobs use their
byte lengths. Default limits are 16 MiB total logical state, 16,384 keys, 256
bytes per key, 1 MiB per text or blob value, 1,024 valid write attempts per
turn, and 4,096 state calls per turn. Logical usage is not the SQLite file
size: pages, indices, journal files, unused allocation, and other database
storage do not consume application quota.

Administrative commands inspect counts and logical bytes without printing
values:

```bash
youth state inspect --app-id dev.saman.tally --state-dir .youth/state
youth state verify --app-id dev.saman.tally --state-dir .youth/state
```

Runtime opening fails closed on corruption or usage mismatch. Offline usage
repair requires confirmation and an explicit nonexistent backup destination.
It recalculates Youth's accounting metadata only. It does not reinterpret,
migrate, complete, or repair application-owned values. A partially present or
otherwise invalid application model remains an application error even when the
database and its quota metadata are structurally sound.

## Recommended explicit persistence pattern

For a model with several fields, keep one application-owned canonical model
and put its translation to typed keys behind `load` and `save` functions. The
view and event handler should not independently interpret storage keys.

```rust
fn load(state: StateReader) -> Result<Model> {
    let mode = state.text("mode")?;
    let value = state.integer("value")?;

    match (mode, value) {
        (None, None) => Ok(Model::initial()),
        (Some(mode), Some(value)) => Model::from_stored(&mode, value)
            .map_err(|error| Error::invalid_state().with_message(error.to_string())),
        _ => Err(Error::invalid_state()
            .with_message("model state is only partially present")),
    }
}

fn save(state: StateWriter, model: &Model) -> Result<()> {
    state.set_text("mode", model.mode_name())?;
    state.set_integer("value", model.value())?;
    Ok(())
}
```

Use these rules until more applications justify a typed-record abstraction:

- Treat an entirely missing model as the documented initial state.
- Validate every loaded combination and return `invalid-state` for impossible
  or partially present records; do not silently manufacture missing fields.
- Persist only canonical domain state. Derive display strings and other view
  data so restart and resync cannot reveal a second source of truth.
- Share formatting and derivation functions between `view` and `handle`.
- Skip `save` and return `Update::unchanged()` when a command does not change
  the model, avoiding unnecessary writes and quota usage.
- For optional multi-key values, either delete all member keys or write all of
  them in the same turn. Reject partial combinations while loading.
- Keep keys and serialization choices local to the application. Do not expose
  storage layout through node IDs, command IDs, or presentation structure.

This pattern is intentionally explicit. Youth does not yet choose between a
derived typed record, one encoded document, scoped keys, or generated state
bindings; those options have different migration, quota, and partial-update
semantics and need evidence from more than one application.

## Application schema evolution

Youth does not provide a platform migration service. Persisted key names,
value types, and encodings remain part of the application's durable format.
An incompatible application change requires an explicit application migration,
a documented state reset, or a new storage identity; it must not silently
reinterpret old values.

For a multi-field model, storing an application-owned schema marker such as
`model-schema-version = 1` can make incompatible state detectable. The marker
does not create a platform migration system: the application still owns its
format, validation, and any transition between versions. Todo is the current
reference for this pattern: it atomically converts its version-1 boolean
status keys to version-2 text status keys during the first accepted turn,
while filter and page remain ephemeral session state.
