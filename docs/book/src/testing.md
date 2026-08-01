# Testing guide

Tests are lexical `tests/*.youth-test` files and execute against the real
headless runtime with isolated state per file:

```text
youth-test 1

state integer "count" 4
mount
expect text count "Count: 4"
expect state integer "count" 4
activate increment
restart
expect text count "Count: 5"
expect state integer "count" 5
```

The first non-comment, non-blank line may declare the file's format version:
`youth-test <n>`. A file with no header is treated as version 1 (legacy). This
versions the *test language* grammar, independent of the Youth application
protocol (`youth:app@0.0.6` etc.) the driven component implements -- one
language version can drive many supported component profiles.

Every file has exactly one explicit initial `mount`, preceded only by zero or
more `state` commands. A `state` command after the initial mount and any other
command before it fail, as does a second `mount`. Seeds are written through the
ordinary typed state API and its normal quotas before the app is spawned.
`restart` drops the runtime, recreates it with the same file state, and
implicitly mounts the new instance. Strings use JSON quoting and escaping; `#`
begins a comment outside a string.

Node selectors are either a bare identifier (no whitespace, matching a
`node!("...")` key) or a quoted exact name, which can hold whitespace, `#`,
and any other UTF-8 the bare form cannot safely delimit:

```text
expect present "sidebar/current note"
expect present "文書/現在"
```

The state seed grammar is:

```text
state boolean "key" true
state boolean "key" false
state integer "key" 42
state text "key" "a JSON string value"
state utf8-bytes "key" "a JSON string, UTF-8 encoded to bytes"
state bytes-hex "key" "00ff7f80"
state bytes-base64 "key" "AP9/gA=="
```

`bytes` is kept as a compatibility alias of `utf8-bytes`: despite the name, it
can only represent well-formed UTF-8 text encoded as bytes. `bytes-hex` and
`bytes-base64` can represent every value the typed state API's
`StateValue::Bytes(Vec<u8>)` actually supports, including invalid UTF-8.

State assertions open the isolated state file independently of the running app:

```text
expect state boolean "key" true
expect state integer "key" 42
expect state text "key" "value"
expect state missing "obsolete-key"
```

Semantic content assertions use:

```text
expect text <node-name> <JSON-string>
expect countdown <node-name>
```

`expect countdown` checks that the node contains a host-owned schedule reference;
it does not resolve the countdown to a display value.

Activation has two forms with different policy:

```text
invoke <selector>      # direct guest activation: bypasses host interaction
                        # policy entirely (no present/enabled/focus/role
                        # check). Tests guest command guards, and can target
                        # a control the host would refuse a real user access
                        # to (e.g. a disabled button). `activate` is a
                        # backward-compatible alias of `invoke`.
click <selector>        # semantic click: requires the target be present,
                         # enabled, and an activatable role (a button) --
                         # the same host policy a real pointer click would
                         # require. Real headless hit-testing/geometry is
                         # not implemented yet, so this enforces only that
                         # semantic subset of click policy.
key <key>                # passes through focus, shortcut, and editor policy.
```

Harness timing uses:

```text
sleep <milliseconds>
```

`sleep` is a real wall-clock sleep in the test process. It is intended for short
durations paired with the platform's minimum schedule duration (100 ms), not as
a general-purpose delay primitive.

`youth test` builds and validates the component once, runs files in lexical
order, and reports the file, line, command, expected value, and observed
semantic node on failure. It never opens a desktop window.

Tests may drive the host interaction layer without native scan codes:

```text
mount
expect focus none
key tab
expect focus clear
key "7"
key "+"
key enter
expect focus equals
```

Named keys are `enter`, `escape`, `backspace`, `space`, `tab`, `shift-tab`,
`left`, `right`, `up`, and `down`. Character keys are one-scalar JSON strings.
These commands exercise host focus and shortcut policy; the component still
receives only semantic button activation.

Dynamic collection tests can address a derived identity without knowing its
numeric representation:

```text
invoke derived "todo" 1 "toggle"
expect present derived "todo" 1 "row"
expect text derived "todo" 1 "title" "Task 1"
expect child-count items 5
```

`youth test` reconstructs the guest view after mount, restart, and every
committed turn, and compares guest-owned semantics -- reporting missing,
extra, and changed nodes -- without installing the verification snapshot or
publishing another turn. This is controlled by `Youth.toml`'s `[test]`
section:

```toml
[test]
verify_view_convergence = true
```

and defaults to `true` (including for a `Youth.toml` with no `[test]`
section at all), so weaker evidence must be opted into by name, not fallen
into silently. `youth test --verify-view-convergence` and
`youth test --no-verify-view-convergence` override the manifest for one run
-- for named exceptions (perf measurements, deliberately-divergent fixtures,
fault-injection tests, guest-call-counting tests). The runner prints when
convergence checking is disabled.

**Known gap with Editor nodes**: `context.editor().accept()`/`replace()`
(the `youth:editor` capability) advance the live host-owned Editor session's
`document_revision` directly; there is no `Patch::SetEditorRevision`-shaped
patch, so the retained tree's `Editor` node keeps whatever `document_revision`
the guest last declared in a full `view()`-derived tree until the next mount,
restart, or resync. A resync-based reconstruction, in contrast, always
reflects the current state -- so convergence checking reports a "changed
node" for any Editor node `accept()`/`replace()` touched since the tree was
last fully installed, even though nothing is actually wrong. An app that
relies on `accept()`/`replace()` (Scratchpad is the first) needs
`verify_view_convergence = false` with a comment explaining why until this
is resolved -- either by adding a real patch for the Editor node's
declared revision, or by teaching the convergence checker to treat Editor
document-revision drift since the last full install as expected.
