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

Run `ccync init` first to choose a primary master agent and adopt its existing components. During initialization, ccync prompts for an initial projection and displays the agent configuration files and MCP hosts scheduled for modification. After initialization, run `ccync sync --dry-run` to preview pending changes, then run `ccync sync --yes` to apply them. The `ccync add` command accepts catalog IDs, Git URLs, local directories, or compressed archives. Pass `--no-sync` to register a source without immediately projecting it. See [Commands](docs/commands.md) for full argument details.

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

## Documentation map

| Document | Contents |
| --- | --- |
| [Security](SECURITY.md) | Reporting, trust boundaries, and security invariants. |
| [Architecture](docs/architecture.md) | System boundaries, six crates, projection flow, deployment, and quality goals. |
| [Setup and diagnostics](docs/setup.md) | Initialization, synchronization, machine state, backup, restore, diagnostics, and uninstall. |
| [Plugin discovery and management](docs/plugins.md) | Search, source repair, intake, removal, inspection, and team rosters for ccync plugins. |
| [Commands](docs/commands.md) | Uses of the 19 public commands and the limits of update, upgrade, pin, and cleanup. |
| [MCP](docs/mcp.md) | Author definitions, placeholder resolution, five host writers, and per-host ownership recovery. |
| [Adoption and reconciliation](docs/adoption.md) | Master-agent adoption, source provenance, ownership boundaries, and Graphify skill updates. |
| [CONTRIBUTING](CONTRIBUTING.md) | Workspace verification, safe changes, documentation sync, and commits. |
| [Naming](docs/naming.md) | Authoritative definitions for agents, projection targets, MCP hosts, plugin lifecycle, and target keys. |
| [Deferred work](docs/deferred-work.md) | Confirmed gaps, limitations, and conditions for reopening engineering decisions. |
| [Translation guide](docs/i18n/guide.md) | Translation paths, metadata, pairing, and review rules for the bilingual corpus. |
