# ccync

**English** · [繁體中文](docs/i18n/zh-Hant/README.zh-Hant.md)

ccync manages plugins, skills, custom commands, custom agents, and MCP servers in one place. It projects these components into native configurations for Claude Code, OpenAI Codex, GitHub Copilot CLI, OpenCode, and Google Antigravity. ccync records selected sources in `~/.ccync/build/lock.json`, compiles a shared canonical render tree, and writes configurations in each tool's native format. If a target path lacks explicit ownership proof, ccync preserves the existing content rather than deleting unverified data.

## Quick start

```text
ccync init claude
ccync sync --dry-run
ccync sync --yes
ccync add <source>
ccync doctor
```

Run `ccync init` first. Choose one import source (`claude`, `codex`, `copilot`, `opencode`, or `agy`) and the target agents; a fresh initialization selects all five. ccync scans the source without writing to it, shows a preview of the imports, the agent configuration files, and the MCP hosts scheduled for modification, and writes nothing until you consent. The source records where each imported component came from; it is not a permanent master agent. After initialization, run `ccync sync --dry-run` to preview pending changes, then run `ccync sync --yes` to apply them. The `ccync add` command accepts catalog IDs, Git URLs, local directories, or compressed archives. Pass `--no-sync` to register a source without immediately projecting it. See [Commands](docs/commands.md) for full argument details.

## Public commands

ccync provides 19 public CLI commands. [Commands](docs/commands.md) outlines their purposes and contracts. Run `ccync <command> --help` to display exact options and flags.

## Platform notes

- Agent configuration paths differ on Windows, macOS, and Linux. ccync writes each target product's native format at its platform-specific path.
- Windows releases use static CRT linkage. After installation, run `ccync doctor` to verify the local environment.
- Antigravity appears as a single agent in the selection interface. During projection, ccync expands Antigravity into three distinct targets: `agy-cli`, `agy-ide`, and `agy-gui`.
- MCP configuration updates all five supported hosts, regardless of which agents are selected for projection.

## Troubleshooting

When an operation fails, run `ccync doctor` to identify the issue.

- Run `ccync sync --dry-run` to inspect which files a sync operation would change.
- Run `ccync repair` if an adopted item lacks a recorded source.
- Run `ccync cleanup --dry-run` to inspect candidate cache directories after updating or removing sources.

If ccync cannot verify ownership of a file or directory with the same name, it preserves the existing file and outputs a warning rather than deleting unknown data.

Current status: ordinary `ccync sync` publishes provider components through the direct target surfaces. The combined `ccync` surface is retired in the same sync once its direct publication succeeds. A target whose replacement fails keeps the combined writer as a fallback. A sync that deselects a target removes that target's recorded direct copies. The labels and foreign-selector remedy below apply to these direct surfaces. See [Provider identity, loading, and recovery](docs/architecture.md#provider-identity-loading-and-recovery) for the current status.

`ccync list` distinguishes the original provider from its manager, for example `Provider: gal | managed by ccync | loader: gal@ccync-marketplace | ownership: Managed | validation: LoaderNotRun`. If sync reports a `Foreign native selector`, remove that exact selector with its native manager, then run `ccync sync --dry-run` and `ccync sync --yes`. Sync does not remove external registrations or enable disabled plugins. Keep disabled, broken, or unknown states for diagnosis. Per-target sync and disk evidence does not prove that a running agent loaded the component. See [Setup and diagnostics](docs/setup.md#plugin-provider-and-management-labels) for recovery guidance and evidence limits.

## Documentation map

| Document | Contents |
| --- | --- |
| [Security](SECURITY.md) | Reporting, trust boundaries, and security invariants. |
| [Architecture](docs/architecture.md) | System boundaries, six crates, projection flow, deployment, and quality goals. |
| [Setup and diagnostics](docs/setup.md) | Initialization, synchronization, machine state, backup, restore, diagnostics, and uninstall. |
| [Plugin discovery and management](docs/plugins.md) | Search, source repair, intake, removal, inspection, and team rosters for ccync plugins. |
| [Commands](docs/commands.md) | Uses of the 19 public commands and the limits of update, upgrade, pin, and cleanup. |
| [MCP](docs/mcp.md) | Choose routes, preview changes, and diagnose MCP attempts. |
| [Adoption and reconciliation](docs/adoption.md) | Import from one of five sources, source provenance, ownership boundaries, and Graphify skill updates. |
| [CONTRIBUTING](CONTRIBUTING.md) | Workspace verification, safe changes, documentation sync, and commits. |
| [Naming](docs/naming.md) | Authoritative definitions for agents, projection targets, MCP hosts, plugin lifecycle, and target keys. |
| [Deferred work](docs/deferred-work.md) | Confirmed gaps, limitations, and conditions for reopening engineering decisions. |
| [Translation guide](docs/i18n/guide.md) | Translation paths, metadata, pairing, and review rules for the bilingual corpus. |
