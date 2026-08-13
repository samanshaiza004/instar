# Project status

Instar is an experimental runtime and UI host. It is not a stable SDK or a
general-purpose app distribution platform.

## Closed foundations

- Gate 0: async component suspension and wake-up without idle polling.
- Phase 1: real guest in a real native window, bounded bridge, host-owned crash
  surface, and layered renderer boundary.
- Phase 2: retained semantic UI, layout vocabulary, host-local interaction,
  scroll, focus, accessibility projection, application exercises, and measured
  performance/size baselines.

## Active work

Phase 3 is testing the ownership model against editable text: host buffers and
views, explicit revisions, native editing state, selection, and their protocol
integration. The current WIT and byte protocol are changing during this work.

## Open claims

- Native screen-reader behavior still requires the documented manual smoke
  pass on supported platforms.
- Text editing integration is not closed.
- The first tagged Instar binary release has not yet been published.
- No stable compatibility policy exists for WIT, wire bytes, crate APIs, or CLI.

## Deliberately undefined

Instar does not define a project manifest, app identity format, package bundle,
development server, scaffolder, registry, updater, or application deployment
story. Those are not omissions waiting for boilerplate; each is a contract the
project will add only after an external application establishes a concrete need.

The repository’s `docs/PHASE-*.md`, result reports, design ledger, and baseline
measurements contain the full engineering record.
