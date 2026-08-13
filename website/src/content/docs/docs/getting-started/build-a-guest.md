---
title: Build a guest
description: The shape of a Rust guest component — bindings, the application loop, snapshots, and what a refused commit means.
sidebar:
  order: 4
---

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

Generate bindings from Instar's WIT world:

```rust
wit_bindgen::generate!({
    path: "../../crates/instar-kernel/wit",
    world: "kernel",
    generate_all,
});
```

## The application loop

Every guest follows the same broad rhythm:

```rust
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

```rust
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

Why this matters concretely: focus, hover, press, and scroll offsets are
host-owned state attached to the full key. Reusing key `7` for a different
control hands that control the previous one's host state; changing key `7` to
`8` for the same control throws that state away mid-interaction.

## Commit and handle refusal

A commit completes only after the host has decoded, validated, applied, laid
out, and lowered the snapshot into presentable state. A rejected commit leaves
the previous interface standing.

Treat errors as part of the application boundary:

| Refusal | What it means |
|---|---|
| `invalid-batch` | Malformed bytes or a semantically impossible UI. |
| `invalid-attachment` | A text-view attachment was refused before any tree work happened. |
| `commit-in-progress` | An earlier commit from this generation is still outstanding. Retry after it resolves. |
| `stale-generation` | This guest generation has already been superseded. |
| `host-unavailable` | The native owner disappeared while the guest waited. |

The full taxonomy, including the nested attachment cases, is in the
[error reference](/docs/reference/errors).

## Keep the guest boundary narrow

A guest may depend on `instar-ui-protocol` and optionally `instar-sdk`. It must
not link the native host, layout engine, renderer, window layer, or shell.
Repository tests enforce that rule as an allowlist — a guest that reaches for a
native crate fails the build, not review.

Next: [UI vocabulary](/docs/reference/ui-vocabulary) and the
[WIT contract](/docs/reference/wit).
