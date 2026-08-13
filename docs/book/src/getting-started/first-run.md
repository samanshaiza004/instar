# Your first run

The repository’s counter is the shortest complete Instar application. It has
an increment button, a reset button, and a button that deliberately traps so
you can see the host-owned failure surface.

## 1. Clone the source

```sh
git clone https://github.com/samanshaiza004/instar.git
cd instar
```

## 2. Build the guest component

```sh
cargo build --release \
  --manifest-path guests/counter/Cargo.toml \
  --target wasm32-wasip2
```

The component is written to:

```text
guests/counter/target/wasm32-wasip2/release/counter.wasm
```

## 3. Run it

With an installed host:

```sh
instar run guests/counter/target/wasm32-wasip2/release/counter.wasm
```

Or run the host from the checkout:

```sh
cargo run --release --package instar-shell --bin instar -- run \
  guests/counter/target/wasm32-wasip2/release/counter.wasm
```

Add `--debug` to report lifecycle events, accepted commits, and frame timings
on stderr:

```sh
instar run path/to/component.wasm --debug
```

## What to try

- Click **Click me** and watch the guest commit a new snapshot.
- Click **Reset**; it is disabled at zero, so the host refuses the interaction.
- Navigate controls with the keyboard.
- Attach the platform accessibility inspector or screen reader.
- Click **Crash on purpose**. The guest generation ends, but the native host
  keeps the window and presents an error the guest cannot overwrite.

## What just happened

The counter did not draw pixels and did not receive pointer coordinates. It
committed semantic nodes, suspended at `next-event`, received a click naming a
stable `NodeKey`, updated its state, and committed another full snapshot.
