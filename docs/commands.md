---
type: reference
title: ccync command reference
description: Uses of the 19 public commands and the limits of update, upgrade, pin, and cleanup.
tags: [commands, update, upgrade, cleanup]
status: stable
locale: en
translation_key: commands
---

# ccync command reference

## Public commands

ccync provides 19 public CLI commands. The following table lists each command, its operational purpose, and the primary document defining its behavior. Run `ccync <command> --help` to inspect specific command-line arguments and options.

| Command | Purpose | Behavior |
| --- | --- | --- |
| `init` | Initialize and select a master agent | [Setup](setup.md) |
| `sync` | Build the render and project to native surfaces | [Setup](setup.md) |
| `add` | Register a personal plugin source | [Plugins](plugins.md) |
| `search` | Discover a source to add | [Plugins](plugins.md) |
| `repair` | Record a missing adopted-item source | [Plugins](plugins.md) |
| `remove` | Remove a managed item | [Plugins](plugins.md) |
| `list` | List personal and adopted items | [Plugins](plugins.md) |
| `show` | Inspect one item's source and status | [Plugins](plugins.md) |
| `doctor` | Diagnose machine state | [Setup](setup.md) |
| `backup` | Create a portable machine profile | [Setup](setup.md) |
| `restore` | Validate and merge a machine profile | [Setup](setup.md) |
| `uninstall` | Withdraw proven ccync-managed content | [Setup](setup.md) |
| `update` | Check or replace the ccync binary | This document, “Binary self-update” |
| `upgrade` | Check or upgrade plugins | This document, “Plugin upgrade” |
| `cleanup` | Remove unreferenced plugin cache | This document, “Pin and cleanup” |
| `pin` | Hold or release a plugin | This document, “Pin and cleanup” |
| `roster` | Export or install a team plugin list | [Plugins](plugins.md) |
| `tool` | Discover, track, inspect, hold, or refresh external tools | This document, “External tools” |
| `outdated` | Check managed plugins and tracked tools for updates | This document, “External tools” |

Command outcomes follow the standardized `ExitCode` contract: `0` indicates success, `1` indicates an operational error, `2` indicates a recognized but unwired command, and `64` indicates invalid usage. Lifecycle commands also use `3` for actionable updates, drift, ambiguity, blocked operations, confirmation, or recovery as listed in the lifecycle matrix below. See [ADR 05](adr/05-machine-lifecycle-adoption-and-cli-contract.md) for the original command contract and [Setup](setup.md) for operational context.

## Binary self-update

`ccync update [--check]` updates only the `ccync` binary itself; it never modifies plugins. Pass `--check` to inspect the installed version, the latest upstream release, and the active installation channel without downloading files or writing to disk.

Only standalone binary installations managed via curl (Linux/macOS) or irm (Windows) support self-update through `ccync update`. If ccync was installed via Homebrew, WinGet, or Cargo, the command refuses self-update and instructs the operator to run the appropriate package manager. Before atomically replacing a standalone binary, ccync verifies the downloaded archive against `checksums.txt` using SHA-256.

## Plugin upgrade

`ccync upgrade [<plugin>] [--dry-run]` applies to ccync-managed personal plugins sourced from Git repositories. The command compares the upstream `HEAD` revision with the locally pinned commit. After downloading a complete new revision into the cache, ccync updates the pin and projects changes to target agents. Archives and plain directory snapshots lack comparable upstream branches and are skipped.

A network connection failure or upgrade error for one plugin does not prevent other plugins from being evaluated and updated. Running with `--dry-run` does not clone repositories, update pinned versions, or trigger projection. The dry run returns exit code `3` when updates are available, `0` when all plugins are up to date, and `1` on error.

For adopted items, `ccync upgrade` reports version status without downloading or applying updates. When both upstream source metadata and a baseline version exist, the command displays `current → latest`. If source information is incomplete, it reports `version-unresolved`. MCP servers and loose skills do not support version comparison in this flow. To place an adopted source under full ccync management, first remove the adopted record with `ccync remove <id>`, then register the source explicitly with `ccync add <source>`. The `upgrade` command never alters source ownership.

## External tools

`ccync tool discover <command-or-path> [--online] [--json]` selects the exact executable name or path. It does not select by package name. Discovery is offline by default. Pass `--online` to allow network access and package-manager cache reads or updates. Ambiguous matches and probe errors are reported; ccync does not choose an arbitrary candidate. `ccync tool track <id> ... [--yes]` records a discovered tool. For Cargo and interpreter-pip tools, status and upgrade reconstruct the adapter from the validated recorded executable, source, interpreter, distribution, delivery, and manager-takeover identity. Unsupported recorded adapter kinds fail closed.

The selector compares executable identity, not package names. On Windows it honors `PATHEXT` case-insensitively, strips verbatim path prefixes, and keeps UNC share and directory identity. A path selector compares the canonical executable file identity. On Unix, path and executable matching is case-sensitive. The candidate list is deterministic. Discovery does not guess when more than one candidate remains.

`ccync pin tool:<id>` holds automatic tool upgrades for a tracked tool. Run `ccync pin --remove tool:<id>` to release the hold. A held tool is skipped by automatic upgrades. `ccync tool refresh <id> [--dry-run] [--json]` reconciles the observed installation with the committed snapshot and projects the updated snapshot when applicable. State reports separate local installation evidence from remote update observations. Offline and local checks preserve the prior remote result and check time as stale. Online checks re-probe local evidence, replace the remote result with `Current`, `UpdateAvailable`, `Unsupported`, `NotChecked`, or `CheckFailed`, and mark freshness fresh. A completed or failed online attempt updates `last_check`. An older update result is never carried into a fresh check.

Tracking and refresh may change machine records or projections. ccync requests confirmation before live changes. Pass `--yes` to accept the displayed operation where supported. `--dry-run` previews refresh without committing records or projecting files. Discovery without `--online` uses local evidence and makes no network requests. Online checks can query package-manager state. Cargo remote checks accept credential-free HTTPS Git sources with a valid installed commit fragment or canonical crates.io sources. Interpreter-pip checks use the recorded interpreter and normalized distribution. Other adapters report unsupported or not checked unless their remote result is proven. `ccync outdated [--offline] [--json]` checks tracked tools and managed plugins; offline mode does not query remote state. Exit code `3` means a fresh update or actionable external change was found, `0` means no action is known, `1` means an operational error, and `64` means invalid usage. A remote `CheckFailed` is an operational error, not proof of an update.

> **Beta: `ccync upgrade tool:<id>` is not available for production use.** As of 2026-10-01, every package-update gate in the production build is `Gated`. This includes Cargo (used by `rtk`) and interpreter-pip (used by Graphify). The command returns `blocked` with exit code `3` before it runs any package manager, and `--dry-run` is blocked the same way. Detection by `ccync outdated` and reconciliation by `ccync tool refresh <id>` do not depend on this command.
>
> Authorized live acceptance on Windows did not pass, so the gates stay closed. Known limitations from that run:
>
> - With `--json`, a committed `tool refresh` or `upgrade` prints projection summary lines on stdout before the JSON document. In that path, stdout is not a single JSON document.
> - Two `ccync upgrade tool:rtk` attempts ended with manager exit code `101`. The cause is unconfirmed, and the upgrade path does not show the manager's stderr.
> - Recovery was exercised with injected journals only, not with a naturally interrupted operation.
>
> Behavior below describes the intended contract of this beta feature. Evidence is in the acceptance receipt `.dev/plans/fix-external-tool-lifecycle-remaining-gaps/real-machine/final.md`.

Tool upgrades use `ccync upgrade tool:<id> [--dry-run] [--yes] [--json]`. A tool adapter can report capabilities that are unsupported; in that case ccync does not attempt that operation. External updates are detected by `outdated`; run `ccync tool refresh <id>` to reconcile them. After a tool upgrade, refresh the snapshot when the report requests it. When a refresh or tool upgrade updates projections, ccync reports affected agents and advises reloading them because running agents do not automatically reload changed tool generations.

If a package manager may have completed an operation but ccync cannot verify the result, ccync records `manager-outcome-unknown` and stops. Inspect the tool and manager state before recovery. Use `ccync upgrade tool:<id> --retry-manager-step` only for that tool's recorded unknown manager outcome. This recovery-only flag retries the manager step after explicit confirmation. Do not treat it as a general retry option. Batch upgrade never retries an unknown manager outcome automatically. After recovery, run `ccync tool refresh <id>` to reconcile the committed snapshot and projection.

Tool snapshots are committed records. Installing or upgrading a package does not itself regenerate a committed snapshot. Use `ccync tool refresh <id>` to create or reconcile one. Snapshot projection follows the same ownership checks as other managed content.

Lifecycle commands keep diagnostics on stderr and reserve stdout for requested machine-readable JSON or command data. JSON results preserve typed outcomes such as `Current`, `UpdateAvailable`, `Unsupported`, `NotChecked`, and `CheckFailed`; they do not turn an unproven result into `Current`. The command exit code follows the outcome matrix: successful inspection or no known action returns `0`, discovered updates or actionable drift return `3`, operational or incomplete probe failures return `1`, and invalid usage returns `64`. JSON output does not suppress these exit codes.

A new update is eligible for `ccync upgrade tool:<id>` only when a fresh typed remote check proves `UpdateAvailable`. Unsupported, unchecked, failed, stale, or ambiguous evidence fails closed and cannot authorize package-manager execution. The package operation records intent and result in the lifecycle journal. If its outcome is unknown, ccync stops and requires inspection. Recovery can continue only from a valid journal for the same tool and operation. Cleanup and replacement require validated snapshot ownership proof, including the recorded generation identity; names or package-manager listings alone do not authorize deletion. Interrupted cleanup resumes from its recorded plan and proven inventory. Unknown or conflicting proof preserves the affected files.

Lifecycle capability gates use two stages. A capability can become supported only after the isolated acceptance cells and their receipt pass. Until then, the adapter gate remains `Gated`, and the CLI does not execute that package update. `--yes` accepts a displayed operation where supported; it does not grant implicit authorization for unrelated live mutation or recovery. Without the required live authorization, mutation, recovery, projection, and restoration steps remain unrun.

### CLI outcome contract

For valid invocations, JSON-capable commands emit one document on stdout and diagnostics on stderr. Human output follows this matrix. Invalid usage returns `64` and reports diagnostics on stderr. `tool track` and `tool untrack` reject `--json` with `64`.

| Command | Outcomes | Human streams | JSON |
| --- | --- | --- | --- |
| `tool discover` | `selected` (0), `not-found` (3), `ambiguous` (3), `probe-error` (1) | Result on stdout; incomplete probe diagnostics and retry on stderr. | Supported; candidates sorted by adapter, canonical executable path, candidate ID. |
| `outdated` | `current`, `unsupported`, `not-checked` (0); `update-available`, `actionable-drift` (3); `check-failed` (1) | Status on stdout; per-tool check diagnostics on stderr. | Supported. |
| `tool track` | `tracked` (0); `not-found`, `ambiguous`, `blocked`, `confirmation-required` (3); `probe-error` (1) | Success on stdout; diagnostics and confirmation on stderr; no reload advice. | Unsupported. |
| `tool untrack` | `untracked` (0); `not-found`, `blocked`, `confirmation-required`, `recovery-required` (3) | Success on stdout; diagnostics and recovery on stderr. | Unsupported. |
| `tool refresh` | `committed`, `no-op`, `dry-run` (0); `blocked`, `recovery-required` (3); `check-failed` (1) | Primary outcome on stdout; diagnostics/recovery on stderr. | Supported. |
| `upgrade tool:<id>` (beta) | `committed`, `current`, `dry-run` (0); `blocked`, `recovery-required` (3); `check-failed` (1) | Primary outcome on stdout; diagnostics/recovery on stderr. | Supported. |

The common JSON envelope contains `schemaVersion: 1`, `command`, `outcome`, `selectionState`, `updateState`, `updateFreshness`, `nextAction`, `affectedAgents`, `projectionChanged`, and `reloadAdvice`. `outcome` is a lower-case string from the matrix. Applicable selection/update states are lower-case strings, otherwise `null`. Remote states include `current`, `update-available`, `unsupported`, `not-checked`, and `check-failed`. `updateFreshness` is `fresh`, `stale`, or `null` when not applicable. `nextAction` and `reloadAdvice` are strings or `null`. `affectedAgents` is always an array of strings, empty when unknown. `projectionChanged` is always boolean.

Only successful `committed` or `untracked` outcomes with an actual projection change advise reload. No-op, dry-run, failure, blocked, and recovery-required outcomes never advise reload. `loaded generation unknown` accompanies reload advice only after a successful projection change.

Use an exact selector, then the reported candidate ID to resolve ambiguity. These commands inspect candidates and record the selected tool after confirmation:

```powershell
ccync tool discover graphify --json
ccync tool discover C:\Python314\Scripts\graphify.exe --json
ccync tool track graphify --command graphify --candidate <candidate-id> --recipe graphify-agents --yes
```

On Windows, `graphify`, `graphify.exe`, and case variants match the same executable basename when `.EXE` is allowed by `PATHEXT`. A bare selector can still match separate directories and remain ambiguous. An absolute selector identifies the canonical executable. It preserves UNC share/directory identity after stripping a verbatim prefix. Unix matching is case-sensitive. Probe errors name the adapter, distinguish incomplete results, and provide retry guidance instead of becoming `not-found`.

For new upgrades, ccync checks remote eligibility. Recovery first validates an existing same-tool journal. Known manager success continues without rerunning the manager. An unknown manager outcome requires `--retry-manager-step` and a validated journal matching tool, operation, intent, and manifest. Committed operations are recognized as complete. See [Architecture](architecture.md) for ownership and cleanup recovery.


## Pin and cleanup

`ccync pin <id>` marks a plugin as `held`, which pauses automated version checks and re-cloning. Run `ccync pin --remove <id>` to release the hold, and run `ccync pin --list` to display all currently pinned items.

`ccync pin tool:<id>` holds automatic tool upgrades for a tracked tool. Run `ccync pin --remove tool:<id>` to release the hold. Pinning updates the tool declaration, but does not install or upgrade the tool.

`ccync cleanup [--dry-run]` deletes plugin cache directories under `~/.ccync/cache/` that are no longer referenced by the lockfile. If the lockfile is corrupt or unreadable, ccync removes nothing. If ccync cannot verify the active version of an item and multiple candidate cache directories exist, it retains all candidates and issues a warning. Run `ccync cleanup --dry-run` to preview candidate cache deletions before executing cleanup.
