# Developer Preview 3 — Dynamic Todo Collections

- Status: **Gate A in progress**
- Youth branch: `codex/utility-todo`
- Driving application: Youth Todo (`dev.saman.todo`)
- Application protocol: `youth:app@0.0.5` (unchanged)
- Base: `cfbac4c39be021759dc04d8d7bbcff7520642cee`

## Thesis

> A bounded dynamic collection should retain stable semantic identity while an
> application explicitly inserts, removes, moves, filters, pages, persists,
> migrates, and reconstructs its presentation—without exposing raw WIT or
> requiring a new protocol node.

Todo is an architecture probe, not a production task editor. It uses generated
titles, at most 64 live records, five visible rows, explicit state keys, and an
application-owned ephemeral filter/page session. Text entry, scrolling, list
nodes, state enumeration, structured persistence, scoped state, automatic tree
diffing, and reactive dependencies remain out of scope.

## Checkpoints

| Gate | Evidence | Status |
| --- | --- | --- |
| `utility-todo-gate-a-domain-proof` | Pure collection model, strict v1/v2 codec, atomic migration plan, SDK blocker reproduction | In progress |
| `utility-todo-gate-b-structural-sdk` | Derived identities, named containers, explicit structural updates, derived test selectors, view convergence | Not started |
| `utility-todo-gate-c-collection-evidence` | Complete external Todo behavior, paging/filtering, rollback, focus, restart, migration | Not started |
| `utility-todo-gate-d-release` | Canonical artifact certified unchanged on all hosts, source-build portability, metrics and findings | Not started |

Gate B is intentionally split into independently green B-1 through B-5
checkpoints: derived identities; structural SDK operations; test selectors;
convergence verification; immutable SDK publication and exact contract-profile
pinning. No SDK addition is authorized until Gate A demonstrates its blocker.

## Durable model

The durable model is `{ next_id, order, items }`, with deterministic loading
through a `BTreeMap`. `todos-order` is the sole source of visible order. IDs are
nonzero, never reused, and allocated monotonically. The canonical codec uses:

```text
model-schema-version
todos-next-id
todos-order
todo/<id>/title
todo/<id>/status       # v2
todo/<id>/done         # v1 only
```

Order and IDs use strict canonical ASCII decimal encodings. Titles are nonempty
UTF-8 of at most 256 bytes. Version 1 loads read-only; the first accepted command
converts every legacy boolean atomically inside the application turn. Rejected
commands do not migrate, and a commit failure retains the complete old model.
Orphan-key detection is recorded as evidence but does not authorize state
enumeration.

Filter and page belong to a guest-process-local `TodoSession`. Each new
component instance begins at All/page zero; ordinary resync retains the session.
This classification is deliberate: presentation navigation is not made durable
merely because durable state is the currently convenient SDK mechanism.

## Platform boundary

Protocol `0.0.5` already contains the primitive create/delete/insert/remove/move
patch vocabulary. DP3 may add SDK-owned derived item identities, named container
builders, and explicit structural update builders only if the external Gate A
fixture proves they are required. It must not add a list node or WIT variant.

The structural operations are strict: parents and direct children must match,
inserted subtrees must have globally unique stable identities, final move indices
refer to the post-move child list, and a current-position move emits no patch.
Operations observe the staged tree in call order.

The optional test convergence mode reconstructs through read-only resync after
mount, restart, and every committed turn. It compares guest-owned semantics but
never installs the reconstructed tree, changes focus, publishes an event, or
alters production accounting. Missing, extra, and changed nodes are distinct
diagnostics. This is evidence about explicit updates; it is not automatic SDK
diffing.

## Release evidence

One canonical validated Todo component is built on Ubuntu, retained, hashed,
and mounted as the exact same bytes on Ubuntu, Windows, and macOS. Independent
host-local builds prove source portability and log their hashes without claiming
byte reproducibility. The release publishes the established Utility Suite
metrics and marks unsupported measurements unavailable rather than zero.

The external repository must contain no path dependency, generated binding,
numeric node or command ID, revision, acknowledgement, raw patch, or component
export plumbing. Its `FINDINGS.md` is the authoritative application evidence;
`UTILITY-SUITE-FINDINGS.md` indexes accepted platform conclusions.
