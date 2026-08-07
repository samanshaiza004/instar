# Toolchain

Pinned tuple for Instar Phase 1, chosen by head-to-head comparison rather than
defaulting to whatever the former Youth codebase happened to have pinned.
Research done 2026-08-06; re-verify if this document is more than a few
months old before trusting it.

## Decision: Wasmtime 47.0.3 (not 46.0.1, not 46.0.2)

The former Youth codebase pinned `wasmtime = "46"` / `wasmtime-wasi = "46"`
(exactly `46.0.1` per its `Cargo.lock`), with Component Model async
explicitly **disabled** (`wasm_component_model_async(false)` in
`crates/youth-runtime/src/engine.rs`). Since Phase 1's entire premise is
proving that async suspend/wake works, staying on the existing pin by
default would be begging the question. Two candidates were compared instead:

| | `46.0.2` | `47.0.3` |
|---|---|---|
| Released | 2026-07-31 | 2026-07-31 |
| Security fixes ([GHSA-hgjw-h833-99q9](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-hgjw-h833-99q9), [GHSA-2hw9-mc66-jc2q](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-2hw9-mc66-jc2q)) | yes | yes |
| Async-delivered write-closed events for futures fixed (#13914) | **no** | yes (landed in 47.0.2) |
| Call hooks with yields + concurrent execution fixed (#13871) | **no** | yes (landed in 47.0.2) |
| `Accessor::poll_ready_for_concurrent_call` (concurrency backpressure API) | **no** | yes (landed in 47.0.0) |

Verified directly against the GitHub releases API
(`gh api repos/bytecodealliance/wasmtime/releases/tags/<tag>`): `46.0.2`'s
release body contains *only* the two security-advisory fixes, backported
from the `47.x` line onto `46.x` with nothing else. `47.0.3` carries those
same two fixes plus everything `47.0.0`–`47.0.2` shipped on top, including
the two async-specific fixes above — both of which are directly load-bearing
for Gate 0 (the write-closed-future-events fix affects `future<T>`
completion delivery; the call-hook/yield fix affects exactly the
suspend-while-yielded state Gate 0's `next-event` spike depends on).

**Decision: pin `wasmtime = "47.0.3"` and `wasmtime-wasi = "47.0.3"`** when
`instar-kernel` is scaffolded in WP2. `46.0.2` is strictly worse for this
project's purposes and was rejected — it offers no reason to prefer it over
`47.0.3` besides "smaller version bump," which doesn't matter here since
nothing outside the new `instar-kernel` crate depends on Wasmtime yet.

## Async enablement

Host-side, the modern equivalent of `wasm_component_model_async(true)` is
documented in the `wasmtime` CLI as two separate flags — confirmed against
`component-model.bytecodealliance.org`'s Wasmtime page:

- `-Sp3` enables WASI 0.3 imports.
- `-W component-model-async=y` enables the Component Model's async
  primitives (`async func`, `stream<T>`, `future<T>`).

The embedding API equivalent (what `instar-kernel` actually uses, not the
CLI) is `wasmtime::Config::wasm_component_model_async(true)` — already
present in `youth-runtime/src/engine.rs`, just set to `false`. WP2 flips it.

## Decision: wit-bindgen 0.60.0 (not 0.51.0)

The former Youth codebase pins `wit-bindgen = "0.51.0"` (used by
`youth-sdk`). Building the WP1.3 empty-component fixture with that pin
surfaced a direct signal: `cargo build` reported `Adding wit-bindgen v0.51.0
(available: v0.60.0)`. Checked every release body from `0.52.0` through
`0.60.0` (`gh api repos/bytecodealliance/wit-bindgen/releases/tags/<tag>`)
for async-relevant changes rather than assuming the newer version is safe
to skip. Found real, named fixes directly on Gate 0's test surface:

- **`0.59.0`** — "async: remove inter-task wakeup stream from waitable set
  before cancelling" (#1638). This is exactly WP3.4's cancellation-mechanism
  proof — a bug here would show up as leaked or hanging cancellation, which
  is precisely what that gate checks for.
- **`0.58.0`** — "fix: async lifted exports with direct results" (#1614).
  Directly on the `next-event: async func() -> result<...>` export path the
  Gate 0 spike's whole WIT world depends on.
- **`0.52.0`** — "fix(core): async import emission" (#1455) — a general
  async-import correctness fix, this codebase's very first releases-worth of
  async bug-fixing after `0.51.0`.
- `0.56.0`–`0.60.0` also show ongoing async-stream/wasip3-ABI refinement
  (stream read/write length limits, `no_std`/`alloc` support for
  `async-spawn`, evolving wasip3 async ABI for tasks) — signs of an actively
  maturing feature, not a settled one, which argues for staying current
  rather than picking an early snapshot.

**Decision: pin `wit-bindgen = "0.60.0"`** for `instar-sdk`/`instar-kernel`
going forward (applied when those crates are touched in WP2/WP8). Given
named fixes on exactly the cancellation and async-export paths Gate 0
tests, staying on `0.51.0` would mean testing against a version with known,
already-fixed bugs in the area under test — that would make a "no-go"
result from Gate 0 ambiguous (real toolchain limitation vs. an already-fixed
bug). `0.60.0` removes that ambiguity for free.

## Nightly Rust: not required, based on available evidence

The user's original brief flagged "the official guide still requires
nightly Rust" as a risk to check. Current `wit-bindgen` documentation states
`wasm32-wasip2` has been natively supported by *stable* Rust since 1.82 —
no mention of nightly being required, for either the target itself or async
generation specifically. The pinned toolchain here
(`rust-toolchain.toml`: `channel = "1.97.1"`) is already stable and well
past 1.82. **No toolchain channel change is planned** unless WP2/WP3's
empirical spike hits something that genuinely requires nightly (e.g. an
unstable compiler feature gate) — if that happens, it gets recorded here
with the specific error that forced it, not adopted preemptively.

## WP1.3: known-good empty component fixture — built and validated

Built a minimal throwaway component (`package instar:toolchain-check@0.1.0;
world empty { export run: func(); }`, one no-op export, no async yet — this
step validates the *basic* pipeline before WP2/WP3 add async) targeting
`wasm32-wasip2` on this machine's pinned Rust (`1.97.1`, stable) and locally
installed `wasm-tools 1.255.0` — already exactly the version pinned above.
Verified against **both** candidate `wit-bindgen` versions:

- `0.51.0`: builds clean, `wasm-tools validate` passes, `wasm-tools
  component wit` round-trips the world correctly.
- `0.60.0`: same — builds clean, validates, round-trips correctly.

Confirms the toolchain choices above don't break basic component
generation before async is even in the picture. No nightly Rust was
needed for either version, consistent with the "nightly not required"
finding above.

## wasm-tools: pin to 1.255.0

Latest release as of this research (`gh api
repos/bytecodealliance/wasm-tools/releases/latest`): `v1.255.0`,
2026-07-30. **Pinned and in use**: `.github/workflows/gate0.yml` installs
`wasm-tools@1.255.0` explicitly.

The Youth-era `ci.yml`, which installed it unpinned, has been deleted rather
than repaired — it built workspace members that no longer exist, so it could
not pass in any form. `gate0.yml` is the whole pipeline until WP9 writes
Instar-native CI.

## Summary of the pinned tuple

| Component | Version | Status |
|---|---|---|
| Rust toolchain | `1.97.1`, stable | unchanged, already sufficient |
| Target | `wasm32-wasip2` | unchanged |
| `wasmtime` / `wasmtime-wasi` | `47.0.3` | **changed from 46.0.1** — applied when `instar-kernel` is scaffolded (WP2) |
| `wit-bindgen` | `0.60.0` | **changed from 0.51.0** — applied to `instar-sdk`/`instar-kernel` in WP2/WP8 |
| `wasm-tools` (CI) | `1.255.0` | pinned in `gate0.yml` |
