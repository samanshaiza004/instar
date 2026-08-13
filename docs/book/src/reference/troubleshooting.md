# Troubleshooting

## `instar: no command given`

Use the only command and pass a component:

```sh
instar run path/to/component.wasm
```

## `no such file`

Confirm the path. The included counter builds under its own guest workspace:

```text
guests/counter/target/wasm32-wasip2/release/counter.wasm
```

## The file is empty or is a directory

Pass the component `.wasm` file itself. Instar distinguishes empty files,
directories, missing paths, and other read failures in its error message.

## The component does not implement the world

Build a Component Model component targeting `wasm32-wasip2` and generate
bindings from the repository’s current `kernel` world. A core Wasm module or a
component built against a different protocol will not load correctly.

## Protocol version mismatch

Rebuild the guest from the same checkout as the host. The repository build
scripts track WIT and protocol sources so normal builds should not retain stale
embedded guests.

## No window appears on Linux

Run inside a graphical session and check `DISPLAY` or `WAYLAND_DISPLAY`.
Headless CI can run package tests but cannot present the native application
without a display server.

## Installed command is not found

Add Cargo’s bin directory to `PATH`:

```sh
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
```

## Installer says no release exists

No tagged binary release has been published yet. Follow
[Build from source](../development/build-from-source.md). The website installer
will begin delegating to cargo-dist automatically after the first release.

## Get more diagnostics

```sh
instar run component.wasm --debug
```

When filing an issue, include the host OS, architecture, Instar commit/tag,
guest build commit, exact command, stderr, and whether a native window appeared.
