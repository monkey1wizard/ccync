---
type: reference
title: Deferred engineering work
description: Confirmed gaps, limitations, and conditions for reopening engineering decisions.
tags: [debt, deferred, limitations]
status: stable
locale: en
translation_key: deferred-work
---

# Deferred engineering work

This document tracks verified technical debt, engineering gaps, and deferred architectural improvements across the codebase. Entries represent known items rather than commitments to immediate delivery. When an item is resolved, the responsible maintainer must update or remove its entry.

## Machine state and atomic writes

- **P7 live-surface relocation remains deferred.** Until host registrations can be safely re-established with verified ownership and recovery evidence, registered paths under `~/.ccync` stay in place. This includes `plugins/`, source links, `build/render/`, `build/active/`, `data/`, and installer receipts.
- **P5 package snapshot export remains deferred.** Portable package snapshots must be derived from private state, admitted through the closed portability rules, and written after private state. They must not become routing or ownership authority. Keep credentials and unclassified data private.
- Several adoption workflows still use standard file writes when updating the lockfile. Future refactoring should adopt the shared atomic write utility and add tests covering interruption recovery.
- During cache cleanup, ccync retains all candidate cache directories and issues a warning if it cannot establish an item's active version. Any automated cleanup heuristic must rely on verifiable ownership and version metadata rather than file modification timestamps.
- Identity rebind remains deferred. Reopen it when the manager can prove that a changed package identity refers to the previously owned installation without claiming a user-managed installation.
- Persisting the loaded tool generation remains deferred. Reopen it when a supported operation needs to detect or recover from a generation change across process restarts.
- `ccync upgrade tool:<id>` stays beta with every package-update gate `Gated`. Reopen it only after a live acceptance passes. The 2026-10-01 Windows run left three open findings: committed `tool refresh --json` and `upgrade --json` print projection prose on stdout before the JSON document, two `upgrade tool:rtk` attempts ended with manager exit `101` of unconfirmed cause while the upgrade path discards manager stderr, and recovery was only exercised with injected journals.

## Projection and host behavior

- `ccync sync` exits `1` on an uninitialized machine, while `ccync init` refusals and cancellations exit `64`. Align `sync` with the init exit-code matrix in [CLI outcome contract](commands.md#cli-outcome-contract) when a script needs one code for both. Reopen it when an operator reports a script that cannot tell the two apart.
- In prior testing on physical machines, an OpenCode configuration path escaped the overridden home directory. Any fix requires end-to-end isolation tests that guarantee the operator's actual home directory cannot be modified.
- A third-party installer can follow an Antigravity directory junction and write directly into ccync's generated output. Before changing this boundary, determine whether ccync should protect the output root or whether a wrapper script should restrict writes to a single primary source.
- The two-phase publisher falls back to a physical directory copy when an initial symbolic-link attempt fails, but it does not subsequently retry creating symlinks. If this behavior changes, retries must stage replacements in a sibling directory prior to an atomic swap without removing the active root first.
- Launch-resolved MCP package references belong to `feat-mcp-first-class-intake`. Reopen this work in that feature when MCP package references are resolved at launch and their ownership and secret boundaries are defined.

## MCP and release checks

- The MCP `--dry-run` command could provide a more detailed per-host and per-server execution plan. If a secret placeholder cannot be resolved or a host file cannot be read, the affected component must be flagged as an error rather than counted as a success.
- **P7 codegraph and Graphify handoff remains pending.** Plan codegraph intake through the new personal MCP list, explicit hosts and a full preview. The Graphify uv migration is not a prerequisite. Record the implementation commit and build, installed binary version and hash, qualification results and evidence for each host combination, and each source, destination, owner, gap and unknown. Planning and fixture results do not qualify a host or prove live connectivity.
- The 2026-11-12 qualification check reported all five Native hosts Blocked and produced no candidates. The bilingual recovery replay used isolated fixtures and did not recover real history. Recheck the retained evidence before making a live decision.
- Inventory the Graphify external package, tracking record, source skill, each projected copy and rollback snapshots separately, including ownership and proposed disposition. Matching names or contents grant no withdrawal rights. Preserve foreign content and snapshots still referenced by active or rollback state. The codebase-memory MCP is a separate decision. Each installation, deployment, sync, recovery or removal requires authorization for that exact operation. No such operation is implied by this handoff.
- Hand off new layout and RTK tee observations as read-only evidence for separate decisions. Historical D-68 cleanup remains resolved. Do not infer new residue or treat an unexecuted safety premise as proven.

### Offline MCP recovery checklist

Before live-surface recovery, ask the user for prior records from the same user and host. A history count alone cannot identify a published server. Records from another account or host are leads only. Do not copy credentials or resolved secret values into the checklist.

1. Keep the supplied records unchanged. Record each source location and digest, the user, host and server name. If no server name is available, retain a host-level row and identify the missing evidence.
2. On each isolated host, run `ccync sync --dry-run` and compare the full preview with the prior records row by row. Record the current state, explicit choice, possible additional process count and pending action. For stdio, each newly active host session may start one process. A preview is not proof that a process started or connected.
3. Classify each row as confirmed missing, insufficient history, disabled or excluded, present, or same-name conflict. Confirm a missing item only when same-user, same-host evidence identifies the previously published source and server and the current state shows it missing. A different source with the same name is a conflict, not proof of the missing item's presence. Keep unresolved rows and unknown changes pending.
4. Review the entire proposed diff, including other hosts and items. Checking writes no config and selects nothing for recovery. Only after separate authorization for one item, preview `ccync sync --reselect-mcp SOURCE SERVER HOST --dry-run`, review its full diff, and use the same command without `--dry-run` for that item if authorized. Normal publication checks still apply. Do not select other items implicitly.

The following fictional evidence illustrates the checklist. `A` and `B` are isolated host environments for the same user. `sha256:a` and `sha256:b` stand for recorded digests, not literal SHA-256 values. Each source location identifies the supplied prior record, not a command to create it. Preview observations are assumed for this example.

| Prior record and user | Host | Server | Preview/current state and choice | Classification | Possible new processes | Pending action |
| --- | --- | --- | --- | --- | --- | --- |
| `records/A`, `sha256:a`, user `lee` | `claude` | `atlas` | Previously published from source `personal`; now missing and held | confirmed missing | 1 per active session if selected | Review full diff and seek separate authorization for `personal atlas claude` |
| `records/A`, `sha256:a`, user `lee` | `claude` | `birch` | Previously published; now explicitly disabled | disabled or excluded | 0 while disabled | Keep disabled |
| `records/A`, `sha256:a`, user `lee` | `claude` | `cedar` | Previously published and still present | present | 0 for unchanged entry | No recovery selection |
| `records/B`, `sha256:b`, user `lee` | `codex` | `atlas` | Previously published from `personal`; another source owns the present `atlas` | same-name conflict | unknown until resolved | Resolve identity and conflict before considering recovery |
| `records/B`, `sha256:b`, user `lee` | `codex` | unknown | History count only, no server name | insufficient history | unknown | Keep host-level row and obtain a named record |

For the first row only, the separately authorized operation is `ccync sync --reselect-mcp personal atlas claude`. The other rows remain pending or unchanged. These examples do not establish actual recovery candidates on any machine.
- Release checks depend heavily on the host platform and CI environment. In the absence of an appropriate CI runner, mark the check as `NotRun` rather than assuming verification passed. The static CRT linkage check on Windows remains an enforced release gate.
- Hosted CI runs the full workspace tests with no name-based skips on Ubuntu and Windows, including inside the exported public snapshot.
- WinGet remote update discovery remains deferred. Reopen it when ccync needs to discover updates from the remote WinGet source rather than relying on local package state.
- The non-Windows pipx and Homebrew Phase B proof remains deferred. Keep each unproven Phase B gate `gated`. Reopen the proof when a suitable non-Windows runner can exercise installation, update, and recovery for both package managers.
- Other lifecycle work outside the approved capability scope remains deferred. Reopen an item only when a supported platform, package manager, or user workflow requires it and its ownership, recovery, and secret handling can be verified. The final acceptance receipt lists the current gate state; this document does not repeat a gate list that could become stale.

## Standalone copies of imported skills

- `crates/ccync-cli/tests/sync_cli_matrix.rs` expects that a skill imported from one source agent root is published as a standalone copy into every selected reader root except the root that already holds it. The registry records each copy in `directSkillProjectionPaths`. Repeated `sync` keeps the copies, and `uninstall` removes them.
- The expectation records current behavior. It is not a confirmed design decision. The plan text allows standalone publication with "may" and names no source agent root, so the owner has not confirmed that a Claude root skill should appear in `~/.agents/skills` and in the two `~/.gemini` skill roots. The test change is a separate commit and can be reverted on its own.
- Not verified: whether `sync` restores a copy that a user deleted, how deselecting a target, selecting it again, and removing the source treat these copies, and whether `sync` regenerates the copies from the canonical source or only `init` writes them.
- Reopen this item when the owner decides the intended behavior. If the copies are not intended, revert the test expectation and change the engine so that imported skills stay in their own root. If they are intended, state the rule in [Plugins](plugins.md) and keep the expectation.

When any of these behaviors changes, update the authoritative topic document: [Setup](setup.md), [Architecture](architecture.md), [MCP](mcp.md), or [Commands](commands.md).

### Legacy backlog inventory

The former developer guide recorded the following engineering backlog items. This index preserves their original identifiers and findings. Complete historical context remains available via `git show cc40ecd6:docs/devguide.md` under “Open engineering backlog.” In this table, `open` indicates an item whose fix has not yet been verified, while `partial` indicates that only a specific subcomponent was addressed. Maintainers must verify current source code before acting on an entry.

| ID | Follow-up | State |
| --- | --- | --- |
| 1 | PowerShell `irm \| iex` installer leaks variables and functions into the caller. | open |
| 2 | Codex TOML string escaping does not cover control characters. | open |
| 3 | Codex MCP uninstall does not consult `unaddressable` ownership state. | open |
| 4 | An empty desired MCP manifest can insert `"mcpServers": {}` into a host file. | open |
| 5 | Atomic writes use a fixed temporary sibling that concurrent processes can collide on. | open |
| 6 | The snapshot walker loads an unbounded source tree into memory. | open |
| 7 | A second, unused author-facing `.mcp.json` parser remains in the protected projection module. | open |
| 8 | Source snapshots omit empty directories. | open |
| 9 | A temporary cache directory can satisfy the fetcher's existing-cache prefix test. | open |
| 10 | Deselection cleanup treats Copilot agents differently from other targets. | open |
| 11 | Canonical render failure now returns an error; managed-MCP warning-only handling remains. | partial |
| 12 | Refresh and sync timestamp differences lack byte-level regression coverage. | open |
| 13 | `devMode` and `ccyncRoot` are still parsed although they do not select a production mode. | open |
| 14 | `doctor --dry-run` and `--release-gate` are accepted but do not vary checks. | open |
| 15 | Rust identifiers cited in documentation have no continuous existence check. | open |
| 16 | Several adoption lockfile mutations remain non-atomic. | open |
| 17 | Line-based grep allowlists can hide a retired concept in another clause on the same line. | open |
| 18 | Line-number-based allowlists drift when earlier content changes. | open |
| 19 | Residue probes disagree on whether generated agent-instruction adapters are in scope. | open |
| 20 | A stable-only CI job cannot be fully exercised by a release-candidate rehearsal. | open |
| 21-release | The changelog extractor recognizes a narrower section boundary than its stated contract. | open |
| 22 | Workflow trust-surface probes establish presence but not exclusivity. | open |
| 23 | `github.ref_name` expands into three shell blocks in signing. | open |
| 24 | A helper wired later can retain an obsolete `allow(dead_code)` and “unwired” comment. | open |
| 25 | A cross-task test-plan row can be deferred until no task runs it. | open |
| 26 | A prohibited-literal criterion can conflict with a required test fixture containing that literal. | open |
| 27 | A directory-derived manifest can become stale after a later step writes to that directory. | open |
| 28 | Tests that copy a health check's logic may miss a production regression. | open |
| 29 | A retirement residue allowlist does not distinguish dead references from live ones. | open |
| 30 | Present-tense backlog closure notes can become false after later changes. | open |
| 31 | `ProjectionEvidence` has an unenforced invariant that can turn a successful run into exit 1 with no report. | open |
| 32 | Retained `build/mcp/managed.json` can keep adopted MCP environment or header values after uninstall. | open |
| 33 | An Antigravity root published as a copy never retries symbolic-link capability. | open |
| 34 | Tests of only intermediate data may miss errors in the user-facing result. | open |
| 35 | Narrowing a requirement in the manual can hide an implementation gap. | open |
| 21-cache | Missing plugin cache can be absorbed before the next sync removes a projection. | open |
| 36 | `ccync remove` leaves orphaned `~/.ccync/data/<id>/` directories. | open |
| 37 | Per-target MCP `cwd` support has platform-dependent gaps. | open |
| 38 | Serializer-local unresolved-secret skipping is unreachable for secret-class names after earlier manifest failure. | open |
| 39 | A test-plan row's declared type is not proven by its task's scoped command. | open |
| 40 | Doctor cannot distinguish a non-secret placeholder resolved on one host from one unresolved on another. | open |
| 41 | OpenCode agents are absent from `managed_surface_roots`. | open |
| 42 | Later plugins can overwrite same-named commands or agents in the canonical render. | open |
| 43 | `decompose_plugin` is used only by tests. | open |
| 44 | Backup placeholder allowance accepts only the `${VAR}` form. | open |
