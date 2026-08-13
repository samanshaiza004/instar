---
title: Requirements
description: What you need to run an Instar component, and what you additionally need to build one.
sidebar:
  order: 1
---

You need different tools depending on whether you only run a component or also
build one.

## To run a component

| Requirement | macOS | Linux | Windows |
|---|---|---|---|
| 64-bit host | Apple Silicon or Intel | x86_64 glibc | x86_64 |
| Native window session | required | X11 or Wayland desktop | required |
| Rust toolchain | no, for a packaged release | no, for a packaged release | no, for a packaged release |

Instar is a desktop host. A headless server can run the repository's tests, but
`instar run` needs a graphical session capable of creating a native window.

## To build the included examples

Install the pinned Rust toolchain from the repository:

```sh
rustup toolchain install 1.97.1
rustup target add wasm32-wasip2 --toolchain 1.97.1
```

Cloning the repository activates that toolchain through `rust-toolchain.toml`.
The guest target matters: native Instar is built for your host, while guest
components are built for `wasm32-wasip2`.

## Linux packages

Building the native shell from source may require the development packages for
the window system selected by `winit`. On Debian/Ubuntu, start with:

```sh
sudo apt-get install build-essential pkg-config libx11-dev libxkbcommon-dev \
  libwayland-dev libudev-dev
```

Package names vary by distribution. A prebuilt release avoids the native Rust
build but still requires a working desktop session and its runtime libraries.

## Disk and network

The host binary is small; the toolchain around it is not. A source build pulls
the Rust toolchain, the `wasm32-wasip2` target, and the workspace's dependency
tree — budget a few gigabytes for `target/` if you intend to build and test the
whole workspace rather than install a release.

Next: [Install Instar](/docs/getting-started/install).
