# ccync manual

Every `ccync` command. ccync is a cross-agent plugin / MCP / skills manager; all state lives under
`~/.ccync/` and never touches `~/.gal/`.

Run `ccync --help` / `ccync --version` for the live surface. The public surface is **9 commands**:
`init`, `sync`, `add`, `remove`, `list`, `doctor`, `backup`, `restore`, `uninstall`.

## Lifecycle

### `ccync init [<master>]`

First-run setup. Picks a **master agent** (`claude` or `codex`) — interactively when `<master>` is
omitted, or directly from the argument — adopts that agent's installed plugins / MCP into ccync's
truth source (`~/.ccync/state/plugins.lock.json`), records the runtime selection (default: all
supported runtimes), and runs the unified projection engine. Unknown master → usage error.

- `--yes`: skip the first-run overwrite-confirmation prompt (see `ccync sync` for details).

On a fresh machine the projection engine shows the live agent config files it will write and asks
for confirmation before writing them.

### `ccync sync [--dry-run] [--yes]`

The one projection engine. Resolves the catalog (+ machine config + personal catalog) into the
lockfile, renders the canonical root under `~/.ccync/plugins/ccync/`, and projects all managed
surfaces — skills, commands, agents, and MCP host config — to every selected agent (best-effort
per surface). Run it after changing config, profiles, or installed plugins; it is idempotent.

- `--dry-run`: print the live agent config files that would be written without writing them.
- `--yes`: skip the first-run overwrite-confirmation prompt (also: set `CCYNC_ASSUME_YES=1`).

**First-run gate:** on a fresh machine (no prior successful projection), `ccync sync` (and `ccync
init`) lists the live agent config files it is about to write — `~/.claude.json`,
`~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and the opencode config — and asks for
confirmation before writing. On a non-interactive terminal, pass `--yes` or set
`CCYNC_ASSUME_YES=1` to proceed without a prompt.

Master adoption (`--import-from`) has been moved to `ccync init`.

### `ccync uninstall`

Remove ccync's managed state under `~/.ccync/`. Does not touch other agents' own configs.

## Plugins

### `ccync add <source> [--no-sync]`

Add a personal plugin from **any source**, register it in `~/.ccync/local/catalog.json`, pin it
into the lockfile, and auto-`sync` to every selected agent. Source auto-detection:

| Source kind | Example | Cache key |
| --- | --- | --- |
| Git URL | `https://github.com/owner/plugin.git` | `<id>@<commit-sha>` |
| Local path | `/path/to/plugin` or `./my-plugin` | `<id>@<commit-sha>` |
| Archive (`.zip` / `.tar.gz`) | `plugin-v1.tar.gz` | `<id>@<sha256-first12>` |
| Catalog ID | `my-catalog-plugin` (bare identifier) | resolved via embedded catalog |

The plugin id is the source's last path segment (`.git` stripped; catalog ID used as-is).

Archives are extracted to `~/.ccync/local/cache/<id>@<sha256-first12>/` preserving the full
CC-plugin layout. A GitHub-style single-root wrapper directory is automatically promoted. Re-running
`add` with the same source (same archive bytes → same SHA-256 prefix) returns immediately
(`AlreadyPresent` — idempotent).

**Hooks**: a plugin shipping `hooks/hooks.json` has its hooks projected and loaded by Claude; for
non-Claude agents hooks are N/A. See [`architecture.md`](architecture.md#hooks-projection) for the
mechanism.

- `--no-sync`: register without projecting. Run `ccync sync` later to project.
- `--yes`: skip the first-run overwrite-confirmation prompt.

### `ccync remove <id>`

Remove a personal plugin from the catalog, prune **all** its projected artifacts (skills, commands,
agents, hooks, MCP entries), and auto-`sync`.

### `ccync list`

List installed personal plugins with their pinned commit + source.

## Diagnostics

### `ccync doctor`

Read-only management health check: canonical root, master-adopted state, MCP projection, Claude
plugin cache freshness, and shared skills projection. Always read-only.

## Backup / restore

### `ccync backup [--output <dir>]`

Export the machine-local state files to a backup directory.

### `ccync restore --from <dir>`

Restore machine-local state from a backup directory.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | success |
| 1 | operation error |
| 64 | usage error (unknown/missing command or bad flag) |
