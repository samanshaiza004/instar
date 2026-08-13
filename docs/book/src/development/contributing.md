# Contributing

Instar is developed by evidence: new surface area should answer a need shown by
an application or a failing gate, not complete a familiar framework checklist.

## Before changing behavior

1. Read the focused reference document (`ARCHITECTURE.md`, `PROTOCOL-0.md`, or
   the current phase plan).
2. Find the boundary test that protects the behavior.
3. Add or adjust the smallest discriminating test.
4. Make the change without broadening guest authority.
5. Run formatting, targeted tests, and then the workspace suite.

## Rules worth knowing

- The guest dependency allowlist is stricter than a blocklist.
- A protocol change must rebuild all embedded guest components automatically.
- Host UI state is keyed by full generational `NodeKey`.
- A refused commit must leave the previous presentation intact.
- Benchmark binaries are examples, not release artifacts.
- Existing historical and baseline documents are evidence; do not rewrite
  their past terminology to look current.

## Report a problem

Open an issue with a minimal reproduction, OS/architecture, toolchain, exact
command, expected result, actual result, and relevant logs. For UI behavior,
state whether the problem is semantic tree, layout, interaction, accessibility,
scene lowering, raster output, or native presentation if known.
