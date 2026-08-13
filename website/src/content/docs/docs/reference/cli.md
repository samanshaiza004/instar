---
title: Command line
description: The whole Instar CLI — one command, its options, and its exit codes.
sidebar:
  order: 1
---

Instar has one command.

```text
instar run <component.wasm> [--debug]
```

## Global help

```sh
instar --help
```

`instar run --help` prints the same compact command help and exits successfully.

## Arguments and options

| Form | Meaning |
|---|---|
| `run` | Run one Component Model guest in a native window. |
| `<component.wasm>` | Path to a non-empty component implementing Instar's current `kernel` world. |
| `--debug` | Report lifecycle, commits, accessibility requests, and frame timing on stderr. |
| `--inset-scrollbars` | Undocumented experimental host policy: place scrollbar chrome inside the content viewport. Accepted by current builds, but intentionally omitted from `--help`. |
| `-h`, `--help` | Print help and exit 0. |

Options may appear around the command and path:

```sh
instar --debug run app.wasm
instar run --debug app.wasm
instar run app.wasm --debug
```

## Exit behavior

| Code | Meaning |
|---|---|
| `0` | Help was printed, or the guest returned normally. |
| `1` | The component could not load, the event loop failed, or startup failed. |
| `2` | Invalid command-line use. |

## Deliberately absent commands

`new`, `build`, `dev`, `package`, `inspect`, `validate`, and `doctor` do not
exist. The project has not frozen an application manifest, build convention,
package layout, or compatibility contract, and the CLI does not imply one.

This is a standing decision rather than a backlog item. Each of those commands
would publish a contract — where a project lives, what a bundle contains, what
"valid" means — that Instar has not yet earned the right to freeze. See the
[project status](/docs/project/status) for the reasoning.
