# Build from source

## Clone and build the host

```sh
git clone https://github.com/samanshaiza004/instar.git
cd instar
cargo build --locked --release --package instar-shell --bin instar
```

The binary is `target/release/instar` (`instar.exe` on Windows).

Install it through Cargo if desired:

```sh
cargo install --locked --path crates/instar-shell
```

The repository pins Rust 1.97.1 and the `wasm32-wasip2` target. Rustup reads
`rust-toolchain.toml` automatically.

## Build a guest

Guests are separate Cargo workspaces:

```sh
cargo build --release \
  --manifest-path guests/counter/Cargo.toml \
  --target wasm32-wasip2
```

This separation is intentional. A guest compiles for a different target and
may link only the wire/SDK boundary, never native host crates.

## Build a distribution archive

Install cargo-dist 0.32.0, then run the native target build:

```sh
dist build --artifacts=local --target "$(rustc -vV | sed -n 's/^host: //p')"
```

Artifacts are written under `target/distrib`. Release CI remains the authority
for the complete four-platform set, checksums, attestations, installation, and
execution proof.
