# Youth — Developer Preview 0

**Status:** Active  
**Application protocol:** `youth:app@0.0.2`  
**State protocol:** `youth:state@0.0.1`

Developer Preview 0 proves that a developer can create, understand, test,
rebuild, and run a Youth application outside this repository:

```text
youth doctor
youth new tally --id dev.saman.tally
cd tally
youth check
youth dev
youth test
youth build --release
```

No new runtime behavior is in scope. The milestone packages the capabilities
already proved by Milestones 0 and 1 into an honest external workflow.

## Definition of done

The generated Tally application:

- has no filesystem dependency on a Youth checkout;
- does not expose generated WIT modules, numeric node IDs, revisions, event
  acknowledgements, raw patches, WASI imports, or component export plumbing;
- preserves durable state across `youth dev` restarts;
- is testable through semantic `.youth-test` files without a desktop;
- produces a validated component at `dist/<app-id>.wasm`; and
- passes the supported workflow on Linux, Windows, and macOS.

The first external Tally is assembled manually in a separate sibling
repository. Its proven structure becomes the only initial `youth new`
template.

## Project contract

`Youth.toml` is developer-owned and language-neutral:

```toml
[app]
id = "dev.saman.tally"
name = "Tally"
protocol = "0.0.2"

[build]
language = "rust"
package = "tally"
target = "wasm32-wasip2"

[development]
state = ".youth/state"
```

`Youth.lock` is generated, committed, and machine-owned:

```toml
lock-version = 1
protocol = "0.0.2"
sdk-source = "https://github.com/samanshaiza004/youth"
sdk-revision = "<exact-40-character-commit>"
wit-sha256 = "<lowercase-sha256>"
cli-version = "0.0.1"
template-version = 1
```

`youth check` requires the manifest, lock, SDK dependency, Cargo lock, vendored
WIT, running CLI, and embedded template to agree. DP0 provides no automatic
lock upgrade.

The WIT digest visits every regular file below `wit/youth` in sorted,
normalized relative-path order. Each file contributes a big-endian `u32` path
length, the UTF-8 path, a big-endian `u64` content length, and its exact bytes.
Paths are relative to `wit/youth` itself; symlinks are invalid.

The SDK's internal WIT is the sole source for Rust bindings and export
plumbing. The project-vendored WIT is an inspectable, language-neutral contract
snapshot and is not another Rust binding source.

## SDK boundary

`youth-sdk` owns lifecycle adaptation, read-only view/resync calls, mutable
event calls, revisions, acknowledgements, wire errors, typed host state,
semantic tree construction, and supported patch construction. DP0 exposes
root, column box, text, and button nodes plus text, label, and enabled updates.

Named node IDs are app-global. For a name, hash the exact unnormalized UTF-8
bytes after `youth:node-id:v1\0` with unsigned wrapping FNV-1a 64 (offset
`14695981039346656037`, prime `1099511628211`), then calculate. The suffix
shown as `\0` is exactly two ASCII bytes, backslash (`0x5c`) and digit zero
(`0x30`); it is not a NUL byte.

```text
(hash & 0x7fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000
```

Anonymous IDs occupy the lower half in deterministic preorder from one.
Duplicate names and collisions are hard errors.

| Name | Decimal | Hex |
| --- | ---: | --- |
| `count` | `17798422533909140438` | `0xf700b2fe97f653d6` |
| `increment` | `15700045616422714228` | `0xd9e1c44e444dfb74` |
| `café` | `14607564819619782035` | `0xcab87ecf2aee1d93` |

## Command behavior

- `youth new` atomically writes the one proven Rust Tally template and refuses
  to overwrite an existing destination.
- `youth doctor` performs headless-safe toolchain, target, protocol, and state
  checks. `youth doctor --full` additionally presents a native smoke window.
- `youth check` validates the project contract, checks and builds the guest,
  enforces the import profile, and proves the component links with this host.
- `youth build [--release]` runs the same validation and atomically publishes a
  bare component with its size and SHA-256.
- `youth dev` retains the last valid child after build failure and closes and
  reopens the window only after a new component builds and validates. This is
  rebuild-and-restart, not hot reload.
- `youth test` runs lexical `tests/*.youth-test` files against the real headless
  runtime with isolated durable state.

Every `.youth-test` has exactly one explicit initial `mount`. Commands before
it and later explicit mounts fail. `restart` is valid only after mount; it
recreates the runtime against the same state and implicitly mounts it.

## Checkpoints

1. `dp0-gate-a-external-sdk` — the separate Tally uses only `youth-sdk`.
2. `dp0-gate-b-project-workflow` — project contract, doctor, check, and new.
3. `dp0-gate-c-dev-test-build` — restart loop, semantic tests, and artifacts.
4. `dp0-gate-d-developer-preview` — docs and cross-platform external-project
   evidence complete.

## Non-goals

Reactive diffing, new protocol nodes, row layout, keyboard input,
accessibility, multiple windows, true hot reload, application packaging,
application publishing, WIT registries, SDK auto-upgrades, non-Rust guests,
and prebuilt installers are deferred.
