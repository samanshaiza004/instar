# Tests and gates

## Workspace suite

```sh
cargo test --workspace
```

The suite covers the runtime, byte protocol, retained UI, layout, interaction,
bridge behavior, rendering structure, pixel output, shell behavior, and crate
layering.

## Gate 0

```sh
cargo test -p instar-kernel --test gate0
```

This builds a real `wasm32-wasip2` component and verifies idle suspension,
wake-up, concurrent async progress, generation teardown, and clean shutdown.

## Lints and formatting

```sh
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
```

## Release profile compile

```sh
cargo build --locked --profile dist --package instar-shell --bin instar
```

Release CI repeats this on every supported OS, packages the binary, verifies
the archive, installs it in isolation, and executes both help forms. Failures
are labeled by configuration, build, package, install, execute, or attestation
stage.

## Manual gates

Native assistive-technology behavior has a platform checklist in
`docs/F4-SMOKE.md`. Automated AccessKit projection tests do not substitute for
that manual screen-reader acceptance pass.
