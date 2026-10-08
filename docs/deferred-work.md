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

- Several adoption workflows still use standard file writes when updating the lockfile. Future refactoring should adopt the shared atomic write utility and add tests covering interruption recovery.
- During cache cleanup, ccync retains all candidate cache directories and issues a warning if it cannot establish an item's active version. Any automated cleanup heuristic must rely on verifiable ownership and version metadata rather than file modification timestamps.
- Identity rebind remains deferred. Reopen it when the manager can prove that a changed package identity refers to the previously owned installation without claiming a user-managed installation.
- Persisting the loaded tool generation remains deferred. Reopen it when a supported operation needs to detect or recover from a generation change across process restarts.
- `ccync upgrade tool:<id>` stays beta with every package-update gate `Gated`. Reopen it only after a live acceptance passes. The 2026-10-01 Windows run left three open findings: committed `tool refresh --json` and `upgrade --json` print projection prose on stdout before the JSON document, two `upgrade tool:rtk` attempts ended with manager exit `101` of unconfirmed cause while the upgrade path discards manager stderr, and recovery was only exercised with injected journals.

## Projection and host behavior

- In prior testing on physical machines, an OpenCode configuration path escaped the overridden home directory. Any fix requires end-to-end isolation tests that guarantee the operator's actual home directory cannot be modified.
- A third-party installer can follow an Antigravity directory junction and write directly into ccync's generated output. Before changing this boundary, determine whether ccync should protect the output root or whether a wrapper script should restrict writes to a single primary source.
- The two-phase publisher falls back to a physical directory copy when an initial symbolic-link attempt fails, but it does not subsequently retry creating symlinks. If this behavior changes, retries must stage replacements in a sibling directory prior to an atomic swap without removing the active root first.
- Launch-resolved MCP package references belong to `feat-mcp-first-class-intake`. Reopen this work in that feature when MCP package references are resolved at launch and their ownership and secret boundaries are defined.

## MCP and release checks

- The MCP `--dry-run` command could provide a more detailed per-host and per-server execution plan. If a secret placeholder cannot be resolved or a host file cannot be read, the affected component must be flagged as an error rather than counted as a success.
- Release checks depend heavily on the host platform and CI environment. In the absence of an appropriate CI runner, mark the check as `NotRun` rather than assuming verification passed. The static CRT linkage check on Windows remains an enforced release gate.
- The snapshot walker's Unix test suite and non-Windows code branches require automated execution on a Unix CI runner or WSL to establish cross-platform verification.
- WinGet remote update discovery remains deferred. Reopen it when ccync needs to discover updates from the remote WinGet source rather than relying on local package state.
- The non-Windows pipx and Homebrew Phase B proof remains deferred. Keep each unproven Phase B gate `gated`. Reopen the proof when a suitable non-Windows runner can exercise installation, update, and recovery for both package managers.
- Other lifecycle work outside the approved capability scope remains deferred. Reopen an item only when a supported platform, package manager, or user workflow requires it and its ownership, recovery, and secret handling can be verified. The final acceptance receipt lists the current gate state; this document does not repeat a gate list that could become stale.

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
