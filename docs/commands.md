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
| `init` | Initialize from one source agent and select targets | [Setup](setup.md) |
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

## Machine profile restore

`ccync restore --from <dir> [--dry-run]` restores the five machine profile files into the active storage layout. A backup must include `plugins.json`. Restore validates backup entries before writing, rejects links and case-folded filename collisions, and validates portable exports while leaving them out of the import. On a committed layout, restore regenerates the portable exports. Run `ccync sync` afterward to fetch remote sources. Local sources whose recorded path is missing are reported as `needs-rebind`. Restore does not import ownership. `--dry-run` reports drift and writes nothing.

## MCP routing and diagnosis

`ccync sync --dry-run` previews per-host decisions read-only; `ccync sync` applies safe changes. `ccync sync --help` lists both forms without changing configuration or acquiring the lifecycle lock. For one held item, use `ccync sync --reselect-mcp SOURCE SERVER HOST [--dry-run]` with the exact reported source, server and host. `ccync doctor [--dry-run] [--release-gate] [--mcp-evidence <path>]` accepts a sanitized event file. When an evidence file is supplied, doctor analyzes only that file instead of running ordinary checks. `ccync init <source>` previews imports before consent and may then project live. Use `ccync backup [--output <dir>] [--strip-secrets]` and `ccync restore --from <dir> [--dry-run]`. See [MCP](mcp.md) and [setup](setup.md).

## Binary self-update

`ccync update [--check]` updates only the `ccync` binary itself; it never modifies plugins. Pass `--check` to inspect the installed version, the latest upstream release, and the active installation channel without downloading files or writing to disk.

Only standalone binary installations managed via curl (Linux/macOS) or irm (Windows) support self-update through `ccync update`. If ccync was installed via Homebrew, WinGet, or Cargo, the command refuses self-update and instructs the operator to run the appropriate package manager. Before atomically replacing a standalone binary, ccync verifies the downloaded archive against `checksums.txt` using SHA-256.

## Plugin upgrade

`ccync upgrade [<plugin>] [--dry-run]` applies to ccync-managed personal plugins sourced from Git repositories. The command compares the upstream `HEAD` revision with the locally pinned commit. After downloading a complete new revision into the cache, ccync updates the pin and projects changes to target agents. Archives and plain directory snapshots lack comparable upstream branches and are skipped. Pass the management ID, such as `ccync upgrade plugin --dry-run`. A package name is not accepted as a selector; see [Package names and management IDs](plugins.md#package-names-and-management-ids).

A network connection failure or upgrade error for one plugin does not prevent other plugins from being evaluated and updated. Running with `--dry-run` does not clone repositories, update pinned versions, or trigger projection. The dry run returns exit code `3` when updates are available, `0` when all plugins are up to date, and `1` on error.

For adopted items, `ccync upgrade` reports version status without downloading or applying updates. When both upstream source metadata and a baseline version exist, the command displays `current → latest`. If source information is incomplete, it reports `version-unresolved`. MCP servers and loose skills do not support version comparison in this flow. To place an adopted source under full ccync management, first remove the adopted record with `ccync remove <id>`, then register the source explicitly with `ccync add <source>`. The `upgrade` command never alters source ownership.

## Adding a source

`ccync add <source> [--no-sync]` emits one stable intake token on stdout: `added`, `fetch-failed`, or `no-contribution`. The token describes intake, while the exit code reports the command result.

| Intake token | Meaning | Exit behavior |
| --- | --- | --- |
| `added` | The source contributed supported content and its catalog entry was committed. | With `--no-sync`, exits `0`. Otherwise, exits according to projection: `0` on success and `1` on projection failure. A projection failure does not remove the committed catalog entry. |
| `fetch-failed` | The source could not be fetched or inspected. | Exits `1`; no catalog entry is committed. |
| `no-contribution` | The source was fetched, but contains no supported components or MCP servers. | Exits `1`; `plugins.json` is left unchanged. |

Supported contribution locations are `skills/`, `commands/`, `agents/`, `hooks/`, `.mcp.json`, and `mcp.json`. Hooks in `hooks/` are accepted for loose intake but are not projected into native Claude Code or Codex packages. See [Native packages for Claude Code and Codex](plugins.md#native-packages-for-claude-code-and-codex). A non-empty MCP manifest must be valid. A corrupt manifest fails intake before catalog mutation. For direct, user-authored MCP servers, use `~/.agents/mcp.json`; ccync reads this optional input and writes it only on explicit restore. See [MCP definitions and the URL procedure](mcp.md#personal-list-and-routes) for how to handle a URL with no supported manifest.

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

`init` has no JSON output and is not part of the table above. Its exit codes are `0` when the first projection completes; `1` when the source is invalid or unreadable, a write fails, or the projection fails; and `64` when a prompt is cancelled, consent is missing, `--agents` is empty or names an unknown target, or a non-interactive run gives no source. A cancelled prompt or missing consent prints `nothing written`. A non-interactive run that lacks the source or the target list, or that names an unknown target, exits `64` with a message that names the missing or invalid input, and also writes nothing. A script that ran `ccync init claude` without consent before this change now exits `64` and must add `--agents` and `--yes`. On an uninitialized machine, `sync` exits `1`, not `64`, so the two commands differ today. Aligning `sync` with this matrix is recorded in [deferred work](deferred-work.md).

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

## Sync and init outcome reference

Use this table to read the human output of `sync` and `init`. Each reason is the exact text the CLI prints. [Setup](setup.md#publication-outcomes-and-retry) describes the recovery for each state.

| Output | Meaning | Exit | Next action |
| --- | --- | --- | --- |
| `published` | Every required route for the target was written or already matched. | `0` when the whole run succeeds | None. |
| `not published`, or `publication not published` | A required route was not written. | `1` | Fix the reported write error, then rerun `ccync sync`. |
| `not published; conflict: <reason>` | A route is blocked by a conflict. | `1` | Resolve the conflict, then rerun `ccync sync`. |
| `not published because unsupported: <reason>` | The target has no supported surface for this component kind. | Not a failure | None. The action reads `no write is needed for this unsupported route`. |
| `loader NotRun` | No loader check ran for the route. | Not applicable | Start the agent and confirm the component is listed. The state does not prove loading. |
| `loose conflict: <name> has N origins` | Init preview found one loose-skill name from several origins. Sync blocks the whole group. | Not applicable | Resolve the duplicate, then rerun `ccync sync`. |
| `mcp blocked: N server(s), not imported` | Init preview and completion list each blocked MCP definition with its reason. | Not applicable | Review each named definition and its reason in the source. |

### Standalone publication and withdrawal

`ccync sync` publishes standalone content, such as an adopted loose skill, into the skill root of each selected target. Sync and `ccync doctor` label these routes `standalone published`, `recovery blocked`, or `identity unresolved`. Withdrawal removes only a copy that ccync published, and it waits for proof from every known reader of a shared root. A reader that was never selected still withholds withdrawal. An interrupted publication or withdrawal keeps its journal and backup, and running `ccync sync` again retries it. These labels describe filesystem results and do not show that an agent loaded the component. See [Setup](setup.md#publication-outcomes-and-retry) for recovery.

## Instruction sync

`ccync sync --instructions-only [--dry-run] [--yes]` previews or applies only your instruction source, `~/.ccync/AGENTS.md`, to the current-write file of each selected target. Claude has no current-write file, so this source does not reach Claude's user scope. There is no `~/.claude/AGENTS.md` destination. After a successful source read, selecting `claude` also prunes a managed block from the legacy `~/.claude/CLAUDE.md` path, whether the source is absent or nonempty. An absent or empty legacy file stays absent or empty. Malformed markers and inaccessible paths remain untouched and block that path. The route does not run migration, tool-snapshot validation, catalog resolution, canonical render, plugin projection, or MCP work. It writes no `~/.ccync/` state. Without `--instructions-only`, `ccync sync` still projects instructions as one phase of a full sync.

### Preconditions

The route fails closed with exit `1` before it reads or writes any target file when one of these holds:

- The machine is not initialized.
- `~/.ccync/config.json` is missing, unreadable, or not valid JSON.
- The config sets both `selectedRuntimes` and `selectedProjectionTargets`. Run `ccync init <source>` to choose one selection.
- The config selects no valid target, or lists any rejected target value. A mixed list of valid and rejected values also fails. The message lists the rejected values.

### Targets and deduplication

ccync resolves selected targets in canonical order and removes duplicate physical paths. Six logical targets map to four current-write files, because the three Antigravity targets share one file. Claude has no current-write path.

| Target | Instruction file |
| --- | --- |
| Claude Code | No current-write path |
| Codex | `~/.codex/AGENTS.md` |
| Copilot | `~/.copilot/AGENTS.md` |
| OpenCode | `~/.config/opencode/AGENTS.md` |
| `agy-cli`, `agy-ide`, `agy-gui` | `~/.gemini/AGENTS.md` |

The legacy Claude path is prune-only. Ordinary sync checks it only when `claude` is selected. `ccync doctor` classifies it regardless of selection or config errors. `ccync uninstall` checks all current-write paths and the legacy Claude path regardless of selection, and removes a managed block only with ccync ownership proof.

Each current-write file is an independent write unit. Ordinary sync skips files for deselected targets and skips the legacy Claude path unless `claude` is selected.

### Source states

Only a missing source file or a zero-byte source file means that you want no instructions. In that case, ccync removes only the ccync-owned block from each selected file. A source that contains only whitespace is content, and ccync writes it.

A source that cannot be read, is not valid UTF-8, or is a dangling symbolic link never removes anything. Dry-run and apply list every resolvable path with the blocker `blocked: source read`, `blocked: source encoding`, or `blocked: dangling source link`. They end with `no files written` and exit `1`.

### Per-path states

| State | Meaning |
| --- | --- |
| `create` | The file does not exist, and ccync creates it with the block. |
| `append` | The file has no block, and ccync appends one. |
| `rewrite` | The block differs from the transformed source, and ccync replaces it. |
| `prune` | The source is absent and the file holds a block, so ccync removes it. |
| `unchanged` | The file already matches. ccync does not publish a write and keeps the modification time. |
| `blocked: target read` | The target file cannot be read. |
| `blocked: malformed marker` | The ccync markers in the target file are damaged. ccync never repairs them. |
| `blocked: dangling link` | The target is a symbolic link whose destination does not exist. |

ccync preserves every byte outside the owned block, including mixed line endings.

### Dry-run

`--dry-run` never prompts and writes no file. It prints `--- INSTRUCTIONS ONLY: DRY RUN ---`, one state line per deduplicated current-write path, and a separate `legacy instruction files (prune only)` group when Claude is selected. A blocker does not hide other paths. The exit code is `0` for a complete preview and `1` when any blocker appears.

### Consent and apply

Apply first prints the mode (`write` or `prune`), the deduplicated paths, and the planned state of each path. Then it needs authorization. Only these forms authorize writing:

- The `--yes` flag.
- The exact environment value `CCYNC_ASSUME_YES=1`.
- An interactive answer `y` or `yes`.

An unset, `0`, empty, or any other environment value refuses. A refusal, or a noninteractive session without authorization, writes nothing, exits `1`, and tells you to rerun with `--yes`. The scoped route does not write the projection lock, so each apply before the first full projection needs authorization again.

### Partial success and recovery

A target-local failure does not stop independent targets and does not roll back their writes. Apply prints one line per path, then `changed: N; unchanged: N; failed: N`. A blocked path counts as failed and ccync does not write it. The exit code is `1` when `failed` is above `0`.

To recover, fix the reported cause and run the same command again. Unchanged paths stay unchanged on the rerun.

| Reported cause | Recovery |
| --- | --- |
| Rejected config or target values | Run `ccync init <source>`, or remove the rejected values from `config.json`. |
| Source read or encoding failure | Fix the permissions of `~/.ccync/AGENTS.md`, or save it as UTF-8. |
| Dangling source link | Restore the intended source target and access. |
| Dangling target link | Preserve the link. Inspect its target and access, restore access, then rerun `ccync doctor`. |
| Target read failure | Fix the permissions of the target file. |
| Malformed marker | Manually repair the ccync marker boundaries without changing other file contents, then rerun `ccync doctor`. |

### Full sync

Full `ccync sync` uses the same source reader and target inspector. Its dry-run lists separate groups for current instruction files and legacy instruction files that would be pruned when Claude is selected. It lists the same per-path state or source blocker. The full dry-run still prints the whole report, including every other surface and every resolvable instruction path. Its exit code is `0` when the preview is complete and `1` when any instruction path has a blocker. If full sync cannot build machine options, it skips the instruction phase.

When the source fails during a full apply, ccync skips all instruction writes, keeps the other completed phases, reports the instruction failure on stderr, and exits `1`.

### Doctor

`ccync doctor` reads the strict config, source, and selected current-write files and writes nothing. It compares the transformed source with each selected file's block. It also classifies the legacy Claude path regardless of selection, config errors, or source errors. An absent or empty file, or a file without a ccync block, needs no action. A remaining managed block is a warning. When Claude is selected, remove only the complete managed block, including its markers, with `ccync sync --instructions-only --yes`. Otherwise, remove only the complete managed block and its markers by hand or run `ccync uninstall`. An unreadable file, malformed markers, or a dangling link is an error. For a dangling link, preserve it, inspect its target and access, restore access, then rerun `ccync doctor`. It reports these as errors with exit `1`:

- Config that is missing, unreadable, malformed, all-invalid, or mixed.
- A source read, encoding, or dangling-link failure.
- A target that is unreadable, has damaged markers, or is a dangling link.
- A missing block, a block that differs from the source, or a block that remains after the source was removed.
- A Codex override file, `AGENTS.override.md`, that cannot be read.

It reports these as warnings. Warnings alone exit `0`:

- A stale block in a deselected target.
- A readable, nonempty Codex `AGENTS.override.md` next to a managed block.

Every finding names its reason and a recovery action.

### Evidence boundary

| Evidence | What it proves |
| --- | --- |
| Dry-run | Predicts what apply would do, from the current files. |
| Apply | Reports whether the selected file changed, matched, or was blocked. |
| Doctor | Reports the read-only disk health of the files. |
| Real agent session | Only a session probe observes whether an agent loaded the instructions. |

No ccync command reports whether a running agent loaded the file. After a changed apply, reload or restart your agents. The agent load state is unknown to ccync. A skill name in your instruction source is not evidence that every host provides that skill.
