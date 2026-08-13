# Build a guest

The included counter is the canonical working example. This page explains its
shape; use the source rather than copying fragments from a possibly stale page.

## Guest package

A Rust guest is a `cdylib` targeting `wasm32-wasip2`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.60.0"
instar-ui-protocol = { path = "../../crates/instar-ui-protocol" }
```

Generate bindings from Instar’s WIT world:

```rust,ignore
wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
    generate_all,
});
```

## The application loop

Every guest follows the same broad rhythm:

```rust,ignore
async fn run() -> Result<(), String> {
    let mut state = AppState::new();
    commit(snapshot(&state), &[]).await?;

    loop {
        let bytes = kernel_runtime::next_event().await?;
        let event = WireEvent::decode(&bytes)?;
        state.update(event);
        commit(snapshot(&state), &[]).await?;
    }
}
```

The second commit argument is the text-view attachment side table. Pass an
empty list when the snapshot has no `TextView` nodes; text-capable guests pass
borrowed host view resources there. The text interface is still experimental.

`next-event` is async. While waiting, the component is suspended and Instar
does not poll it on a timer.

## Build semantic snapshots

`instar-sdk` is an optional helper around the byte protocol. It manages child
counts and routes stable node keys to your own message type:

```rust,ignore
enum Msg { Increment, Reset }

let mut ui = instar_sdk::Ui::new();
ui.root(0, |ui| {
    ui.column(1, |ui| {
        ui.text(2, "Counter");
        ui.button(3, "Increment").on_activate(Msg::Increment);
        ui.button(4, "Reset").on_activate(Msg::Reset);
    });
});
let (batch, routes) = ui.finish();
```

The numeric IDs are application-owned identities, not tree positions. Keep an
ID stable while it represents the same semantic node. If an ID is retired and
reused, advance its generation through `NodeKey` in lower-level protocol code.

## Commit and handle refusal

A commit completes only after the host has decoded, validated, applied, laid
out, and lowered the snapshot into presentable state. A rejected commit leaves
the previous interface standing.

Treat errors as part of the application boundary:

- `invalid-batch`: malformed bytes or semantically impossible UI.
- `stale-generation`: this guest generation has already been superseded.
- `host-unavailable`: the native owner disappeared while the guest waited.
- text errors: invalid resource IDs, stale revisions, or attachment conflicts.

## Keep the guest boundary narrow

A guest may depend on `instar-ui-protocol` and optionally `instar-sdk`. It must
not link the native host, layout engine, renderer, window layer, or shell.
Repository tests enforce that rule as an allowlist.

Next: [UI vocabulary](../reference/ui-vocabulary.md) and
[WIT contract](../reference/wit.md).
