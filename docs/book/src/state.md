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

State is typed and quota-limited. Administrative commands inspect counts and
logical bytes without printing values:

```bash
youth state inspect --app-id dev.saman.tally --state-dir .youth/state
youth state verify --app-id dev.saman.tally --state-dir .youth/state
```

Runtime opening fails closed on corruption or usage mismatch. Offline usage
repair requires confirmation and an explicit nonexistent backup destination.

## Recommended explicit persistence pattern

For a model with several fields, keep one application-owned canonical model
and put its translation to typed keys behind `load` and `save` functions. The
view and event handler should not independently interpret storage keys.

```rust
fn load(state: StateReader) -> Result<Model> {
    let mode = state.text("mode")?.unwrap_or_else(|| "ready".into());
    let value = state.integer("value")?.unwrap_or(0);
    Model::from_stored(mode, value)
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
