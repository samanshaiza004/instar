# `.youth-test` DSL evolution — implementation brief

Status: proposed, not yet started. This document is the full brief for a
session picking this work up cold. Gate A (Scratchpad, the Editor node,
`youth:editor` capability) is complete and merged to `master` — this work
does not touch Gate A's platform code, only the `.youth-test` test
language and its runner.

## What Youth is, in one paragraph

Youth is a local-first application platform: guest apps are Wasm
components (`wasm32-wasip2`) built against a versioned `youth:app`
WIT contract via `youth-sdk`; the host (`youth-runtime`) mounts them in a
closed WASI context, drives them through discrete "turns" (one guest call,
one committed or rolled-back transaction against a semantic tree +
durable state), and renders/handles input entirely host-side (desktop via
`youth-desktop`, headless via `youth-cli`/`youth-test`). The guest never
sees pixels, native input codes, or raw byte offsets into anything it
doesn't own. This is why the existing `.youth-test` DSL is unusually
strong already: it drives real components through the real transaction
machinery, semantic tree, durable state, and host services, with no
rendering dependency — closer to Compose's semantics-tree testing or
Playwright's "test user-facing behavior, not implementation" philosophy
than to a pixel/screenshot harness.

Four apps exist in the Utility Suite so far: Calculator, Timer, Todo, and
now Scratchpad (the first to use a host-owned Editor node — real text
buffer, cursor, selection, IME, undo/redo, clipboard, scrolling,
AccessKit, all host-local with zero guest turns for ordinary typing).
Scratchpad lives at `~/dev/scratchpad`, a sibling repo, same pattern as
`~/dev/youth-timer`.

## Where the current system lives

- `crates/youth-test/src/lib.rs` — the entire parser + runner. One file.
  `parse_command` (line ~195) does line-oriented parsing via
  `str::strip_prefix`; there is no block/nesting syntax anywhere today.
  `Command` (line ~62) is the full enum of supported commands. The `run`
  loop (`match &located.command { ... }`, starting ~line 676) is where
  each command actually executes against a real `YouthAppHandle`.
- `crates/youth-cli/src/main.rs` — CLI surface. `Commands::Test { verify_view_convergence: bool }`
  (line ~49) is the `youth test [--verify-view-convergence]` flag; it is
  **not** read from `Youth.toml`, and defaults to `false`.
- `crates/youth-project/src/lib.rs` — `Youth.toml`/`Youth.lock` manifest
  schema (`Manifest`, `App`, `Build`, `Development` structs) and the
  `SUPPORTED_PROFILES` contract-profile list (protocol → WIT hash → SDK
  revision). Adding a `[test]` section to the manifest schema happens
  here.
- `crates/youth-runtime/src/worker.rs` — `YouthAppHandle::activate` (line
  ~272) sends a raw `AppCommand::Activate` directly by `NodeId`, with **no**
  enabled/focus/policy check — that check lives entirely in
  `youth_interaction::InteractionState::key()`
  (`crates/youth-interaction/src/lib.rs`), which `.youth-test`'s
  `Command::Key` already routes through and `Command::Activate` bypasses
  entirely.
- `crates/youth-state/` — has a `test-support` feature
  (`youth-runtime/Cargo.toml`: `test-support = ["youth-state/test-support"]`)
  exposing deterministic/virtual clock and wake-driver seams
  (`youth_state::DeadlineClock`, `WakeDriver`), already used throughout
  Gate B/C (Timer's scheduling work). `.youth-test`'s current `Command::Sleep`
  uses real `tokio::time::sleep` (wall time), **not** these seams — the
  runner spawns apps with the production system clock/wake-driver, so
  "advance virtual time" does not exist as a capability today even though
  the underlying seams do.

## Current DSL surface (for reference — do not break these without updating callers)

`mount`, `restart`, `activate <selector>`, `sleep <ms>`, `key <key>
[+modifiers]`, `state <boolean|integer|text|bytes> <"key"> <value>`,
`expect state <boolean|integer|text|missing> <"key"> [value]`, `expect text
<selector> <"value">`, `expect countdown <selector>`, `expect focus
[<selector>]`, `expect present <selector>`, `expect missing <selector>`,
`expect child-count <parent> <n>`, `expect child <parent> <index> <selector>`.
Selectors are either a bare identifier (matches a `node!("...")` key) or
`derived "<namespace>" <item> "<role>"`. Existing `.youth-test` files exist
in this repo's `test-components/` fixtures and in the sibling app repos
(`~/dev/youth-timer/tests/*.youth-test`, `~/dev/scratchpad/tests/*.youth-test`)
— any breaking grammar change needs those to keep parsing, or needs a
version gate (see below).

## The proposal (from a design review, verified against real code)

The reviewer's full proposal, organized by area. Every "current behavior"
claim below was independently verified against the code in this session —
see the "Verification notes" after each section.

### 1. Format version header

First non-comment line of every `.youth-test` file:

```text
youth-test 1
```

Versions the *test language*, independently of the Youth application
protocol (`youth:app@0.0.6` etc.) — a given language version should be
able to drive multiple supported component profiles. Missing header =
treat as legacy version 1 (or have a future `youth fmt-tests` insert it
after a transition period). Do this **first**, before any of the grammar
changes below, since the grammar is about to expand substantially and a
version gate is what lets you change syntax without breaking every
existing `.youth-test` file in every app repo at once.

### 2. `invoke` / `click` / `key` — split the current overloaded `activate`

```text
invoke <selector>      # direct guest activation, bypasses host policy — tests guest command guards, can target a disabled control
click <selector>       # passes through host interaction policy — requires present/enabled/correct role
key <key>              # passes through focus, editor, shortcut, keyboard policy (already exists, unchanged)
```

**Verified**: `activate` today (`worker.rs:272`,
`YouthAppHandle::activate`) is already exactly what `invoke` should be —
it sends `AppCommand::Activate` straight to the guest by `NodeId` with zero
policy checks. `key` already routes through
`youth_interaction::InteractionState::key()`, which does apply
focus/enabled/shortcut policy. So this is mostly a *naming and
documentation* fix plus one new command (`click`): keep `activate` as a
compatibility alias for `invoke`, stop describing it as "like a mouse
click" anywhere in docs, and add `click` as a new command that goes
through `InteractionState`'s policy (present, enabled, correct role) —
real headless hit-testing/geometry can come later; until then `click`
enforcing only semantic policy is an honest, useful subset.

This distinction would have surfaced SCRATCHPAD-F001 (Scratchpad has no
real `Ctrl`/`Cmd`+S — a focused Editor declining that combination has
nowhere to fall through to today, since `youth_tree::ShortcutKey` has no
modifier-aware variant) as a test failure instead of something found by
manual code tracing. High value, low implementation cost.

### 3. Virtual time: `advance time <duration>`, demote `sleep` to `sleep real <duration>`

```text
advance time 100ms   # advance the injected DeadlineClock, run due reconciliation,
                      # process resulting mailbox work, deliver pending events,
                      # stop when quiescent
sleep real 150ms      # (or `wall-sleep`) — real wall-clock sleep, kept explicit
                      # and rare, for production-clock smoke evidence
```

**Verified**: the deterministic clock/wake-driver seams already exist
(`youth_state::test_support`, used throughout Timer's Gate B/C work) but
`.youth-test`'s runner does not currently wire them in — it uses real
system time via `tokio::time::sleep`. This is **not** just "add a new
command"; it means the runner needs to spawn `.youth-test` apps with a
swappable/virtual `DeadlineClock` + `WakeDriver` by default, falling back
to the real ones only for `sleep real`. Check `youth-state`'s
`test-support` feature surface first (what's exposed there today was built
for Timer's own Rust integration tests, not necessarily in a shape the
`.youth-test` runner can reuse directly) before committing to a design.
Keep at least one real `sleep real` test for Timer (don't delete the
existing wall-clock Gate C-4 evidence — it's the only thing that proves
the production deadline clock and wake path work against actual elapsed
time, not just the virtual model).

### 4. Automatic settling — no general `await` command

**Verified, refined**: `app.activate(node).await` (and the equivalent for
`key`) already fully awaits *that turn's* commit/rollback before the next
`.youth-test` line runs — so ordinary same-turn assertions are already
deterministic with no extra command needed. What is *not* automatically
drained is anything triggered asynchronously off the back of that turn
(wake-driven schedule reconciliation on a background path) — which is
exactly why `advance time` (item 3) needs to exist as an explicit
"drain queued host work" primitive, not because ordinary command→assertion
sequencing is currently unsynchronized. Add `eventually <assertion>
within <duration>` only for genuinely external, non-Youth-controlled
conditions (a native filesystem watcher, once the filesystem gate exists)
— it should describe an assertion *retry policy*, not a second async
control-flow language.

### 5. Convergence checking → `Youth.toml`, on by default for generated projects

**Verified**: `verify_view_convergence` already exists end-to-end
(`youth-test`'s `RunOptions`, `youth-cli`'s `--verify-view-convergence`
flag) but is **opt-in and defaults to `false`**, and is **not** read from
`Youth.toml` at all today. Concretely: the Scratchpad `.youth-test` run
done during Gate A9 did *not* pass this flag, so that evidence is weaker
than it could be — this is a real, immediately-actionable gap independent
of everything else in this brief.

Add:

```toml
[test]
verify_view_convergence = true
```

to the `Youth.toml` schema (`youth-project::Manifest`/`App`/new `Test`
struct), default `true` for freshly generated projects (`youth new`), CLI
gets `youth test --no-verify-view-convergence` for named exceptions
(perf measurements, deliberately-divergent fixtures, fault-injection
tests, guest-call-counting tests). The runner should print when it's
disabled, so a developer can't silently get weaker evidence than they
think they have. This item alone (no dependency on anything else in this
brief) is worth doing first or in parallel — go fix Scratchpad's and
Timer's own `.youth-test` suites to actually pass this flag once it's
wired, and re-verify they still pass.

### 6. Editor interaction family

```text
type editor "Hello, 世界"          # committed text input
paste editor "replacement text"     # host clipboard/edit operation
compose editor start "n"            # IME preedit lifecycle
compose editor update "ñ"
compose editor commit "ñ"
compose editor cancel
select editor graphemes 0..5        # host-owned selection, explicit position unit
replace-selection editor "Hi"
expect editor text editor "Hi, 世界"
expect editor selection editor graphemes 2..2
expect editor dirty editor true
```

This maps almost 1:1 onto real primitives built during Gate A —
`youth_runtime::EditorLocalEdit::{InsertText, Paste, ImeSetCompose,
ImeClearCompose, ImeFinishCompose, ExtendSelectionToPoint,
SetSelectionFromAccessKit}` in `crates/youth-runtime/src/editor_session.rs`.
Do **not** implement `type` by iterating `key` over characters — that
tests keyboard shortcut handling, not the editor contract, and would miss
exactly the kind of thing these commands exist to test. Position units
must be explicit and should default to **grapheme clusters** for this
user-facing DSL (not bytes, not UTF-16 units, not Unicode scalar values) —
lower-level byte/engine-internal-cursor correctness is already covered by
`youth-editor-engine`'s own Rust unit tests and should stay there. Note:
`EditorLocalEditResult`'s `selection`/`cursor` are currently byte offsets
(`crates/youth-runtime/src/editor_session.rs`); the runner will need a
byte→grapheme conversion at the assertion layer, mirroring the
byte→char-index conversion already done for AccessKit
(`youth-editor-engine`'s `presentation()` — see how Parley's own
`accessibility()` handles this for a pattern to follow, since raw
byte-offset math there has already caused one real bug this session).

### 7. `measure` / metrics markers — prove the zero-guest-turn architecture from app repos

```text
measure begin "typing"
type editor "10,000 characters..."
measure expect "typing" guest-turns 0
measure expect "typing" state-writes 0
```

Candidate counters: `guest-turns`, `state-calls`, `state-writes`,
`commits`, `rollbacks`, `host-repaints`, `observer-outcomes`,
`pending-deliveries`. This is the single highest-leverage new capability
in the whole proposal: right now "no guest turn per keystroke" is provable
only from *inside* `youth-runtime`'s own Rust test suite
(`crates/youth-runtime/tests/editor_local_input.rs`'s
`baseline.guest_call_count` / `after.guest_call_count` comparisons — see
`ten_thousand_local_edits_make_zero_guest_calls` for the exact pattern to
generalize). Scratchpad's own `FINDINGS.md` (at `~/dev/scratchpad`)
explicitly says its test suite cannot exercise this property today and
has to point at the platform's Rust tests instead — `measure` would close
that gap and let every app repo prove Youth's core architectural promises
in its own words, not just the platform's. Namespace these clearly under
harness observation, not the normal semantic-tree `expect` vocabulary —
they're not app semantics.

### 8. App-vs-host lifecycle split

```text
restart app      # today's `restart`: drop the guest/runtime instance, recreate against the same state file, mount again — keep this behavior, just rename for clarity once the other two exist
close app         # unload without dropping host process/services
open app          # mount a previously-unloaded app
shutdown host      # clocks, watchers, in-memory resources, process state all disappear
start host
```

Low priority relative to items 1–7 — the distinction only matters once
there's an actual persistent host process/service layer to distinguish
from the guest instance (there isn't one yet; today's "host" in a
`.youth-test` run is just the test process itself). Keep `restart` working
as-is; add `restart app` as the honest name once `close`/`open` exist, and
add `shutdown host`/`start host` only when a real use case needs them
(background services, multi-app hosting).

### 9–12. Deferred: resource fixtures, capability grant/deny, external mutations, `eventually`

```text
workspace "notes" { file "inbox.md" "Hello\n"  directory "journal" }
grant workspace "notes"
external write "notes" "inbox.md" "Changed elsewhere\n"
capability workspace deny
expect request workspace
respond deny | respond grant "notes"
```

**Do not start these yet.** Youth's WASI context is fully closed today —
there is no filesystem capability of any kind, guest or host-mediated.
This is correctly speculative design for a future "filesystem gate" that
doesn't exist. Revisit when that gate is scoped. The one thing worth
doing now: keep this section of the brief so whoever scopes the
filesystem gate has it, and make sure paths in any future design stay
relative to a temporary capability root — the DSL must never accept an
ambient developer-machine path.

### 13–15. Later: typed semantic-property vocabulary generalization, canonical semantic snapshots, parallel test execution

Normalize `expect` around semantic categories (role, content, state,
structure, focus, capability/resource association, accessibility meaning)
rather than one bespoke grammar per widget kind or a generic untyped
property bag (`expect property node "arbitrary-name" ...` — explicitly
avoid this; it would let the DSL leak implementation details and stop
Youth from having to define what each capability's assertions actually
mean). Semantic snapshots (`snapshot semantics "name"`, `insta`-style) wait
until there's a frozen canonical serialization (excludes host-owned
cursor/focus unless asked for, excludes numeric IDs, deterministic
property order, distinguishes stable names from derived selectors, no
protocol-version-specific wire representation). Parallel execution: files
must stay isolated and never depend on lexical order, even though lexical
order remains useful for deterministic reporting today.

### Fix the `bytes` state seed format (small, standalone, do anytime)

**Verified**: `state bytes "key" "..."` (`youth-test/src/lib.rs:497-506`,
`parse_state_value`) parses a JSON string and does `.into_bytes()` — it is
UTF-8 text with a misleading name, and cannot represent arbitrary binary
or invalid UTF-8. Either rename honestly (`state utf8-bytes`) or add real
encodings (`state bytes-hex "key" "00ff7f80"`, `state bytes-base64 "key"
"AP9/gA=="`). The DSL should be able to exercise every value the typed
state API (`youth_state::StateValue::Bytes(Vec<u8>)`) actually supports.

### UTF-8-safe selectors

Keep the bare-identifier shorthand (`expect text count "..."`), but make
the canonical grammar support quoted exact names for non-ASCII/
punctuation-containing keys: `expect text "sidebar/current note" "Hello"`,
`expect present "文書/現在"`. Don't let the parser's convenience shorthand
accidentally narrow what Youth's actual UTF-8 identity model allows.

## Suggested order of work

Roughly the reviewer's own ordering, adjusted only in that everything
originally scoped "before Scratchpad Gate A" is now simply "do now," since
Gate A already shipped:

1. Format version header (`youth-test 1`) — do this before anything else touches the grammar.
2. `Youth.toml` `[test]` section + convergence-default flip (independent, high value, verify Scratchpad/Timer's suites still pass with it on).
3. `invoke`/`click`/`key` split (`activate` → alias for `invoke`).
4. `bytes` state seed format fix.
5. UTF-8 selector quoting.
6. Virtual time (`advance time` / `sleep real`) — check `youth-state::test_support`'s actual surface before committing to a design; this is the one item here with real architectural uncertainty.
7. Editor interaction family (`type`/`compose`/`paste`/`select`/`replace-selection`, grapheme positions).
8. `measure`/metrics markers.
9. App-vs-host lifecycle naming (`restart app` etc.) — low urgency, do if time allows.

Items 10–12 (workspace fixtures, capability grant/deny, external
mutations) stay blocked on a filesystem gate that doesn't exist yet — do
not start them. Items 13–15 (typed vocabulary generalization, semantic
snapshots, parallel execution) are correctly "later."

## Ground rules for this session

- Follow the same rigor pattern used throughout Gate A: design → implement
  → `cargo test`/`cargo fmt --check`/`cargo clippy --workspace --all-targets -- -D warnings`
  → verify end-to-end against a real fixture (this repo's `test-components/`
  and/or `~/dev/scratchpad`, `~/dev/youth-timer`) → commit.
- **Commit locally as you go, but do not push to `origin/master`.** Leave
  pushing for review by the primary session/user.
- **Do not add a `Co-Authored-By: Claude ...` trailer to any commit.**
  Commits should be attributed to the user's own git identity only — this
  is a standing preference, not specific to this task.
- If a grammar change would break existing `.youth-test` files in
  `~/dev/youth-timer` or `~/dev/scratchpad`, either keep it backward
  compatible or gate it behind the version header (item 1) — do not
  silently break sibling app repos.
- When in doubt about current behavior, verify against the real code
  before assuming the proposal's description is accurate — most of it was
  independently checked while writing this brief, but not every line.
