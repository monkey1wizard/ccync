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

`ccync init [<source>]` takes one source agent: `claude`, `codex`, `copilot`, `opencode`, or `agy`. It scans that agent's installed plugins, loose skills, and MCP server definitions without writing to it, then prints a preview of what it will import and which targets it will write. Nothing is written until the operator confirms. After confirmation, ccync records the adoption, writes the target selection to `~/.ccync/config.json`, and runs the first projection. In interactive terminals, the command prompts for the source and the targets when they are omitted. In non-interactive scripts, the operator must pass the source, `--agents <list>`, and `--yes` or `CCYNC_ASSUME_YES=1`.

The preview labels a source as `source state: offline` when its files are readable but its command is not on `PATH`. Import still proceeds, and loader verification for that source stays `NotRun`. The preview also lists each shared root that a selected target reaches, with every consumer of that root, including targets you did not select.

The preview and the completion report also name each MCP definition that the scan blocked, with its reason. They count disabled definitions on a separate line:

```text
mcp blocked: 1 server(s), not imported; mcp disabled: 1 server(s)
```

A blocked definition is not imported, and other valid items from the same source still import. The report shows names, reasons, and config paths. It never shows credential values. When two loose skills share one name but come from different origins, sync blocks the whole group. See [Loose-skill conflicts and rescans](adoption.md#loose-skill-conflicts-and-rescans).

`--agents` accepts the five agent names, `all`, and the seven target keys `claude`, `codex`, `copilot`, `opencode`, `agy-cli`, `agy-ide`, and `agy-gui`. `agy` expands to its three surfaces. A target-key list keeps a partial AGY subset as written. An empty list is refused.

Scripts written for the earlier form, `ccync init claude`, proceeded without consent. They now stop with exit code `64` until they add `--agents` and `--yes`. See [CLI outcome contract](commands.md#cli-outcome-contract) for the exit codes.

Initialization can proceed directly into the first live projection after adoption completes. Its target selection and write-scope disclosure apply modifications directly; they do not function as a dry run. A subsequent `sync --dry-run` cannot preview writes that initialization already performed. Prior to initialization, write commands including `sync`, `remove`, projecting `add`, and live `upgrade` halt without modifying machine state and direct the operator to run `ccync init <source>`.

Read-only operations such as `sync --dry-run` and `upgrade --dry-run` function before initialization. Similarly, `add --no-sync` registers a source without projecting it. The commands `list`, `show`, `doctor`, `search`, `repair`, `backup`, `restore`, `pin`, `update`, `cleanup`, and `uninstall` require no prior initialization. The existence of `~/.ccync/config.json` serves as the predicate confirming that the machine is initialized.

## Synchronization and the first-run write gate

`ccync sync [--dry-run] [--yes]` resolves plugin sources, compiles the shared canonical render tree, and projects configurations to selected target agents. A separate projection pipeline schedules updates for the five fixed MCP hosts, regardless of which agents are selected. Before writing live configuration for the first time, ccync displays a detailed manifest of target agent files and MCP host configurations scheduled for modification. Run `sync --dry-run` to inspect proposed changes in advance. Passing `--yes` or setting `CCYNC_ASSUME_YES=1` bypasses the interactive confirmation prompt.

On a legacy storage layout, `sync --dry-run` reports the storage inventory and any pending older sublayout fixup without changing files. An applying `sync` performs recovery and commits the portable layout before projection. `refresh` never performs this portable migration. When it finds a legacy layout, it reports that `ccync sync` performs migration.

### Publication outcomes and retry

After each sync, every target has one status: `Applied`, `Unchanged`, `Skipped`, `Failed`, or `NotRun`. A target is published when every required route for it was written or already matched. When a required route was not written, the evidence line for that target reads `publication not published`, names the retry action, and sync exits `1`. The private combined fallback for that target stays in place.

```text
evidence [codex]: publication not published; loader NotRun; served-by-root not reported; retry: fix the reported write error, then rerun `ccync sync`
```

To recover, fix the reported cause, run `ccync sync --dry-run`, and then run `ccync sync`. Routes that already match stay unchanged. The table lists the route outcomes that sync reports.

| Route outcome | What sync prints | Effect on the run |
| --- | --- | --- |
| Written, or already matches | `published` | Exit `0` when no target fails. A combined copy can retire once its direct replacement is complete. |
| Write failed | `not published`, with the reported write error | Exit `1`. The private fallback stays. |
| Blocked by a conflict | `not published; conflict: <reason>; action: resolve the conflict, then rerun ccync sync` | Exit `1`. The private fallback stays. |
| Unsupported by the target | `not published because unsupported: <reason>`, with the action `no write is needed for this unsupported route` | Not a failure. No replacement is required. |

`loader NotRun` means ccync did not run a loader check for that route. It is not a load failure, and it is not evidence that an agent loaded the component. Only a real agent session can show that. Record `NotRun` until you have inspected that session.

### Plugin provider and management labels

Current status: ordinary `ccync sync` publishes provider components directly on each target's native roots and retires the combined `ccync` surface for every target whose direct publication succeeded. For Claude Code and Codex, ordinary sync also registers each plugin group as a native package through a verified adapter, when the installed CLI is on a verified line (Claude Code 2.1, Codex 0.169). Otherwise the group keeps its loose route, and sync reports the reason, for example `unverified adapter version <version>`. Copilot, AGY, and OpenCode keep their loose or component routes. Publication is file-level, so no label below proves that a running agent loaded a component. See [Provider identity, loading, and recovery](architecture.md#provider-identity-loading-and-recovery) for the current status.

Identity follows source form. Native plugin sources keep their native package identity. Standalone skills and already materialized components use source and component identity without a manager-added `ccync:` namespace. A separate source with a colliding native identity is reported as a conflict and is not silently merged or removed.

`ccync list` can show a provider diagnostic in this form (the values are illustrative): `Provider: demo | managed by ccync | loader: demo@<marketplace> | ownership: Managed | validation: LoaderNotRun`. The provider is the original plugin identity. `managed by ccync` identifies the manager separately. This example means the provider remains `demo`; ccync manages its target installation. The loader and enum values depend on the target and observed state. Do not interpret a label by itself as proof that a running agent loaded the component.

Selector names: the canonical selector is `<package>@ccync-<package>`, where `<package>` is the plugin's package name. A legacy selector is an earlier ccync-owned selector for the same target and provider source, under a different name. For example, an earlier sync recorded the registration `plugin@ccync-plugin` under package name `plugin`. The manifest was later renamed to `example`, so the canonical selector is now `example@ccync-example`, and the owned `plugin@ccync-plugin` is the legacy selector that migrates. When sync finds such a registration that ccync owns for the same target, provider source, and plugin, it installs the canonical selector first, verifies it, removes the old registration, and commits ownership, all under one journaled intent. The gate requires the same provider source key, the same upstream revision, complete ownership proof, a matching observed view digest, and a legacy selector that equals `<recorded package name>@ccync-<recorded package name>`. A disabled, unobservable, or unprovable legacy registration blocks the group and keeps both registrations. A foreign registration at the destination blocks the group the same way, and sync reports the reason. The gate does not compare the repository path, subpath, or manifest identity, because a committed record does not store them. ccync never removes a registration because its name matches, and it never edits the plugin source or foreign content.

When sync reports `Foreign native selector: demo@...`, ccync prints `Remedy (not executed): remove <selector> with its native manager, then run ccync sync`. Review the exact selector, remove that external registration using its own manager, and then run `ccync sync --dry-run` followed by `ccync sync --yes` (or confirm interactively). This is an operator-directed removal. Generic `ccync sync` does not remove the external registration. Never remove a selector based only on a matching name or component content.

If a target is reported disabled, do not expect sync to enable or replace its native registration. Decide whether to enable it using that product's native manager, then inspect the state again before syncing. For broken or unknown/unverifiable state, preserve the registration and existing files, inspect the target's native configuration and the reported selector or path, and use `ccync doctor` for the current diagnosis. Resolve the underlying issue and rerun `ccync sync --dry-run` before applying. If the conflict remains, keep it unresolved and retain the existing loader; do not infer ownership from names or equal contents.

Evidence is target-specific. `ccync list` reports recorded provider, manager, loader, ownership, and validation fields. Sync reports per-target `Applied`, `Unchanged`, `Skipped`, `Failed`, or `NotRun` state and identifies a foreign selector when recognized. These reports do not establish actual component loading in an agent session or Codex Settings presentation. Record live checks separately for each target. Use `NotRun` when a target session was not inspected. A successful disk projection on one target says nothing about another selected or unselected target.

If a managed update is interrupted or a target becomes partially synchronized, first preserve the reported journal/state and the last working loader. Correct the reported cause, rerun `ccync doctor`, then repeat `ccync sync --dry-run` and `ccync sync` to let the recorded operation converge. Do not manually delete generated packages or native registrations as a recovery shortcut. If the report still shows a conflict, disabled, broken, unknown, failed, or not-run target, stop and retain its evidence for diagnosis.

The provider lifecycle journal is written as schema v3. V1 and v2 journals are accepted only when their contents fit their schema. V1 cannot carry standalone publication intents, and v1 or v2 cannot carry standalone withdrawal intents. A pending intent is not downgraded or discarded. Keep the journal, preimages, ownership records, and last working route while resolving a conflict. Sync and doctor distinguish `standalone published`, `recovery blocked`, `identity unresolved`, and retained or fallback outcomes. These CLI results do not attest attached UI loading or account-bound settings. The owner reported on 2026-10-24 that live acceptance is complete. The evidence is not stored in this repository, and ccync did not verify it. CLI listings and filesystem checks do not establish attached UI loading.

This delivery work covers identity, publication, withdrawal, and recovery for already materialized content. Tool installation and upgrade, producing new tool snapshots, and connecting tool snapshots to native package routes remain in the separate P5 tool lifecycle. MCP writer behavior and hooks are also outside this delivery path.

If the operator declines the initial confirmation prompt, ccync aborts without modifying live agent configuration files. Preceding source downloads in the cache remain preserved. If a plugin cache directory is missing or fails to fetch, repair the source before projecting again. Certain sync or upgrade paths can misinterpret a missing source as an empty plugin, which risks incorrectly removing existing projected files from agent directories.

`config.json` records selected projection targets and machine settings. The canonical selection key is `selectedProjectionTargets`. Legacy keys `selectedRuntimes` and `runtimes` are recognized during configuration migrations but are never written back. MCP host updates operate independently of agent target selections: all five supported MCP hosts update even when their corresponding agent is not selected.

### Scope of this repair

This repair makes publication, retirement, and retry reports match what sync actually wrote. It does not change who owns an MCP entry that a source host already defines. That first-init ownership collision is a separate follow-up, and it remains unsolved. Existing Antigravity (AGY) face-local MCP lookup paths are unchanged.

## User-scope instructions

When `~/.ccync/AGENTS.md` exists, ccync synchronizes its content into `<!-- ccync:managed:begin -->` and `<!-- ccync:managed:end -->` blocks in the current-write files listed below. User content outside these markers remains untouched. The three Antigravity targets share `~/.gemini/AGENTS.md`, which ccync updates once per run. Damaged markers are reported and skipped. Claude has no current-write file, so this source does not reach Claude's user scope. There is no `~/.claude/AGENTS.md` destination. After ccync reads the source successfully, ordinary sync checks the legacy `~/.claude/CLAUDE.md` path for a managed block to prune only when `claude` is selected. It prunes the block whether the source is absent or nonempty.

`ccync sync --instructions-only [--dry-run] [--yes]` applies only this block and touches no plugin, MCP, or other projected surface. See [Instruction sync](commands.md#instruction-sync) for syntax, per-path states, and consent forms.

### Source ownership

You own `~/.ccync/AGENTS.md` and every byte of a target file outside the ccync markers. ccync owns only the text between the markers. ccync never generates, rewrites, lints, or checks the meaning of your instructions.

- A missing or zero-byte source means you want no instructions. ccync then removes only the ccync-owned block from each selected file.
- A source that holds only whitespace is content, and ccync writes it.
- A source that cannot be read, is not valid UTF-8, or is a dangling symbolic link never removes anything.

### Selection behavior

ccync resolves selected targets in canonical order and removes duplicate physical paths. Six logical targets map to four current-write files. The three Antigravity targets share `~/.gemini/AGENTS.md`. Claude has no current-write path.

| Target | Instruction file |
| --- | --- |
| `claude` | No current-write path |
| `codex` | `~/.codex/AGENTS.md` |
| `copilot` | `~/.copilot/AGENTS.md` |
| `opencode` | `~/.config/opencode/AGENTS.md` |
| `agy-cli`, `agy-ide`, `agy-gui` | `~/.gemini/AGENTS.md` |

The first-projection disclosure and full `sync --dry-run` show separate groups for current instruction files selected for this run and legacy files that would be pruned when Claude is selected. `init` uses the same disclosure. The instructions-only dry-run lists each current path and, when applicable, the legacy prune path. It writes no files. An apply requires `--yes`, `CCYNC_ASSUME_YES=1`, or an interactive confirmation. The scoped route requires authorization for every apply before the first full projection.

The legacy Claude path is separate from current writes:

| Lifecycle | Path | When ccync checks it |
| --- | --- | --- |
| Legacy prune | `~/.claude/CLAUDE.md` | During ordinary sync only when `claude` is selected. |
| Doctor classification | `~/.claude/CLAUDE.md` | Regardless of target selection or config errors. |
| Uninstall | All current-write paths and the legacy Claude path | Regardless of target selection, with ownership proof required for removal. |

Ordinary sync never touches a current-write file for a deselected target. It also skips the legacy Claude path unless `claude` is selected. `ccync doctor` classifies the legacy path regardless of selection or config errors. `ccync uninstall` checks all instruction paths regardless of selection and removes only blocks backed by ccync ownership proof.

Each physical file is an independent write unit. A failure on one file does not stop the others and does not roll back their writes.

### Recovery

The scoped route fails closed before it reads or writes any target file when the machine is not initialized or the config is unusable. Fix the reported cause, then run the same command again. Files that already match stay unchanged.

| Layer | Failure | Recovery |
| --- | --- | --- |
| Config | `~/.ccync/config.json` is missing, unreadable, or not valid JSON. | Run `ccync init <source>`, or repair the file. |
| Config | The config sets both `selectedRuntimes` and `selectedProjectionTargets`. | Run `ccync init <source>` to choose one selection. |
| Config | The config selects no valid target, or lists a rejected target value. A mixed list also fails. | Run `ccync init <source>`, or remove the rejected values that the message lists. |
| Source | `~/.ccync/AGENTS.md` cannot be read or is not valid UTF-8. | Fix the file permissions, or save the file as UTF-8. |
| Source | `~/.ccync/AGENTS.md` is a dangling symbolic link. | Repair or remove the link. |
| Target | The target file cannot be read. | Fix the permissions of the target file. |
| Target | The ccync markers in the target file are damaged. | Repair the markers by hand, then run `ccync doctor`. ccync never repairs them. |
| Target | The target file is a dangling symbolic link. | Preserve the link. Inspect its target and access, restore access, then rerun `ccync doctor`. |

A source failure never prunes a block. During a full `ccync sync`, a source failure skips all instruction writes, keeps the other completed phases, and exits `1`.

The ordinary lifecycle uses the selected targets for current writes and the selected-Claude legacy prune. If full sync cannot build machine options, it skips the instruction phase. It never adds Claude as a current-write destination.

### Reload advice and evidence boundary

After an apply that changed at least one file, reload or restart your agents. ccync cannot observe whether an agent has loaded the change, so it reports no load state. A skill name in your instruction source is not evidence that every host provides that skill.

For Codex, Copilot, OpenCode, and the three AGY faces (`agy-cli`, `agy-ide`, `agy-gui`), ccync writes the files to their locations on disk. It cannot confirm that the host loaded them, so reload that agent after the first projection. The preview of `ccync init` states the same reload need before you confirm.

Claude Code's built-in `AGENTS.md` plugin loads instruction files independently of ccync. Claude Code supports it from version 2.1.277, though some sessions may lack support before 2.1.281. The plugin must be enabled. Use `/plugin` to manage its enabled state, and set Project instructions to a suitable file-selection mode. In the default `claude-md-or-agents-md` mode, repository `AGENTS.md` loading is suppressed when the working directory or an ancestor contains `CLAUDE.md`, `.claude/CLAUDE.md`, or `CLAUDE.local.md`. Users can choose `claude-md-and-agents-md` to load both. ccync changes no settings and does not guarantee loading. See the [Claude Code memory documentation](https://code.claude.com/docs/en/memory).

### Native package acceptance status

A native Claude Code or Codex package reaches a running app only after you fully quit the app, reopen it, and start a new session. ccync does not reload agents. A registration that is disabled in the client keeps zero loads, and sync does not enable it; use the client's own manager to re-enable it.

A successful CLI registration does not prove that a desktop app loaded the package. Isolated-profile probes on 2026-10-22 reported identity 1, enabled count 1, and disabled count 0 on Codex CLI 0.163.0, Claude Code 2.1, GitHub Copilot CLI 1.0.101 (CLI only, not VS Code), and OpenCode 1.18.35. These probes do not show that a desktop, IDE, or App loaded the component. The AGY CLI 1.4.4 probe is NotRun, because `agy -p /skills` starts a model turn and the listing cannot be read. AGY IDE and GUI are not verified.

The owner reported on 2026-10-24 that live acceptance on real clients is complete for the Codex/ChatGPT and Claude apps, for OpenCode, Copilot CLI and VS Code, and AGY IDE and GUI, and for the unselected shared-root readers. The acceptance evidence is not stored in this repository, and ccync did not verify it. CLI listings and filesystem checks do not establish attached UI loading.

The evidence for six current-write targets and four files has four sources, and each proves a different thing. Claude's legacy prune path is checked separately when selected.

| Evidence | What it proves | What it does not prove |
| --- | --- | --- |
| `sync --instructions-only --dry-run` | Predicts the state of each selected current-write path and the selected-Claude legacy prune path. | That any write happened. |
| `sync --instructions-only` | Reports whether each selected file changed, matched, or was blocked. | The full block bytes or whether any agent loaded the file. |
| `ccync doctor` | Reports read-only health for selected current-write files and independently classifies the legacy Claude path. | That any agent loaded the file. |
| Real agent session | Only a session probe observes whether an agent loaded the instructions. | Nothing about the other agents. |

Disk results are mandatory acceptance evidence. A loading observation counts only when it comes from a real session of that agent. Record `NotRun` when no such session ran. Never infer loading from doctor output or from a successful apply.

## Diagnostics and uninstall

`ccync doctor [--dry-run] [--release-gate]` evaluates configuration validity, machine state, generated render outputs, target agent files, and release readiness conditions. Diagnostic findings explicitly name the affected components. Currently, `--dry-run` and `--release-gate` do not introduce additional checks; neither flag should be treated as an independent release audit.

For instruction health, doctor classifies the legacy Claude path even when the target is not selected or config and source checks fail. An absent or empty file, or a file without a ccync block, needs no action. A remaining managed block is a warning. When Claude is selected, remove that block with `ccync sync --instructions-only --yes`. Otherwise, manually remove only the content between the two verified ccync markers, including the markers, or run `ccync uninstall`. Preserve all bytes outside the managed block. An unreadable file, malformed markers, or a dangling link is an error. Doctor never repairs malformed markers. For a dangling link, preserve it, inspect its target and access, restore access, then rerun `ccync doctor`.

Native package diagnostics in `ccync doctor` are read-only. Doctor runs the client's listing command for each committed native record. After a failed or timed-out listing, it does not repeat that executable's listing for its remaining records. It never changes a registration. A listing entry does not prove that a running client loaded the package. A fallback route shows its reason, and its loose route stays active.

A pinned personal plugin whose cache exists but produces no projectable components triggers a `render-contribution` diagnostic warning. Unpinned items or unreadable catalog records represent distinct failure states and do not pass this check. Doctor errors indicate that ccync cannot operate correctly; warnings indicate that ccync functions but an agent workspace may be incomplete or inconsistent. Doctor emits warnings even when exiting with code 0.

`ccync uninstall` removes only MCP entries, skills, custom commands, custom agents, and managed instruction blocks backed by ccync ownership proof. It checks current-write instruction files and the legacy Claude path regardless of target selection. It removes tool records and derived tool state, including `tools.json`, tool snapshots, the operation journal, and the install receipt. It also deletes derived `cache/` and `build/` data while preserving `config.json`, `plugins.json`, `mcp.json` when present, and native agent configuration directories. The command reports the actual preserved machine INPUT files. The persistent coordination file `build/lifecycle.lock` remains so concurrent commands continue to use the same lock. The command does not delete the `ccync` binary; remove the binary using the original package manager or installer.

## Exit codes

Exit code `0` indicates success, `1` indicates an operational failure, `2` indicates a recognized command that is not yet implemented, `3` indicates that a read-only query discovered available updates, and `64` indicates invalid command-line usage. `ccync update --check` operates in read-only mode and returns `0` upon query completion even when the installation channel does not support automated self-update. Attempting an unsupported binary `update` returns exit code `1`. See [ADR 05](adr/05-machine-lifecycle-adoption-and-cli-contract.md) for complete specifications.

## Machine state

The `~/.ccync/` directory holds machine inputs, cached sources and generated state. The personal MCP input is `~/.agents/mcp.json`, separate from those files. See [MCP](mcp.md) for legacy input and routing. `config.json`, `plugins.json`, optional `AGENTS.md` and `tools.json` are machine inputs. Generated state includes the lockfile, render, MCP ownership and journals. Committed tool snapshots regenerate only with `ccync tool refresh <id>`.

The native configuration files read by coding agents reside outside `~/.ccync/`. ccync manages only specific entries and demarcated marker blocks backed by ownership proof, never assuming ownership of entire host configuration files.

## Backup and restore

`ccync backup [--output <dir>] [--strip-secrets]` creates a portable profile. Direct MCP input with raw credentials, URL userinfo, placeholder collisions or values that cannot be classified safely is refused before output creation. Machine secrets and host ownership are not exported. `--strip-secrets` retains its compatibility behavior for supported redactions.

Run `ccync restore --from <dir> --dry-run` first, then `ccync restore --from <dir>`. New personal lists retain their bytes. An old backup's `mcpServers` root is converted to `ccyncMcpServers`. Conflicting active inputs or explicit routes stop restore. Restore does not write native host configuration or transfer ownership. Supply missing machine-local values, then preview with `ccync sync --dry-run`. If interrupted, rerun with the same backup and resolve any third fingerprint. See [MCP](mcp.md).

## MCP recovery boundary

`ccync init <source>` previews imports before consent and may then apply them. Use `ccync sync --dry-run` for later route previews and `ccync doctor --mcp-evidence <path>` for a sanitized event file. For a held item, preview `ccync sync --reselect-mcp SOURCE SERVER HOST --dry-run` before reselecting it without `--dry-run`. P7 live recovery requires separate authorization for each run. Synthetic evidence does not establish the cause of the historical incident.
