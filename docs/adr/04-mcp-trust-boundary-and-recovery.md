---
type: adr
title: MCP trust boundary and recovery
description: MCP sources, per-host ownership, redaction and interruption recovery.
tags: [adr, mcp, security]
status: stable
locale: en
translation_key: adr-04-mcp-trust-boundary-and-recovery
---

# MCP trust boundary and recovery

## Background

MCP declarations can contain executable paths, arguments, URLs, environment variables, HTTP headers, and placeholders. The five hosts use distinct native formats, so one filesystem rename cannot update them atomically. Incorrect ownership inference can overwrite user configuration, while recording resolved definitions in journals can duplicate credentials. Personal input must be validated before projection writes derived state.

- Impact scope: author input validation, placeholder expansion, five serializers, native host files, per-host ownership, interruption recovery, and backup redaction.
- Real alternatives: pass unknown fields through, execute author-defined helpers, infer ownership from names, back up whole native files, or stop all projection when one server cannot resolve.
- Verifiable evidence: author input validation tests cover schema precedence and warnings; transaction and recovery tests cover fingerprints and divergent states; unresolved placeholders leave independent components available.

## Decision

The user-owned `~/.agents/mcp.json` uses `ccyncMcpServers` and optional `hostRoutes`. A legacy input remains valid until a manual switch. Conflicting, foreign or unreadable inputs stop MCP writes and retain the original. Imported originals are preserved and consent is bound to their local evidence. Unsafe originals are not copied into portable output. Merge precedence retains provenance and never turns a matching name into ownership.

Five native host writers manage only selected, precisely located copies with committed per-host proof. A same-name foreign entry blocks that host while other safe hosts may complete. `managed.json` is derived and is not ownership proof. `projected-state.json` records verified per-host ownership and migrated route intent. The write-ahead `projection.txn.json` records fingerprints and transaction metadata, not resolved definitions or secrets. An unreadable entry or unrecognized third fingerprint stops automatic recovery and retains the journal. Before and after matches are handled separately. Unchanged files retain their bytes.

Secrets may appear in the process environment, machine secret settings, the original source that already held them, and host files that require them. Declarations, portable exports, lockfiles, ownership, journals, diagnostics and backups must not acquire resolved secrets. Unsafe direct inputs, URL userinfo and placeholder collisions stop backup before output. Restore carries declarations and intent, never ownership or source exceptions. Its separate pending marker blocks other mutations until the same backup is resumed or a divergent destination is resolved.

The empty Windows support table blocks stdio publication. Host measurements and event capture are follow-up work. Evidence-file diagnosis is read-only and accepts sanitized versioned events. It does not infer causation from a window, process count or timing alone. See [MCP](../mcp.md) for procedures.

## Rejected alternatives

- Executing author-supplied helpers or passing unknown fields can expose credentials or execute untrusted code.
- Treating a generated manifest or matching name as ownership risks overwriting foreign entries.
- Persisting whole host files for rollback duplicates secrets and can overwrite later user edits.

## Consequences

Independent hosts can complete despite a conflict, but incomplete evidence prevents destructive recovery. A backup is a portable profile, not a full machine image. Live recovery remains separately authorized for each run.
