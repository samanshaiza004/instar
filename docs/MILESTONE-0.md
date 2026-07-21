# Youth — Milestone 0 Specification: Headless Protocol Core

- Status: Ready to implement
- Protocol version: `0.0.1`
- Expected scope: Small enough to finish before any renderer work
- Primary language: Rust
- Guest artifact: WebAssembly Component targeting a custom Youth WIT world
- Visible UI: None

## 1. Milestone thesis

Milestone 0 proves this statement:

> One untrusted, architecture-independent Wasm component can be loaded by a
> native Youth host, mounted through a typed WIT contract, receive an ordered
> semantic event, return a revisioned UI patch batch, and update a validated
> retained semantic tree without direct access to the operating system.

The output is not a window. The output is a trustworthy headless application
machine.

Example:

```text
load counter.wasm
→ validate component contract
→ instantiate in isolated store
→ call mount()
→ validate initial tree revision 0
→ enqueue activate(button-4)
→ call handle()
→ validate patch batch
→ atomically update tree to revision 1
→ print canonical tree
```

Expected final tree:

```text
root #1
└── box #2
    ├── text #3 "Count: 1"
    └── button #4 "Increment"
```

Milestone 0 is successful when the exact same `.wasm` artifact produces the
same canonical tree on Linux, Windows, and macOS.

## 2. Why Rust is the correct language

**Host: Rust.** Use stable Rust with the 2024 edition. Rust is optimal because:

- Wasmtime's Rust embedding API has first-class Component Model bindings
  through `wasmtime::component::bindgen!`.
- Host-defined Component Model resources are typed and unforgeable.
- The likely renderer stack—Masonry, Vello, Parley, AccessKit, and winit—is
  already Rust-native.
- Rust avoids introducing a garbage-collected runtime into Youth's native host.
- Rust's ownership model suits store/resource lifetimes.
- Memory safety matters because Youth loads untrusted components and validates
  attacker-controlled trees and patch batches.
- Cargo workspaces make the protocol engine, runtime, CLI, guest fixtures, and
  tests easy to separate.

Wasmtime's generated bindings map WIT worlds into typed Rust host APIs.
Wasmtime also provides resource limits, fuel, epoch interruption, and
hostcall-transfer limits for containing guests.

**Protocol: WIT.** WIT is the permanent language-neutral boundary. A WIT world
precisely states what a component exports and imports. A component cannot
access a host service that is absent from its imported interfaces.

**First guest: Rust.** Use Rust for the first fixture only to reduce
simultaneous unknowns. Build it with:

- `wit-bindgen`
- native `wasm32-wasip2`
- `cdylib`

Current official Component Model guidance recommends native Rust tooling
rather than beginning a new project around `cargo-component`, which is being
deprecated. A Rust `cdylib` can implement a custom WIT world and compile
directly into a component with `cargo build --target wasm32-wasip2`.

Rust is not Youth's required application language. It is merely the fastest
way to prove the first host/guest contract.

## 3. Non-goals

Milestone 0 must not include: windows, rendering, winit, Masonry, Vello,
styling, layout calculation, SQLite, durable application state, filesystem
capabilities, effect outbox, hot reload, package format, component
composition, custom surfaces, accessibility adapters, SDK view diffing,
WASI 0.3 async components, multiple simultaneous applications, state
migrations, app installation.

Do not "prepare" these systems through speculative abstractions. Milestone 0
builds only the permanent execution spine required before them.

## 4. Repository structure

```text
youth/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── deny.toml
├── README.md
├── docs/
│   ├── MASTER-REFERENCE.md
│   └── MILESTONE-0.md
├── wit/
│   └── youth-app/
│       └── youth-app.wit
├── crates/
│   ├── youth-tree/
│   ├── youth-runtime/
│   └── youth-cli/
├── guests/
│   └── counter/
├── test-components/
│   ├── trap-on-mount/
│   ├── trap-on-handle/
│   ├── invalid-snapshot/
│   ├── invalid-patch/
│   ├── infinite-loop/
│   └── memory-bomb/
└── tests/
    ├── fixtures/
    └── scripts/
```

### Crate responsibilities

**`youth-tree`** — Pure retained semantic-tree engine. Contains: internal node
types, tree revisions, patch operations, validation, atomic batch application,
canonical tree snapshots, deterministic inspection output.

It must not depend on: Wasmtime, WASI, Tokio, rendering libraries, platform
APIs. Why: the semantic-tree correctness core should be testable and reusable
without executing Wasm.

**`youth-runtime`** — Owns: Wasmtime engine, component compilation, WIT
bindings, linker, WASI context, store, guest instance, resource limits, fuel,
epoch deadlines, serialized application worker, event sequencing, conversion
between WIT values and internal tree values, lifecycle state.

**`youth-cli`** — Thin inspection and test surface. Commands:

```text
youth validate <component.wasm>
youth mount <component.wasm>
youth activate <component.wasm> --node 4
youth script <component.wasm> <events.json>
youth inspect <component.wasm>
```

JSON is acceptable for CLI fixtures and inspection output. JSON is not the
application wire protocol.

**`guests/counter`** — The first valid Youth component. It exports the custom
Youth application world and maintains an in-memory count for the duration of
the instance. Its memory is intentionally disposable. Restarting the component
resets it.

## 5. Dependency policy

Use current stable releases at project creation, then commit `Cargo.lock`.

Initial dependencies:

```text
youth-tree:    thiserror, serde (inspection output only), proptest (dev)
youth-runtime: wasmtime, wasmtime-wasi, tokio, thiserror, tracing, tracing-subscriber
youth-cli:     clap, serde_json, tokio, tracing-subscriber
counter guest: wit-bindgen
```

Wasmtime and `wasmtime-wasi` must use the same release line. Pin the selected
release through the workspace lockfile and upgrade intentionally rather than
continuously following `main`.

**Toolchain policy.** Create an exact `rust-toolchain.toml` after the first
successful build. Do not leave CI on a floating toolchain indefinitely.
Require: `rustfmt`, `clippy`, `wasm32-wasip2`. Install `wasm-tools` for
component inspection (`wasm-tools component wit` inspects the interfaces
encoded in a component).

## 6. Canonical WIT contract

Create `wit/youth-app/youth-app.wit`:

```wit
package youth:app@0.0.1;

interface ui {
    type node-id = u64;
    type tree-revision = u64;
    type event-sequence = u64;

    record box-data {
        enabled: bool,
    }

    record text-data {
        value: string,
    }

    record button-data {
        label: string,
        enabled: bool,
    }

    variant node-data {
        root,
        box(box-data),
        text(text-data),
        button(button-data),
    }

    record node {
        id: node-id,
        data: node-data,
        children: list<node-id>,
    }

    record tree-snapshot {
        revision: tree-revision,
        root: node-id,
        nodes: list<node>,
    }

    record create-node {
        value: node,
    }

    record delete-node {
        id: node-id,
    }

    record set-text {
        id: node-id,
        value: string,
    }

    record set-label {
        id: node-id,
        value: string,
    }

    record set-enabled {
        id: node-id,
        value: bool,
    }

    record insert-child {
        parent: node-id,
        index: u32,
        child: node-id,
    }

    record remove-child {
        parent: node-id,
        index: u32,
        expected-child: node-id,
    }

    record move-child {
        parent: node-id,
        from-index: u32,
        to-index: u32,
        expected-child: node-id,
    }

    variant patch {
        create(create-node),
        delete(delete-node),
        set-text(set-text),
        set-label(set-label),
        set-enabled(set-enabled),
        insert-child(insert-child),
        remove-child(remove-child),
        move-child(move-child),
    }

    variant event-kind {
        activate(node-id),
    }

    record event {
        sequence: event-sequence,
        kind: event-kind,
    }

    record event-batch {
        tree-revision: tree-revision,
        events: list<event>,
    }

    record patch-batch {
        base-tree-revision: tree-revision,
        next-tree-revision: tree-revision,
        processed-through: event-sequence,
        patches: list<patch>,
    }

    enum app-error-code {
        invalid-state,
        rejected-event,
        internal,
    }

    record app-error {
        code: app-error-code,
        message: option<string>,
    }
}

interface lifecycle {
    use ui.{
        tree-snapshot,
        event-batch,
        patch-batch,
        app-error,
    };

    mount: func() -> result<tree-snapshot, app-error>;
    handle: func(events: event-batch) -> result<patch-batch, app-error>;
    resync: func() -> result<tree-snapshot, app-error>;
}

world application {
    export lifecycle;
}
```

This contract is explicitly disposable before `0.1`. Its purpose is to expose
real implementation pressure, not predict the final protocol. WIT defines
contracts but not behavioral semantics, so every rule below must also exist as
tests and written conformance requirements.

## 7. Internal protocol boundary

Do not use generated Wasmtime/WIT structs as Youth's retained-tree
representation. The runtime must convert:

```text
generated WIT values → validated Youth internal values → youth-tree
```

And for events:

```text
Youth internal event → generated WIT value → guest call
```

Why: generated binding types will change as WIT and tooling change. The tree
engine must not be coupled to one binding generator's output.

Implement explicit conversion modules:

```text
youth-runtime/src/wire/from_guest.rs
youth-runtime/src/wire/to_guest.rs
```

No broad `From` implementation should silently skip validation. Use fallible
conversions:

```rust
TryFrom<wire::TreeSnapshot> for youth_tree::TreeSnapshot
TryFrom<wire::PatchBatch> for youth_tree::PatchBatch
```

## 8. Retained-tree invariants

`youth-tree` must enforce all of these.

**Identity**

- Node ID `0` is reserved and invalid.
- Every node ID is unique.
- Exactly one root exists.
- The declared root ID must exist.
- The root node must use `node-data.root`.
- No non-root node may use `node-data.root`.

**Ownership**

- Every non-root node has exactly one parent.
- The root has no parent.
- A child cannot appear twice under one parent.
- A node cannot appear under multiple parents.
- The graph must be acyclic.
- Every node must be reachable from the root.
- Orphan nodes are invalid.

**Node shape**

- Root and box nodes may contain children.
- Text and button nodes must have no children.
- `set-text` is valid only for text nodes.
- `set-label` is valid only for button nodes.
- `set-enabled` is valid only for box and button nodes.

**Patch operations**

- `create` introduces a detached node with a fresh ID.
- A newly created node may be attached later in the same batch.
- `delete` is valid only for a detached leaf node.
- Removing a child must include the expected child ID.
- Moving a child must include the expected child ID.
- Insertion indexes may equal the current child count.
- Removal and move indexes must already exist.
- Patches are evaluated in listed order against a staged tree.

**Revision rules**

- The initial `mount()` snapshot must use revision `0`.
- A patch batch's base revision must equal the host tree revision.
- A non-empty batch must use `next = base + 1`.
- An empty batch must use `next = base`.
- Revisions may not decrease.
- Revisions may not skip.
- A rejected batch does not change the host revision.

**Event rules**

- The host assigns event sequence numbers.
- Sequence numbers are strictly increasing.
- Events in a batch are ordered by sequence.
- `processed-through` must equal the last event processed.
- It may not exceed the highest event sent.
- `handle()` may not be called before `mount()`.
- `mount()` may not be called twice on one instance.

## 9. Atomic patch application

A patch batch must either apply completely or not at all. For Milestone 0,
use the simplest correct implementation:

1. Clone the current tree into a staging tree.
2. Apply every patch to the staging tree.
3. Validate the complete staged tree.
4. Replace the live tree only if all operations succeed.

```text
live tree revision 4
        ↓ clone
staging tree revision 4
        ↓ apply 12 operations
        ↓ validate
success → swap into live tree, revision 5
failure → discard staging tree, live tree stays revision 4
```

This is not the final large-tree performance strategy. It is the correct
Milestone 0 strategy because correctness is obvious, rollback is trivial,
tests are simple, and performance is irrelevant at current tree sizes. Do not
optimize this early.

## 10. Deterministic tree representation

Internally use deterministic collections:

```rust
BTreeMap<NodeId, Node>
BTreeMap<NodeId, NodeId> // child → parent
```

Children remain ordered vectors. Canonical snapshots must:

- sort nodes by numeric ID
- preserve child order
- normalize line endings
- exclude timestamps
- exclude memory addresses
- exclude hash-map iteration order

The same snapshot must serialize identically on Linux, Windows, and macOS.
This gives Youth stable fixtures and allows cross-platform hashes.

## 11. Runtime lifecycle

```rust
enum AppLifecycle {
    Loaded,
    Mounted,
    Faulted,
    Stopped,
}
```

- **`Loaded`** — component has compiled and instantiated but has not mounted.
  Allowed operation: `mount`.
- **`Mounted`** — initial tree is valid. Allowed operations: `handle`,
  `resync`, `stop`.
- **`Faulted`** — the component or protocol has entered an untrustworthy
  state. No further guest calls are allowed. The instance must be destroyed
  before restart.
- **`Stopped`** — resources have been released. No further calls are allowed.

**Fault rules.** The instance becomes `Faulted` after: guest trap, fuel
exhaustion, epoch interruption, memory-limit failure, malformed canonical ABI
value, invalid initial snapshot, structurally invalid patch batch, impossible
revision transition, guest claiming to process events it was not sent,
internal guest state diverging from host state in a way that cannot be
resynchronized.

Why poison the instance: guest linear memory may have mutated before the trap
or invalid result. Youth cannot assume the guest can safely continue.

A normal guest-returned `app-error` does **not** poison the instance. It
commits no tree change.

## 12. Host execution model

The public host API must be asynchronous even though guest turns are
synchronous.

```text
CLI / future UI thread
        ↓ async command
YouthAppHandle
        ↓ bounded channel
AppWorker
        ├── Store<HostState>
        ├── component instance
        ├── retained tree
        ├── lifecycle
        └── next event sequence
```

Exactly one worker owns: the Wasmtime `Store`, the component instance, the
retained tree, lifecycle state, sequence counter. A store should correspond
roughly to one main instance's lifetime.

Public API:

```rust
pub struct YouthAppHandle {
    command_tx: tokio::sync::mpsc::Sender<AppCommand>,
}

impl YouthAppHandle {
    pub async fn mount(&self) -> Result<TreeSnapshot, RuntimeError>;
    pub async fn activate(&self, node: NodeId) -> Result<TurnReceipt, RuntimeError>;
    pub async fn resync(&self) -> Result<TreeSnapshot, RuntimeError>;
    pub async fn inspect(&self) -> Result<AppInspection, RuntimeError>;
    pub async fn stop(&self) -> Result<(), RuntimeError>;
}
```

For Milestone 0, a dedicated blocking worker thread per app is acceptable.
The asynchronous handle communicates through bounded Tokio channels and
oneshot replies. This guarantees: no UI-thread blocking later, strict call
serialization, no concurrent Store access, no guest reentrancy, natural
backpressure.

Do not expose `Store`, generated bindings, or component instances outside the
worker.

**Queue policy.** Initial command capacity: 64 commands. When full:
asynchronous callers wait, no events are dropped, no preview coalescing
exists yet. Event coalescing belongs to a later continuous-input milestone.

## 13. Wasmtime configuration

Create one shared `Engine` for the process. Create one `Store` per
application instance.

Enable: Component Model, Cranelift, fuel consumption, epoch interruption,
Wasm backtraces in development, store resource limits.

Do not enable experimental Component Model async, GC, threading, shared
memory, or memory64 for Milestone 0.

**Initial budgets** (configurable defaults):

```text
Maximum component file:       32 MiB
Maximum guest linear memory: 128 MiB
Maximum table elements:       1,000,000
Maximum nodes:               10,000
Maximum tree depth:              64
Maximum children per node:    4,096
Maximum patches per turn:    10,000
Maximum event batch:            256
Maximum ordinary text:       64 KiB
Maximum button label:         4 KiB
Maximum guest error message:  4 KiB
Maximum guest→host transfer:  8 MiB per call
```

**CPU limits** (initial call budgets):

```text
mount:  fuel 100,000,000; hard wall deadline 500 ms
handle: fuel  20,000,000; hard wall deadline 100 ms
resync: fuel 100,000,000; hard wall deadline 500 ms
```

These values are development defaults, not ABI promises. Record actual fuel
and elapsed time, then tune from evidence. Fuel exhaustion traps the guest;
epoch deadlines provide a wall-clock failsafe.

**Memory limits.** Implement a `ResourceLimiter`. Do not rely on it alone:
resource limiting covers guest instances, memories, and tables, but not every
host allocation. Youth must also validate list/string sizes and limit
guest-to-host transfer. Configure Wasmtime's component hostcall fuel for the
per-call transfer ceiling so canonical ABI lifting is bounded before the
runtime's structural wire validation runs.

## 14. WASI profile

The first Rust guest may import WASI Preview 2 services through its standard
library. Add WASIp2 to the linker, but build a closed context. Do not
inherit: command-line arguments, environment variables, host standard
input/output/error, host directories, host sockets, network access.

Provide only the minimum toolchain-required facilities, such as controlled
clocks or randomness where unavoidable. Use the official Wasmtime host
pattern (`wasmtime_wasi::p2::add_to_linker_sync`, a `WasiCtx`, and a
`ResourceTable`) while refusing ambient host inheritance.

The first guest should not use: `println!`, `eprintln!`, `std::fs`,
`std::net`, `std::env`. Logging is deferred until Youth defines an explicit
diagnostic import.

Linking Rust's standard library on `wasm32-wasip2` imports `wasi:cli/*`,
`wasi:io/*`, and `wasi:clocks/*` regardless of whether the guest calls
them. Importing an interface grants no authority — the host decides what
it returns, and Youth's context is closed — but the import list is a real
compatibility surface, so it is budgeted and enforced as an allowlist.
See [GUEST-PROFILE.md](GUEST-PROFILE.md).

## 15. Runtime error model

```rust
pub enum RuntimeError {
    ComponentTooLarge,
    InvalidComponent,
    UnsupportedWorld,
    LinkFailure,
    InstantiationFailure,
    InvalidLifecycle,
    GuestRejected,
    GuestTrap,
    FuelExhausted,
    DeadlineExceeded,
    MemoryLimitExceeded,
    TransferLimitExceeded,
    InvalidSnapshot,
    InvalidPatchBatch,
    RevisionMismatch,
    EventSequenceViolation,
    WorkerStopped,
    Internal,
}
```

Each error contains: stable category, human-readable context, optional source
error, component identity, lifecycle state, turn ID where applicable.

Do not expose raw Wasmtime error strings as Youth's stable API. Do not panic
on guest-controlled data. Add `#![forbid(unsafe_code)]` to Youth-owned crates
during Milestone 0.

## 16. Counter fixture behavior

**Initial snapshot.** `mount()` returns:

```text
revision: 0
root: 1

nodes:
  1 root children=[2]
  2 box(enabled=true) children=[3,4]
  3 text("Count: 0") children=[]
  4 button(label="Increment", enabled=true) children=[]
```

**Activation.** Host sends:

```text
tree-revision: 0
events:
  sequence: 1
  activate node 4
```

Guest increments its disposable in-memory count and returns:

```text
base-tree-revision: 0
next-tree-revision: 1
processed-through: 1
patches:
  set-text node=3 value="Count: 1"
```

**Invalid activation.** Activating node `3`, the text node, returns
`app-error-code: rejected-event`. No patch. No revision change. Instance
remains mounted.

**Resync.** After one successful activation, `resync()` returns a complete
snapshot at revision `1` with `Count: 1`.

## 17. CLI requirements

**Validate** — `youth validate counter.wasm`. Checks: file size, valid
WebAssembly Component, required Youth world, import compatibility, successful
compilation, successful instantiation. It does not call `mount()`.

**Mount** — `youth mount counter.wasm`. Calls `mount()` and prints:

```text
component: counter.wasm
world: youth:app/application@0.0.1
lifecycle: mounted
revision: 0
nodes: 4
depth: 3
```

Then prints the canonical tree.

**Activate** — `youth activate counter.wasm --node 4`. Mounts, sends one
activation, and prints:

```text
turn: 1
event sequence: 1
base revision: 0
next revision: 1
patches: 1
status: committed
```

Then prints the final tree.

**Script** — `youth script counter.wasm tests/scripts/three-clicks.json`.
Runs a deterministic event sequence and outputs one canonical final snapshot.

**Inspect** — `youth inspect counter.wasm --json`. Outputs: lifecycle,
world/version, current revision, event sequence, node count, depth, last turn
metrics, fault information, canonical tree.

## 18. Observability

Use `tracing`. Required spans: `component.load`, `component.compile`,
`component.instantiate`, `app.mount`, `app.turn`, `app.resync`,
`tree.validate`, `tree.apply`, `app.fault`.

Required turn fields: `component_id`, `turn_id`, `event_count`,
`first_event_sequence`, `last_event_sequence`, `base_revision`,
`next_revision`, `patch_count`, `fuel_before`, `fuel_after`,
`elapsed_microseconds`, `result`.

Do not log: text-node contents by default, component memory, environment
data, future document content. Inspection commands may explicitly request
tree payloads.

## 19. Required tests

**`youth-tree` unit tests.**

Valid: minimal root; root with box; nested boxes; text and button leaves;
valid create/attach batch; valid detach/delete batch; valid reorder; valid
no-op batch.

Invalid snapshot: node ID zero; missing root; duplicate ID; multiple roots;
root with parent; orphan; missing child; duplicate child; multiple parents;
cycle; text with children; button with children; excessive depth; excessive
string length; excessive node count.

Invalid patch: wrong base revision; skipped next revision; non-empty patch
without revision increment; empty patch with revision increment; duplicate
create; delete attached node; delete non-leaf; insert unknown child; insert
unknown parent; remove wrong expected child; move wrong expected child;
invalid index; set text on button; set label on text; attach node twice;
create unreachable node left at end of batch.

Atomicity: operation 1 succeeds, operation 2 fails, live tree unchanged;
revision remains unchanged after rejected batch; canonical snapshot identical
before and after failure.

**Property tests.** Use `proptest` to generate: arbitrary malformed
snapshots, random patch sequences, duplicate IDs, invalid indexes, cyclic
relationships, extreme string and list lengths. Properties: validator never
panics; rejected batch never mutates live tree; accepted tree always
satisfies every invariant; canonical snapshot round-trips deterministically.

**Runtime integration tests.**

Valid: load counter component; mount once; activate button; activate three
times; resync after updates; inspect mounted component; stop cleanly.

Lifecycle: handle before mount rejected; mount twice rejected; call after
stop rejected; call after fault rejected.

Guest failures: trap during mount; trap during handle; invalid snapshot;
invalid patch; incorrect revision; false `processed-through`; infinite loop;
memory bomb; oversized string; oversized patch list.

Concurrency: 100 concurrent callers enqueue activations; worker processes
them serially; event sequences strictly increase; final count is 100; no
Store access occurs outside worker.

**Cross-platform test.** Build one release `counter.wasm` artifact once.
Record its SHA-256. Run the same bytes on Linux, Windows, and macOS. Assert:
same WIT contract, same final tree, same canonical snapshot hash, same
revision, same event sequence, same error classification for malformed
fixtures. Host binaries differ. The guest artifact must not.

## 20. CI requirements

Every pull request runs:

```text
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
cargo build -p youth-counter --target wasm32-wasip2 --release
wasm-tools validate <counter.wasm>
wasm-tools component wit <counter.wasm>
```

Matrix: `ubuntu-latest`, `windows-latest`, `macos-latest`.

CI structure:

1. Build the guest component once.
2. Upload it as an artifact.
3. Download the exact artifact in each host job.
4. Verify SHA-256.
5. Run the cross-platform script.
6. Compare canonical output hash.

Optional but useful: `cargo deny check`, Miri for `youth-tree`, code coverage
for tree validation.

## 21. Commit sequence

1. **Workspace** — Cargo workspace, toolchain file, CI, formatting and lint
   policy, empty crates.
2. **WIT contract** — `youth:app@0.0.1`, Rust guest bindings, host bindings,
   `wasm-tools component wit` verification. Definition: host and guest
   compile against the same WIT directory.
3. **Retained tree** — internal nodes, snapshots, invariants, canonical
   output, unit tests. No Wasmtime yet.
4. **Patch transaction** — patch operations, staged clone, batch validation,
   atomic commit, property tests.
5. **Component loader** — Wasmtime engine, component compilation, closed
   WASIp2 context, store limits, typed instantiation.
6. **Mount** — lifecycle, `mount()`, wire conversion, initial snapshot
   validation, canonical inspection.
7. **Event turn** — event sequencing, `handle()`, revision validation, patch
   commit, `resync()`.
8. **Actor boundary** — app worker, bounded command queue, asynchronous
   handle, concurrent-caller test.
9. **Containment** — fuel, epoch deadline, memory limits, transfer limits,
   fault poisoning, malicious fixtures.
10. **CLI and cross-platform gate** — validate, mount, activate, script,
    inspect, three-OS CI artifact test.

## 22. Definition of done

**Contract**

- One custom versioned Youth WIT world exists.
- Host and guest bindings are generated from the same source.
- The component's encoded WIT can be inspected with `wasm-tools`.
- No JSON or Rust serialization format crosses the component boundary.

**Runtime**

- Host loads a valid component.
- Host rejects incompatible components.
- Every app instance has one Store and one serialized worker.
- Public calls are asynchronous.
- Guest calls are synchronous, bounded, and non-reentrant.
- No ambient filesystem, network, environment, or process access exists.

**Tree**

- Initial snapshot is fully validated.
- Patch batches apply atomically.
- Revisions are enforced.
- Events are sequenced.
- Invalid guest output cannot partially mutate the tree.
- Canonical output is deterministic.

**Containment**

- Infinite loop is terminated.
- Memory bomb is rejected.
- Oversized transfer is rejected.
- Guest trap faults the instance.
- No guest-controlled input causes a host panic.
- Faulted instances cannot continue.

**Portability**

- One unchanged guest artifact runs on Linux, Windows, and macOS.
- Canonical output matches across all three.
- The emitted guest component contains no host-OS or host-architecture
  specific application behavior. Build-target gating required to keep
  guest crates workspace-compatible is permitted (see
  [GUEST-PROFILE.md](GUEST-PROFILE.md)).

**Scope**

- No renderer exists.
- No SQLite application storage exists.
- No capability system exists beyond closed WASI plumbing.
- No effect system exists.
- No speculative widget framework exists.

## 23. What Milestone 0 proves

It proves: Youth can host custom Wasm components; WIT can serve as the
host/application boundary; a guest can emit semantic retained UI without
drawing; the host can enforce tree structure and revisions; guest execution
can be serialized and bounded; invalid guest behavior cannot corrupt the live
host tree; one component artifact can cross operating systems unchanged.

It does not yet prove: good UI ergonomics, native rendering, durable
application turns, text input, filesystem safety, effects, accessibility,
application usefulness. Those belong to later milestones.
