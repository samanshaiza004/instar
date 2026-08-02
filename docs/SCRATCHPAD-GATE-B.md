# Scratchpad Gate B — One Real Text Document

Status: implemented on `codex/scratchpad-gate-b`; cross-platform release
certification remains pending until the branch CI matrix passes.

This document is the authoritative contract for the first file-backed Youth
application. It adds `youth:text-document@0.0.1` and `youth:app@0.0.8` while
leaving every `0.0.2` through `0.0.7` profile frozen.

## Ownership and ordering

One explicit grant names an existing UTF-8 document relative to a trusted
root. The host retains the opened directory capability and owns the file
bytes, encoding, accepted baseline, live Editor session, edit sequence, and
replacement worker. The guest sees an opaque `Document`, declares
`Editor::document`, and may request Save semantically.

```text
guest stages UI/state/save request
→ validate staged semantics
→ commit SQLite
→ install and publish the authoritative tree
→ enqueue immutable captured bytes
→ exact-byte conflict check
→ atomic same-directory replacement
→ host-sequenced save-completion turn
```

Any failure before `TurnCommitted` discards the request without touching the
file. A committed request is not a durable effect journal: a process crash
before worker dispatch may leave disk unchanged. If replacement succeeds but
the completion turn is lost, disk is canonical and restart reconstructs it.

## Grant and encoding contract

`--workspace-root` and `--document` are supplied together. The relative path
must be UTF-8, at most 1,024 bytes and 32 normal components, and may contain no
absolute prefix, root, `.`, `..`, or symlink component. The final entry must
exist and be a regular file. The host repeats these checks capability-relative
at save time; missing, substituted, symlink, and wrong-type entries conflict.

The generic capability imposes no filename extension. Scratchpad accepts only
`.txt` and `.md`.

An initial UTF-8 BOM is removed from the Editor-visible text and remembered as
`Utf8WithBom`; it is restored on Save. All other valid UTF-8 bytes—including
CRLF, LF, no final newline, embedded NUL, and interior U+FEFF—are preserved.
Invalid UTF-8 is rejected. Visible text is bounded to 1 MiB; the encoded file
may contain three additional BOM bytes.

## Replacement and conflict contract

The worker creates a private candidate in the retained destination directory,
writes and syncs complete encoded bytes, copies supported ordinary destination
permissions, revalidates the grant, and compares the destination to the
accepted baseline byte-for-byte. A mismatch leaves the destination untouched.
If the bytes match, the host uses the platform's capability-relative
same-directory replacement operation and synchronizes the parent where
supported. Truncate-in-place is never a fallback.

This detects every external content change visible at final comparison. A
non-cooperating writer after comparison but before replacement can still be
overwritten. Metadata-only changes do not conflict. Gate B preserves ordinary
writable/read-only behavior where supported; it does not promise universal
ACL, xattr, ownership, resource-fork, or power-loss preservation.

The replacement backend is isolated behind `AtomicFileReplacer`. The selected
implementation uses `cap-tempfile`'s same-directory, directory-capability-
relative replacement on Unix, macOS, and Windows. This was preferred over an
ambient-path `ReplaceFileW` integration: it retains one containment model and
avoids the partially-progressed ambient-path failure cases documented for
`ReplaceFileW`. Cross-platform type/symlink/conflict/replacement tests are the
release evidence for that choice.

## Effects, completion, and shutdown

Every text-document import counts as a capability call before validation. A
valid Save request counts as one external-effect write attempt. Busy is normal
recoverable input and does not count as an effect write; forged or stale
bindings are contract violations. Guest rejection after an effect write
attempt faults the instance.

Success accepts exactly the captured edit sequence and issues an opaque
host-owned `DocumentVersion`. Newer live edits remain dirty. Only the exact
version/document/editor tuple from the completion may be patched into the
semantic tree. Conflict and stable non-sensitive failure categories never
discard the local buffer.

Clean-to-dirty is a coalesced current-state notification. Further dirty edits,
cursor/selection movement, IME preedit, scrolling, resize, pointer motion, and
no-op edits do not call the guest. Save completion is an effect outcome, not a
coalesced notification.

The host tracks and joins every active save worker. Work may be abandoned only
before its final comparison; from comparison onward it is non-cancellable and
must finish reconciliation. Orderly shutdown never detaches a worker capable
of replacing the document later.

## Test surface and release evidence

`.youth-test` supports byte-exact file fixtures and external writes (text,
hex, and canonical padded base64), `grant document`, `key "s" +primary`, file
assertions, and `expect editor dirty`. Each test receives an isolated root;
restart retains the grant and reloads the canonical file. Actions drain save
work and completion turns before subsequent assertions.

Host tests cover traversal, symlink substitution, invalid UTF-8, the 1 MiB
boundary, BOM/line-ending round trips, exact-byte conflicts, permissions,
commit ordering, completion versions, dirty coalescing, generation isolation,
and orderly worker joining. Scratchpad proves open/edit/Save/restart and
Primary+S through the real component. Protocol fixtures preserve the frozen
`0.0.2`–`0.0.7` profiles and continue rejecting WASI filesystem authority.

The release model separates one canonical Ubuntu-built component executed
unchanged on every host from independent host-local source builds. Independent
byte reproducibility is not claimed.

The first reporting-only 1 MiB release measurements are recorded in
[`metrics/scratchpad-gate-b-local.md`](metrics/scratchpad-gate-b-local.md).
They are local evidence, not a hosted-platform certification or hard latency
threshold.

## Deferred

Pickers, remembered grants, directory operations, watching, reload/merge,
overwrite confirmation, Save Copy, durable effect journals, multiple
documents, autosave, filesystem WASI, universal metadata preservation, Ropey,
and a replacement editor engine remain out of Gate B.
