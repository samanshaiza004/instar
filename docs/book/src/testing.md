# Testing guide

Tests are lexical `tests/*.youth-test` files and execute against the real
headless runtime with isolated state per file:

```text
mount
expect text count "Count: 0"
activate increment
restart
expect text count "Count: 1"
```

Every file has exactly one explicit initial `mount`. Commands before it and a
second `mount` fail. `restart` drops the runtime, recreates it with the same
file state, and implicitly mounts the new instance. Strings use JSON quoting
and escaping; `#` begins a comment outside a string.

`youth test` builds and validates the component once, runs files in lexical
order, and reports the file, line, command, expected value, and observed
semantic node on failure. It never opens a desktop window.
