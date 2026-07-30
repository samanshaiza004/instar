# Testing guide

Tests are lexical `tests/*.youth-test` files and execute against the real
headless runtime with isolated state per file:

```text
state integer "count" 4
mount
expect text count "Count: 4"
expect state integer "count" 4
activate increment
restart
expect text count "Count: 5"
expect state integer "count" 5
```

Every file has exactly one explicit initial `mount`, preceded only by zero or
more `state` commands. A `state` command after the initial mount and any other
command before it fail, as does a second `mount`. Seeds are written through the
ordinary typed state API and its normal quotas before the app is spawned.
`restart` drops the runtime, recreates it with the same file state, and
implicitly mounts the new instance. Strings use JSON quoting and escaping; `#`
begins a comment outside a string.

The state seed grammar is:

```text
state boolean "key" true
state boolean "key" false
state integer "key" 42
state text "key" "a JSON string value"
state bytes "key" "a JSON string, UTF-8 encoded to bytes"
```

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
activate derived "todo" 1 "toggle"
expect present derived "todo" 1 "row"
expect text derived "todo" 1 "title" "Task 1"
expect child-count items 5
```

Run `youth test --verify-view-convergence` to reconstruct the guest view after
mount, restart, and every committed turn. The checker compares guest-owned
semantics and reports missing, extra, and changed nodes without installing the
verification snapshot or publishing another turn.
