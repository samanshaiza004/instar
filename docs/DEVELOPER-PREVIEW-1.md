# Youth — Developer Preview 1

**Status:** Complete

**Application protocol:** `youth:app@0.0.3` with runtime compatibility for `0.0.2`  
**State protocol:** `youth:state@0.0.1`

Developer Preview 1 is driven by the first Youth Utility Suite application: a
standalone calculator in a sibling repository. The calculator must prove
semantic row/grid layout, deterministic decimal formatting, host-owned focus,
logical keyboard shortcuts, view-backed commands, durable state, and published
cross-platform evidence without exposing raw keyboard or WIT machinery to the
guest.

Gate C defines functional completion. Gate D certifies and publishes the
already-functional application and platform.

## Invariants retained from Developer Preview 0

- The state transaction and semantic-tree transaction remain one unit.
- The authoritative order remains validate tree, commit SQLite, install tree,
  then publish the patch.
- The guest receives semantic activations, not platform keyboard streams.
- Application source contains no generated WIT bindings, numeric wire IDs,
  revisions, acknowledgements, raw patches, or export plumbing.
- The calculator is a separate Git repository with an exact SDK revision and
  no path dependency on the Youth checkout.
- Protocol `0.0.2` components normalize at the runtime boundary and never make
  the interaction, layout, or renderer layers branch on protocol version.

```text
0.0.2 component ─┐
                 ├─> normalized tree and patch ─> interaction/layout/renderer
0.0.3 component ─┘
```

Normalized `0.0.2` defaults are column layout, start text alignment, and no
shortcuts. CI runs the unchanged DP0 Tally component against the DP1 host.

## Calculator contract

The calculator supports digits, decimal point, sign change, backspace, clear,
addition, subtraction, multiplication, division, and equals. It uses a bounded
decimal model with twelve significant digits. `f64` is not part of its
semantic state.

Durable state contains only the canonical model:

```text
mode
entry coefficient, scale, and sign
accumulator
pending operator
last operand and operator
error
```

The visible display is always derived by `view`; it is never persisted as a
second source of truth. Before UI integration, model tests freeze rounding,
magnitude bounds, leading/trailing zeros, negative zero, repeated equals,
operator replacement, chained operations, decimal entry after equals, digit
entry after equals, backspace, sign change during error, repeating division,
and intermediate overflow.

## Semantic presentation extension

Protocol `0.0.3` adds these bounded concepts:

```text
box-layout = column | row | grid(columns)
text-alignment = start | center | end
shortcut-key = character(string) | enter | escape | backspace
```

- Grid columns are `1..=16`; children are row-major; tracks are equal width;
  rows take the maximum child height; spans are unsupported.
- Rows and columns use deterministic host spacing and never carry pixel style
  values from the guest.
- A button has at most four shortcuts and a tree has at most 256 shortcuts.
- Character wire values are bounded to four UTF-8 bytes and runtime validation
  requires exactly one Unicode scalar for DP1.
- Duplicate logical shortcuts, command bindings, and named-key declarations
  are hard validation errors, including when controls are disabled.
- Layout, alignment, and shortcuts are static in DP1. Structural replacement
  can change them; no dedicated mutation patch is added.

## Logical shortcut normalization

Character shortcuts are logical, not physically portable. The host matches
`winit`'s `KeyEvent.logical_key` using these rules:

- Accept `Key::Character` only when it contains exactly one Unicode scalar.
- Compare exact scalar values with no normalization or case folding.
- Ignore repeated key events.
- Suppress character shortcuts while command/super or ordinary control is
  held. A Shift-produced character such as `+` is matched as `+`.
- Dead, unidentified, and multi-scalar keys do not match.
- Main-row and numpad digits normalize to the same logical digit.
- Main Enter and keypad Enter normalize to Enter.

Tab, Shift+Tab, arrows, and Space are reserved for focus policy and cannot be
declared as shortcuts in DP1.

Space always activates the focused button. A unique Enter declaration is the
window default command; without one, Enter activates the focused button. A
unique Escape declaration is the cancel command. Backspace invokes its unique
declared command. These distinctions use the same wire representation in DP1
while preserving a future default/cancel-button mapping.

## Command identity

`NodeId` and `CommandId` are distinct SDK types and hash domains. The canonical
inputs use the literal ASCII bytes backslash and zero, as established in DP0:

```text
youth:node-id:v1\0<exact UTF-8 name>
youth:command-id:v1\0<exact UTF-8 name>
```

Both use the DP0 unsigned wrapping FNV-1a procedure and upper-half mask. Each
domain has published cross-language vectors.

| Command input | Decimal ID | Hex ID |
| --- | ---: | --- |
| `clear` | `17571583584959868711` | `0xf3dace2c1a9e2f27` |
| `digit-7` | `10973116907478218231` | `0x984857b0751275f7` |
| `equals` | `17167869189710651467` | `0xee4086119b13a04b` |

DP1 commands are view-backed:

- One command binds to exactly one button.
- One button binds at most one command and may declare several shortcuts.
- Mouse, focused keyboard activation, and shortcut invocation all become that
  button's command in the SDK.
- The runtime still receives and sends the existing semantic node activation.
- Commands without a backing semantic control are unsupported.

The SDK may derive a private backing node identity from a command, but public
types and validation never equate command identity with node identity.
This one-command-to-one-button cardinality is a DP1 validation rule, not a
permanent identity contract. The separate domains leave room for a future
command to have zero or multiple presentations such as a menu item, command
palette entry, accessibility action, or automation endpoint.

## Focus and interaction policy

Focus is host-owned semantic state. It does not cross the component boundary
and does not cause guest calls by itself.

Tab traversal uses enabled buttons in semantic preorder. Shift+Tab reverses
that order. Traversal does not wrap. A newly mounted tree has no focus; the
first Tab selects the first enabled button.

Arrow traversal resolves against the nearest layout ancestor supporting the
requested axis:

- Row supports Left and Right.
- Column supports Up and Down.
- Grid supports all four directions.
- Traversal does not wrap and does not use geometric nearest-neighbor search.
- Disabled targets and ragged empty grid slots are skipped in the same
  direction.
- Failure to find a target retains focus.
- Traversal does not escape the nearest applicable focus group in DP1.

When the semantic tree changes, focus reconciliation retains the same enabled
node, otherwise chooses the next enabled node after its prior semantic
position, otherwise the previous enabled node, otherwise clears focus.

Primary press focuses a button. Release outside cancels activation but leaves
focus on that button. Window deactivation clears pointer arming and pressed
state while retaining semantic focus.

`youth-interaction` exposes renderer-independent state:

```rust
pub struct InteractionSnapshot {
    pub focused: Option<NodeId>,
    pub enabled_actions: Vec<SemanticAction>,
}

pub enum SemanticAction {
    Focus(NodeId),
    Activate(NodeId),
}
```

`enabled_actions` contains `Focus` and `Activate` for each enabled button in
semantic order and is bounded by the validated tree's node limit.

The renderer consumes the focus state. A future AccessKit projection will
consume the same stable semantic IDs, roles, focus, and actions. DP1 honestly
reports zero native accessibility projection rather than making focus
renderer-private.

## Semantic view convergence

After an accepted turn, applying its update to the previous normalized tree is
intended to produce the same guest-owned semantic tree as constructing a fresh
view from the committed durable state. Host-owned focus, pointer state,
geometry, raster output, and other interaction or presentation state are
excluded.

DP1 does not generally enforce this invariant. Explicit patches remain the
authoring model, and production does not call the guest again after every turn.
A future test-only convergence check could compare a patched tree with a
read-only reconstruction, but it first requires `Application::view` to be
formally deterministic and free of observable side effects. SDK tree diffing,
reactive dependencies, and `youth test --verify-view-convergence` are deferred
mechanisms, not DP1 features.

## Evidence and metrics

Every release publishes schema-versioned raw JSON and a Markdown summary.
Environment fields include Youth commit, SDK revision, calculator commit, Rust
and Wasmtime versions, OS, architecture, CPU, runner type, profile, Wasmtime
cache state, component SHA-256, and metrics schema version.

Measurements distinguish:

- source installation from a clean Cargo home versus future distributed
  installation;
- cold process/cold Wasmtime cache from cold process/warm cache, plus load,
  compile, instantiate, mount, layout, and first-present stages;
- headless memory for one, two, and four instances from desktop memory for one
  host/window/app;
- runtime turn latency from keyboard-to-presented-frame latency;
- logical wire payload bytes from physical Component Model memory copying;
- state commit latency from total turn latency.

The calculator release establishes performance baselines. Exact component
identity, zero idle guest calls, transactional recovery, and zero raw-WIT
concepts in app source are immediate hard gates. Numeric regression budgets
begin only after two comparable releases.

The first release baseline is recorded in
`docs/metrics/calculator-dp1-macos-arm64.json`, with its limitations and
interpretation in the accompanying Markdown summary. Gate D CI run
`30504489792` certified one canonical component byte-for-byte on Ubuntu,
Windows, and macOS. The local host build remains separate source-portability
evidence and is not required to be byte-reproducible.

## Checkpoints

1. `utility-calculator-gate-a-app-proof` — calculator model and DP0
   expressiveness evidence are complete in the external repository.
2. `utility-calculator-gate-b-layout` — normalized protocol `0.0.3`, SDK
   layout, renderer layout, and calculator presentation are complete.
3. `utility-calculator-gate-c-keyboard` — focus, commands, logical shortcuts,
   semantic tests, and the functional calculator are complete.
4. `utility-calculator-gate-d-release` — metrics, compatibility evidence,
   cross-platform certification, documentation, and publication are complete.

## Non-goals

Expression parsing, history, scientific functions, locale formatting,
arbitrary styling, text fields, IME input, user-reconfigurable shortcuts,
commands without controls, global menus, command palettes, grid spans, focus
wrapping, geometric focus search, native accessibility projection, reactive
diffing, multiple windows, packaging, and non-Rust guests are deferred.
