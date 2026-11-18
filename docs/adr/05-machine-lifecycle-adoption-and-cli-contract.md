---
type: adr
title: Machine lifecycle, adoption, and CLI contract
description: Explicit initialization, adoption, lifecycle gates, and stable command outcomes.
tags: [adr, lifecycle, cli]
status: stable
locale: en
translation_key: adr-05-machine-lifecycle-adoption-and-cli-contract
---

# Machine lifecycle, adoption, and CLI contract

## Background

ccync must import pre-existing components from one chosen source agent without assuming ownership of that agent's original files. Machine initialization and subsequent projections constitute distinct lifecycle stages. Automated scripts also require clear distinctions between operational failures, recognized but unwired commands, available update notifications, and command-line usage syntax errors.

- Impact scope: `init` component adoption, `sync` projection pipelines, `doctor` diagnostic health checks, binary `update` commands, and standard CLI exit codes.
- Real alternatives: trigger initialization implicitly on `sync`, treat adopted source files as ccync-owned, or return a single generic nonzero exit code for all failures.
- Verifiable evidence: `CommandKind::PUBLIC` and `ExitCode` definitions in `crates/ccync-engine/src/lib.rs`, `crates/ccync-cli/src/commands/init.rs`, and integration test suites in `crates/ccync-cli/src/main.rs`.

## Decision

ccync mandates explicit initialization, provenance tracking for adopted sources, and controlled projection.

1. `ccync init [<source>]` imports existing plugins, loose skills, and MCP server definitions from one designated import source (`claude`, `codex`, `copilot`, `opencode`, or `agy`), creates initial machine configuration, and prompts for target agent selection. Adopted items retain complete source metadata. Adoption never grants ccync ownership over the source agent's original files, and the source is not a permanent master agent. Because initialization can continue directly into the initial projection, the operator must review planned target modifications.
2. `ccync sync [--dry-run] [--yes]` compiles the canonical render tree from registered sources and machine state, then projects it to selected native agent configurations. `--dry-run` previews synchronization changes, but cannot preview modifications executed during `init`. The initial write operation requires explicit user confirmation.
3. `ccync doctor` verifies machine health. `ccync update` exclusively updates the `ccync` binary; it never modifies plugins or projections. `ccync upgrade` manages plugin version updates.

The `ExitCode` contract distinguishes the following execution outcomes:

| Value | Variant | Meaning |
| --- | --- | --- |
| `0` | `Success` | The operation succeeded; `doctor` may have warnings but no errors. |
| `1` | `Error` | The operation failed, including install, render, uninstall, or `doctor` errors. |
| `2` | `NotWired` | A recognized command is not yet wired; this is not a usage error. |
| `3` | `UpdatesAvailable` | A designated read-only query found available updates; this is not a general error. |
| `64` | `Usage` | The command is unknown or missing, or its arguments are invalid. |

## Rejected alternatives

1. **Implicit initialization during `sync`:** this would generate local machine state before the operator selects an import source, compromising the initial confirmation boundary.
2. **Treating imported files as ccync-owned:** this would permit subsequent sync or removal operations to inadvertently delete pre-existing user configurations.
3. **Returning a single nonzero code for all failures:** this would prevent automation scripts from distinguishing available updates from execution failures or command usage errors.

## Consequences

- Adopting components does not convert original source files into removable projections; withdrawing items requires positive ownership proof.
- Initialization can proceed immediately into the first projection; a later `sync --dry-run` cannot preview modifications already applied during initialization.
- Automated scripts must interpret exit code `3` as a successful update query result and `64` as a command usage error. Code `2` must not be parsed as a usage error.
- Explicit initialization, first-write confirmation gates, and ownership boundaries remain mandatory architectural invariants. See [Setup](../setup.md) and [Adoption](../adoption.md) for operational details.
- `ccync init` now previews every import and target, then requires consent before any write. A refused or cancelled init writes nothing and exits `64`. Scripts that ran `ccync init <master>` without consent must add `--agents` and `--yes` (or `CCYNC_ASSUME_YES=1`). The write order after consent is `plugins.json` (the `_adoption` record and target snapshot), then `config.json`, then projection. A failure between the first two writes is repaired by repeating init; sync refuses on the snapshot mismatch until then.
