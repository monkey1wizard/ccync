English | [繁體中文](i18n/zh-Hant/manual.zh-Hant.md)

# ccync Manual

This document details every `ccync` command. ccync is a cross-agent plugin, MCP, and skills manager. Its own state — everything it reads and writes for itself — lives under `~/.ccync/`. It also *projects* that state onto each agent's live configuration surface outside `~/.ccync/` (`~/.claude.json`, `~/.claude/skills/...`, `~/.codex/config.toml`, and similar per-agent paths) — see [Machine Layout](#machine-layout) below for the full picture.

Run `ccync --help` or `ccync --version` in your terminal to see the live command surface. The public CLI consists of exactly **14 commands**:
`init`, `sync`, `add`, `remove`, `list`, `show`, `doctor`, `backup`, `restore`, `uninstall`, `update`, `upgrade`, `cleanup`, and `pin`.

## Lifecycle Commands

### `ccync init [<master>]`

Performs the initial, first-run setup. This command selects a **master agent** (either `claude` or `codex`). If the `<master>` argument is omitted on an interactive terminal, an arrow-key selection prompt appears; on a non-interactive terminal, you must supply the master explicitly. It adopts the master agent's installed plugins into ccync's central source of truth (`~/.ccync/build/lock.json`), and separately snapshots the master's **full MCP server definitions** (command, args, url, env, headers — not just names) into the lockfile's `_mcpServers` namespace, so ccync can reproject every MCP server to your other selected agents without re-reading the master's config each time. It then presents an interactive multi-select where you choose which agents to project to (defaults: `claude`, `codex`, `copilot`; `opencode`, `agy`, and `gemini-cli` are unchecked by default). After recording your runtime selection it executes the unified projection engine without a separate first-run confirmation — the runtime multi-select serves as the intent signal (`ccync init` skips the First-Run Gate below; see the gate's own division-of-labor note). Providing an unknown master agent results in a usage error.

### `ccync sync [--dry-run] [--yes]`

This is the core projection engine. It resolves the catalog (merging the built-in catalog, machine configuration, and your personal catalog) into the lockfile. It then renders the canonical root under `~/.ccync/build/render/` and projects all managed surfaces—skills, commands, agents, and MCP host configurations—to every selected agent on a best-effort basis per surface. You should run this command after modifying your configuration, profiles, or installed plugins. The operation is completely idempotent.

- `--dry-run`: Prints the live agent configuration files that would be modified, without actually writing to them.
- `--yes`: Skips the first-run overwrite-confirmation prompt. You can alternatively set the environment variable `CCYNC_ASSUME_YES=1`.

**First-Run Gate:** On a fresh machine (where no successful projection has occurred before), running `ccync sync` will list the live agent configuration files it is about to modify—such as `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and the OpenCode config. It then pauses to ask for confirmation before making any writes. In a non-interactive terminal environment, you must pass `--yes` or set `CCYNC_ASSUME_YES=1` to proceed without this prompt.

**Division of labor across the four lifecycle commands:** `sync`, `add`, and `remove` all share this same gate as-is — `remove` needs `--yes` (or `CCYNC_ASSUME_YES=1`) to bypass it non-interactively, exactly like `sync`/`add`. `init` skips the gate entirely (passing `skip_first_run_gate=true`) because it already showed its own agent-selection UI before projecting. `upgrade` runs the gate but always auto-confirms (`assume_yes=true`), so it never prompts. `add`'s live-surface render/projection happens strictly *after* the gate passes — a declined gate leaves the catalog/cache/lockfile writes in place but produces zero live-surface writes.

*(Note: Master adoption via `--import-from` has been relocated to the `ccync init` command.)*

### `ccync uninstall`

Removes ccync-managed state and reverses ccync's projections, leaving your input files and shared user directories intact.

**Removes:**

- All derived state under `~/.ccync/` — `cache/`, `build/`, and any legacy `ledger.json` — leaving exactly the two input files (see *Preserves*). No uninstall ledger is written.
- ccync-owned MCP server entries from live agent configs (Claude `~/.claude.json`, Copilot, OpenCode, Codex `~/.codex/config.toml`), matched by the previously projected managed names — user-owned MCP servers are kept.
- ccync's marketplace / plugin-state records: the `ccync` marketplace and `*@ccync` plugin records in Claude's `installed_plugins.json` / `known_marketplaces.json`, and (best-effort, when `codex` is on PATH) the `ccync@ccync-marketplace` Codex registration.
- ccync-owned skill / command / agent projections and AGY CLI/IDE links, authorized strictly by the projection lockfile (user-owned siblings on shared surfaces are never removed).

**Preserves:**

- Your input files: `~/.ccync/config.json` and `~/.ccync/plugins.json`.
- The shared `~/.local/bin/` directory and your `PATH` (never edited).
- The Antigravity GUI user-customizable config, and all non-ccync agent configuration.

The CLI does not delete its own binary — a running process cannot reliably remove itself. Use the packaging script (`packaging/uninstall.sh` on POSIX, `packaging/uninstall.ps1` on Windows) to run `ccync uninstall` and then remove `~/.local/bin/ccync(.exe)`. There is no `--purge` flag; your input files are always kept.

If a surface cannot be cleaned (a config file fails to parse, or `codex` is absent for Codex de-registration), the run reports a named warning instead of silently claiming success, and the file is preserved.

### `ccync update [--check]`

Self-updates the `ccync` binary, mirroring `brew update`. It compares the running version against the latest GitHub release and, when newer, downloads the platform artifact, verifies it against `checksums.txt` (SHA-256, the same contract as the install scripts), and atomically swaps the binary in place. The previous binary is moved aside to `<ccync>.old` (best-effort removed; on Windows it may be left for cleanup on the next run).

`ccync update` is **provenance-aware** and only self-replaces a curl/irm install (`~/.local/bin/ccync`, `%USERPROFILE%\.local\bin\ccync.exe`). When the binary is managed by a package manager it **refuses** and points you at the native command instead — `brew upgrade ccync` (Homebrew), `winget upgrade Monkey1Wizard.ccync` (winget), or `cargo install --git … --force` (cargo). An unrecognized install location is fail-closed (refused with a manual-update hint).

- `--check`: Reports the installed version, the latest available version, and the detected provenance **without** downloading or writing anything.

### `ccync upgrade [<plugin>] [--dry-run]`

Upgrades installed **git-source** plugins to their latest upstream commit, mirroring `brew upgrade`. For each git plugin it runs `git ls-remote` to compare the remote `HEAD` against the pinned commit; when they differ it re-clones into a fresh `~/.ccync/cache/<id>@<new-sha>`, verifies the cache landed, then atomically re-pins it and runs a `sync` to reproject. Archive- and local-source plugins have no upstream "latest" and are reported as `skipped`. A single plugin failing (network/clone) does not abort the others.

- No argument upgrades **all** git plugins; `<plugin>` upgrades a single plugin id (an unknown id is an actionable error).
- `--dry-run`: Prints the upgrade diff (which plugins, `<old>` → `<new>`, and skips) **without** cloning, re-pinning, or projecting. Returns exit `3` when at least one plugin is upgradable, `0` when none, `1` on error.
- Plugins you have **held** (`ccync pin`) are reported as `held` and skipped — never queried, never re-cloned.

### `ccync cleanup [--dry-run]`

Prunes orphaned plugin caches under `~/.ccync/cache/`, mirroring `brew cleanup`. After an `upgrade` re-clones a plugin into `<id>@<new-sha>`, the previous `<id>@<old-sha>` directory is left behind; `cleanup` removes those stale dirs. It deletes by default; `--dry-run` lists what it would remove without touching anything.

Safety is **fail-closed** — the lockfile is the sole authority for what is live:

- If the lockfile cannot be read/parsed, the live set is undetermined and **nothing is pruned**.
- A cache dir whose id is no longer in the lockfile (a removed plugin) is pruned.
- A stale-sha dir of a plugin that *does* have a resolved live pin is pruned.
- A plugin that is managed but has **no resolved pin** while owning multiple cache dirs is **ambiguous** — those dirs are **retained with a warning**, never guessed at.

## Plugin Management

### `ccync add <source> [--no-sync]`

Adds a personal plugin from **any supported source**, registers it within `~/.ccync/plugins.json`, pins it into the lockfile, and automatically runs a `sync` to project it to every selected agent. Source detection works automatically:

| Source Type | Example | Cache Key |
| --- | --- | --- |
| Git URL | `https://github.com/owner/plugin.git` | `<id>@<commit-sha>` |
| Local Path | `/path/to/plugin` or `./my-plugin` | `<id>@<commit-sha>` |
| Archive (`.zip` / `.tar.gz`) | `plugin-v1.tar.gz` | `<id>@<sha256-first12>` |
| Catalog ID | `my-catalog-plugin` (a bare identifier) | Resolved via the embedded catalog |

The plugin ID is derived from the source's final path segment (with `.git` stripped off). Catalog IDs are used exactly as provided.

**How the source type is decided (precedence).** Detection is automatic and ordered — the first rule that matches wins:

1. Ends with `.zip`, `.tar.gz`, or `.tgz` → **Archive**.
2. Starts with `http://`, `https://`, `git@`, or `git://` → **Git** (any git host — GitHub, GitLab, Bitbucket, self-hosted, `git@` SSH; **not** GitHub-only).
3. Contains a `/` or `\`, or starts with `.` or `~`, or is an absolute path → **Local Path** (git working copy on disk).
4. Otherwise (a bare identifier with no separators) → **Catalog ID**.

> **Gotcha:** a bare word with no slash or dot — e.g. `my-plugin` — is treated as a **Catalog ID**, *not* a relative path. To add a local folder named `my-plugin`, write `./my-plugin` so it is detected as a path.

Archives are extracted to `~/.ccync/cache/<id>@<sha256-first12>/`, strictly preserving the full CC-plugin directory layout. If the archive contains a single, GitHub-style root wrapper directory, it is automatically promoted. Re-running `add` with the same archive bytes (same SHA-256 prefix) hits an already-populated cache directory and returns immediately without re-extracting.

**Re-adding an already-added plugin ID is an error, not a no-op.** If the derived plugin ID is already present in `~/.ccync/plugins.json`, `add` refuses with a usage error (exit `64`) and a hint to `ccync remove <id>` first: it does not silently succeed. This is distinct from the cache-level dedup above — that dedup governs re-fetching the *same source content*, not re-registering the *same plugin ID*.

**Hooks:** If a plugin includes a `hooks/hooks.json` file, its hooks are projected and subsequently loaded by Claude. For non-Claude agents, hooks are simply inapplicable. See the [`architecture.md`](architecture.md#hooks-projection) documentation for deeper mechanics.

- `--no-sync`: Registers the plugin without immediately projecting it. You must run `ccync sync` later to complete the projection.
- `--yes`: Skips the first-run overwrite-confirmation prompt.

### `ccync remove <id>`

Removes a personal plugin from the catalog, completely prunes **all** of its projected artifacts (including skills, commands, agents, hooks, and MCP entries), and automatically performs a `sync`.

### `ccync list [--upgrade-available]`

Lists all installed personal plugins, displaying their pinned commits and source locations.

- `--upgrade-available`: instead of the plain list, runs a **read-only** outdated check — for each git plugin it compares the remote `HEAD` against the pinned commit and prints `id  current → latest  source`. This is the same engine as `ccync upgrade --dry-run`, just surfaced as a query. Exit code is script-checkable: `0` when everything is current, `3` (`UpdatesAvailable`) when at least one plugin can be upgraded, and `1` on a real error (network/parse). Held plugins (see `ccync pin`) are reported as `held` and never queried.

### `ccync show <id>`

Prints the full detail of one installed personal plugin: display name, source, install strategy, pinned commit sha, whether it is **held**, and a best-effort count of the components (skills / commands / agents / hooks) it projects. An unknown id is an actionable error. Mirrors `brew info` / `winget show`.

### `ccync pin <id>` · `ccync pin --remove <id>` · `ccync pin --list`

Holds a plugin against `upgrade` (winget-style pin). `pin <id>` freezes the plugin at its current sha, `pin --remove <id>` releases it, and `pin --list` shows which plugins are currently held. A held plugin is skipped by `ccync upgrade` and `ccync list --upgrade-available` (reported as `held`, never queried).

> **`pin` the verb vs the `pin` field — different things.** The `pin` / `pinnedSha` fields in `plugins.json` / the lockfile record the *locked commit sha* (which version is installed). The `ccync pin` **verb** records a *hold against upgrade*, stored in a separate `held` field. Holding a plugin does not change its sha; it only stops `upgrade` from moving it.

## Diagnostics

### `ccync doctor`

Executes a read-only management health check. It validates the canonical root, master-adopted state, MCP projection integrity, Claude plugin cache freshness, and shared skills projection. This command never modifies state.

## Configuration (`config.json`)

ccync's machine configuration lives at **`~/.ccync/config.json`**. It is plain JSON that you edit by hand; ccync owns no `config set` command. Changes take effect on the next `ccync sync` (or any command that resolves state). A missing file or missing key falls back to explicit defaults — ccync never panics on an absent or malformed config.

**How to edit safely:**

1. Open `~/.ccync/config.json` in any text editor (create it if absent).
2. Keep it valid JSON — a parse error makes ccync silently fall back to defaults, so validate before saving.
3. Run `ccync sync` to apply. Run `ccync doctor` to confirm the config is read as expected.

**Example:**

```json
{
  "devMode": false,
  "ccyncRoot": "/path/to/ccync/checkout",
  "secrets": {
    "GITHUB_TOKEN": "ghp_xxx"
  }
}
```

**Keys:**

| Key | Type | Meaning |
| --- | --- | --- |
| `devMode` | bool | When `true`, ccync renders from the working tree at `ccyncRoot` instead of packaged source. Omit or `false` for normal mode. |
| `ccyncRoot` | string | Path to a ccync repo checkout. Required when `devMode` is `true`; if `devMode` is `true` but this is unusable, ccync errors rather than silently falling back. |
| `secrets` | object | Named credentials (e.g. API keys, tokens). Referenced from MCP server definitions via `${VAR}` placeholders, so secret values never appear inline in projected agent configs. Secrets are never path-substituted and never written to live agent files in clear text by the placeholder model. |

**MCP `${VAR}` substitution:** any `${NAME}` placeholder in a managed MCP server entry is resolved at projection time from the process environment, then overlaid with `config.json#secrets` — on a name collision, `config.json#secrets` wins. This lets one config drive every agent's MCP host without duplicating credentials per agent.

> Note: `devMode`/`ccyncRoot` are the authoritative mode fields. The legacy `installMode` field is deprecated — if present it only feeds a `ccync doctor` migration warning and does not control mode.

## Machine Layout

Everything ccync owns on your machine lives under `~/.ccync/`, split into three classes:

- **INPUT** — the two files you (or `ccync add`/`ccync init`) write directly; never auto-deleted, never rebuilt by `sync`.
- **CACHE** — fetched plugin bodies, keyed by pin so a re-fetch of the same content is a no-op; rebuildable from network/source.
- **OUTPUT** — everything under `build/`; fully regenerated by `ccync sync` (or `ccync refresh`) and safe to delete.

The public plugin catalog is **embedded in the `ccync` binary itself** — there is no catalog file on disk under `~/.ccync/`. Resolution reads the embedded copy directly; nothing is downloaded or deployed to disk for it.

```txt
~/.ccync/                              all ccync machine state
├── config.json          [INPUT]       machine config (devMode/ccyncRoot, secrets)
├── plugins.json         [INPUT]       personal plugin catalog (written by `ccync add`)
├── cache/               [CACHE]       personal plugin bodies, strictly pinned
│   └── <id>@<sha | sha256-first12>/  cloned/extracted plugin bodies
└── build/               [OUTPUT]      fully regenerated by `ccync sync`
    ├── lock.json                      resolved lockfile (_personalPlugins, _mcpServers, _adoptMaster, _ccyncProjection); in the backup set — carries non-rebuildable adoption snapshots
    ├── render/                        canonical root: skills/, commands/, agents/, hooks/, .mcp.json
    ├── mcp/managed.json               aggregated managed MCP manifest
    └── active/<provider>/             per-agent projection bookkeeping
            │
            ▼
     projection ──► live agent config surfaces (outside ~/.ccync/):
                    ~/.claude.json, ~/.claude/skills/…, ~/.codex/config.toml,
                    ~/.copilot/mcp-config.json, opencode.json, Antigravity faces
```

This is the "install once, project everywhere" mechanism: `ccync sync` (or `add`/`remove`/`init`/`upgrade`, which all call the same engine) resolves and renders once under `build/`, then projects that single render onto every agent you selected. If you ever want to inspect the built binary catalog directly instead of through `ccync list`/`ccync add`, there is no file to open for it — the source is the binary.

## Backup & Restore

### `ccync backup [--output <dir>]`

Copies the machine-local state files that cannot be rebuilt into a backup directory:

- `config.json` — machine config (INPUT)
- `plugins.json` — personal plugin catalog (INPUT)
- `plugins.lock.json` (from `build/lock.json`) — an OUTPUT file, but included because it carries the init adoption snapshots (`_adoptedItems` / `_mcpServers`), which cannot be regenerated from the INPUT files

Files not present on the machine (e.g. `plugins.json` on a machine with no personal plugins) are skipped with a note — that is not an error. `cache/` and the rest of `build/` are deliberately excluded: they are fully rebuildable by `ccync sync`.

### `ccync restore --from <dir>`

Restores the same three files from a previously created backup directory to their live locations (`plugins.lock.json` goes back to `build/lock.json`). Files absent from the backup — including `plugins.json` in backups taken before it joined the set — are skipped with a note. Existing files at the destination are overwritten. Run `ccync sync` afterwards to rebuild the derived state and re-project onto your agents.

## Exit Codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Operational error |
| `3` | Updates available — a read-only outdated check found at least one pending upgrade (`ccync list --upgrade-available`, `ccync upgrade --dry-run`). Distinct from `1` so scripts can tell "updates available" from a real failure. |
| `64` | Usage error (e.g., an unknown or missing command, or an invalid flag) |
