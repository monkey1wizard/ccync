---
type: guide
title: Setup, lifecycle, and diagnostics
description: Initialization, synchronization, machine state, backup, restore, diagnostics, and uninstall.
tags: [setup, lifecycle, doctor, backup]
status: stable
locale: en
translation_key: setup
---

# Setup, lifecycle, and diagnostics

## Initialization

`ccync init [<master>]` configures Claude Code (`claude`) or OpenAI Codex (`codex`) as the master agent. During initialization, ccync adopts existing plugins, loose skills, and MCP server definitions from that master agent, writes initial machine configuration to `~/.ccync/config.json`, and prompts for target agent selection. In interactive terminals, the command prompts for the master agent if omitted; in non-interactive scripts, the operator must specify `claude` or `codex` explicitly.

Initialization can proceed directly into the first live projection after adoption completes. Its target selection and write-scope disclosure apply modifications directly; they do not function as a dry run. A subsequent `sync --dry-run` cannot preview writes that initialization already performed. Prior to initialization, write commands including `sync`, `remove`, projecting `add`, and live `upgrade` halt without modifying machine state and direct the operator to run `ccync init <master>`.

Read-only operations such as `sync --dry-run` and `upgrade --dry-run` function before initialization. Similarly, `add --no-sync` registers a source without projecting it. The commands `list`, `show`, `doctor`, `search`, `repair`, `backup`, `restore`, `pin`, `update`, `cleanup`, and `uninstall` require no prior initialization. The existence of `~/.ccync/config.json` serves as the predicate confirming that the machine is initialized.

## Synchronization and the first-run write gate

`ccync sync [--dry-run] [--yes]` resolves plugin sources, compiles the shared canonical render tree, and projects configurations to selected target agents. A separate projection pipeline schedules updates for the five fixed MCP hosts, regardless of which agents are selected. Before writing live configuration for the first time, ccync displays a detailed manifest of target agent files and MCP host configurations scheduled for modification. Run `sync --dry-run` to inspect proposed changes in advance. Passing `--yes` or setting `CCYNC_ASSUME_YES=1` bypasses the interactive confirmation prompt.

If the operator declines the initial confirmation prompt, ccync aborts without modifying live agent configuration files. Preceding source downloads in the cache remain preserved. If a plugin cache directory is missing or fails to fetch, repair the source before projecting again. Certain sync or upgrade paths can misinterpret a missing source as an empty plugin, which risks incorrectly removing existing projected files from agent directories.

`config.json` records selected projection targets and machine settings. The canonical selection key is `selectedProjectionTargets`. Legacy keys `selectedRuntimes` and `runtimes` are recognized during configuration migrations but are never written back. MCP host updates operate independently of agent target selections: all five supported MCP hosts update even when their corresponding agent is not selected.

## User-scope instructions

When `~/.ccync/AGENTS.md` exists, ccync synchronizes its content into the `<!-- ccync:managed:begin -->` and `<!-- ccync:managed:end -->` marker blocks of each selected target file. User content outside these boundary markers remains untouched. Because all three Antigravity targets share `~/.gemini/AGENTS.md`, ccync updates that shared file exactly once per run. If a target file contains damaged or missing boundary markers, ccync reports the conflict and skips the file rather than overwriting it.

## Diagnostics and uninstall

`ccync doctor [--dry-run] [--release-gate]` evaluates configuration validity, machine state, generated render outputs, target agent files, and release readiness conditions. Diagnostic findings explicitly name the affected components. Currently, `--dry-run` and `--release-gate` do not introduce additional checks; neither flag should be treated as an independent release audit.

A pinned personal plugin whose cache exists but produces no projectable components triggers a `render-contribution` diagnostic warning. Unpinned items or unreadable catalog records represent distinct failure states and do not pass this check. Doctor errors indicate that ccync cannot operate correctly; warnings indicate that ccync functions but an agent workspace may be incomplete or inconsistent. Doctor emits warnings even when exiting with code 0.

`ccync uninstall` removes only MCP entries, skills, custom commands, custom agents, and managed instruction blocks backed by ccync ownership proof. It removes tool records and derived tool state, including `tools.json`, tool snapshots, the operation journal, and the install receipt. It also deletes derived `cache/` and `build/` data while preserving `config.json`, `plugins.json`, and native agent configuration directories. The persistent coordination file `build/lifecycle.lock` remains so concurrent commands continue to use the same lock. The command does not delete the `ccync` binary; remove the binary using the original package manager or installer.

## Exit codes

Exit code `0` indicates success, `1` indicates an operational failure, `2` indicates a recognized command that is not yet implemented, `3` indicates that a read-only query discovered available updates, and `64` indicates invalid command-line usage. `ccync update --check` operates in read-only mode and returns `0` upon query completion even when the installation channel does not support automated self-update. Attempting an unsupported binary `update` returns exit code `1`. See [ADR 05](adr/05-machine-lifecycle-adoption-and-cli-contract.md) for complete specifications.

## Machine state

The `~/.ccync/` directory contains machine-bound input, cached source repositories, and generated state. `config.json`, `plugins.json`, and optional `AGENTS.md` are input. `tools.json` is also machine-bound input, and uninstall removes it. Tool state includes committed snapshots, an operation journal, a persistent lifecycle lock at `build/lifecycle.lock`, and an install receipt. Other generated state includes `build/lock.json`, `render/`, `_mcpServers/`, MCP ownership records, write-ahead transaction journals, and active markers. Cached repositories and generated artifacts can be regenerated from their inputs. Committed tool snapshots regenerate only when you run `ccync tool refresh <id>`.

The native configuration files read by coding agents reside outside `~/.ccync/`. ccync manages only specific entries and demarcated marker blocks backed by ownership proof, never assuming ownership of entire host configuration files.

## Backup and restore

`ccync backup [--output <dir>]` generates a portable machine profile containing configuration inputs, adoption snapshots, and user instructions. It excludes `tools.json`, tool snapshots, tool state, the operation journal, `build/lifecycle.lock`, and the install receipt because these identities are machine-bound and v1 has no rebind command. By default, the command strips recognized credentials and substitutes sensitive MCP environment and header values with named placeholders in the format `${CCYNC_SECRET_<SERVER>_<KEY>}`. The `--strip-secrets` flag is maintained for backward compatibility and performs identical redactions. ccync refuses to generate a backup if it detects unmodeled plaintext credentials that cannot be safely redacted.

`ccync restore --from <dir> [--dry-run]` validates profile integrity before merging machine inputs. Running `--dry-run` reports matching, divergent, or unreadable items without writing to disk. The restore command does not overwrite native agent configuration files. After completing a restore, provision the required environment variables, install the team roster or personal sources, and run `sync` to generate fresh projections. When restoring profiles across different physical machines, verify external file paths and directory junctions before proceeding.
