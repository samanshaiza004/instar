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
