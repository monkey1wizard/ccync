English | [繁體中文](docs/i18n/zh-Hant/README.zh-Hant.md)

# ccync

**A cross-agent plugin, MCP, and skills manager.** Install a plugin once and project it to every coding agent you use, including Claude Code, GitHub Copilot, Codex, Antigravity, Gemini, and OpenCode.

ccync serves as the management component of a broader ecosystem. It handles arbitrary third-party plugins, MCP servers, and skills, ensuring they remain consistent across all your agents. (The workflow counterpart is maintained as a separate product.)

> **Status: Alpha.** Core plugin management—including resolution, git cloning, caching, pinning, and cross-agent adoption/reconciliation—is fully functional. Projection of installed plugins onto every agent's skill, command, agent, and MCP surface is implemented and shipping.

## Install

### macOS / Linux

```sh
curl -fsSL https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/install.sh | bash
```

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/install.ps1 | iex
```

Both scripts verify the binary against `checksums.txt` (SHA-256 is mandatory, while cosign verification is best-effort).
The binary is installed to `~/.local/bin` on macOS/Linux, or `%USERPROFILE%\.local\bin` on Windows.

**winget (Windows):** `winget install Monkey1Wizard.ccync`

**Homebrew (macOS/Linux):**

```sh
brew tap monkey1wizard/tap
brew install ccync
```

### Build from Source

```sh
cargo install --git https://github.com/monkey1wizard/ccync
```

This installs directly to `~/.cargo/bin/ccync` without requiring a manual clone.

If you need the source tree (e.g., for development):

```sh
git clone https://github.com/monkey1wizard/ccync.git
cd ccync
cargo build --release        # Produces target/release/ccync
```

Next, add the built binary to your `PATH`:

- **macOS / Linux:** `cp target/release/ccync ~/.local/bin/`
- **Windows:** Copy `target\release\ccync.exe` to a directory included in your `PATH`.

Verify the installation:

```sh
ccync --version    # Outputs: ccync x.y.z
```

## Quick Start

```sh
# 1. Initial setup: Select a master agent and adopt its plugins and MCP configuration.
ccync init claude        # Skips master prompt; shows runtime multi-select, then projects immediately.

# 2. Project everything to all selected agents.
ccync sync

# 3. Add a third-party plugin. Accepts a git URL, local path, archive, or catalog ID.
ccync add https://github.com/<owner>/<plugin>   # Git URL
ccync add /path/to/plugin                       # Local path
ccync add plugin-v1.tar.gz                      # Archive (.zip / .tar.gz)
ccync add my-catalog-plugin                     # Bare catalog ID

# 4. List all installed personal plugins.
ccync list
```

> **First Run:** `ccync init` guides you through an interactive master-agent selection and runtime multi-select (defaults: `claude`, `codex`, `copilot`), then projects to the selected agents immediately — no separate confirmation step. In non-interactive shells, supply the master explicitly (`ccync init claude`); runtime defaults are applied automatically.

ccync's own state (config, lockfile, cache, canonical render) is maintained in `~/.ccync/` (a hidden, machine-local directory). ccync also *projects* that state onto each selected agent's live config surface outside `~/.ccync/` — `~/.claude/skills/...`, `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and equivalents for the other supported agents — which is the entire point of "install once, project everywhere."

## Commands

| Command | Description |
| --- | --- |
| `ccync init [<master>]` | Performs the initial setup by selecting a master agent and adopting its plugins/MCP. |
| `ccync sync [--dry-run] [--yes]` | The projection engine. Resolves the catalog, renders it, and projects skills, commands, agents, and MCP configurations to every agent. Use `--yes` to skip the first-run confirmation. |
| `ccync add <source> [--no-sync]` | Adds a personal plugin from any supported source (git URL, local path, `.zip`/`.tar.gz` archive, or catalog ID) and automatically synchronizes. |
| `ccync remove <id>` | Removes a personal plugin and automatically synchronizes. |
| `ccync list [--upgrade-available]` | Lists installed personal plugins; with `--upgrade-available`, a read-only outdated check printing `current → latest` (exit `3` when updates exist, `0` when none, `1` on error). |
| `ccync show <id>` | Prints full detail of one installed plugin (source, strategy, pinned sha, held, component count). Mirrors `brew info` / `winget show`. |
| `ccync doctor` | Runs a read-only management health check. |
| `ccync backup` / `ccync restore` | Exports or imports machine-local state. |
| `ccync uninstall` | Reverses ccync's projections (live MCP / marketplace / skill·command·agent surfaces) and removes derived `~/.ccync/` state, preserving `config.json` + `plugins.json` and never editing PATH. See the [manual](docs/manual.md#ccync-uninstall). |
| `ccync update [--check]` | Self-updates the `ccync` binary (Homebrew `brew update` semantics). Self-replaces only a curl/irm install; refuses + redirects under Homebrew/winget/cargo. `--check` reports versions without writing. |
| `ccync upgrade [<plugin>] [--dry-run]` | Upgrades installed git-source plugins to their latest commit (Homebrew `brew upgrade` semantics), then re-projects. Default all; `--dry-run` shows the diff without writing. Held plugins are skipped. |
| `ccync cleanup [--dry-run]` | Prunes orphaned `~/.ccync/cache/<id>@<sha>` dirs left by `upgrade` (Homebrew `brew cleanup` semantics). Fail-closed: never deletes a live or undecidable dir. `--dry-run` lists only. |
| `ccync pin <id>` · `--remove <id>` · `--list` | Holds a plugin against `upgrade` (winget-style pin; state in the `held` field — distinct from the `pin`/`pinnedSha` *sha* fields). |

Run `ccync --help` for the complete command surface.

## How It Works

- **Catalog** (`plugins/catalog.json`): The curated collection of installable plugins and profiles.
- **Resolution** (executed during `ccync sync`): Merges the catalog, machine configuration, and personal catalog to generate `~/.ccync/build/lock.json`.
- **Universal Install**: `ccync add <source>` accepts four types of sources—git URLs, local paths, archives (`.zip` / `.tar.gz`), and bare catalog IDs—using a single command and unified fetching pipeline. Archives are pinned by their SHA-256 prefix, while catalog IDs are resolved to their underlying source before fetching. All sources are stored in `~/.ccync/cache/<id>@<sha-or-hash>/`.
- **Canonical Root**: `render_canonical_root` copies the `skills/`, `commands/`, `agents/`, and `hooks/` subdirectories of every managed plugin into `~/.ccync/build/render/`, and merges `.mcp.json` entries. During every re-rendering (including after `ccync remove`), stale component directories are pruned first.
- **Hooks**: Plugins that provide `hooks/hooks.json` have their hooks projected into the canonical root, where Claude automatically loads them via its plugin-root mechanism. Since non-Claude agents (Codex, Gemini CLI, OpenCode) lack a hook surface for CC-plugins, hooks are simply inapplicable (not missing) for them.
- **Projection** (executed during `ccync sync`): The `projection` engine writes each plugin's skills, commands, agents, and MCP settings into every selected agent's native configuration format.
- **First-Run Gate**: On a new machine without prior projections, `ccync sync`, `ccync add`, and `ccync remove` all share this gate — they list the live agent configuration files they plan to modify and request confirmation before writing (`remove` needs `--yes` to bypass it non-interactively, same as `sync`/`add`). Use `--yes` or set the `CCYNC_ASSUME_YES=1` environment variable for non-interactive execution. `ccync init` skips the gate entirely — it already presents its own runtime multi-select UI before projecting. `ccync upgrade` runs the gate but always passes `assume_yes=true`, so it auto-confirms without prompting. For `add`, the live-surface render/projection happens strictly *after* the gate passes — a declined gate leaves catalog/cache/lockfile writes in place but produces zero live-surface writes.

For details on the crate hierarchy, refer to [`docs/architecture.md`](docs/architecture.md). For a comprehensive command reference, see [`docs/manual.md`](docs/manual.md).

## Platform Notes

| | macOS / Linux | Windows |
| --- | --- | --- |
| Home Directory Env | `HOME` | `USERPROFILE` |
| ccync Home | `~/.ccync` | `%USERPROFILE%\.ccync` |
| Binary | `ccync` | `ccync.exe` |
| PATH Installation | `~/.local/bin` | `%USERPROFILE%\.local\bin` (or any directory on your `PATH`) |

`~/.ccync` is a hidden directory. You can navigate to it using `cd ~/.ccync` on any OS, or by enabling "show hidden files" in your file manager.

## Troubleshooting

- **`ccync: command not found`** — The binary is not in your `PATH`. Review the [Install](#install) section or run it using its absolute path (e.g., `./target/release/ccync`).
- **`Failed to parse <file>: ...`** — One of ccync's JSON state files (e.g. `~/.ccync/plugins.json` or `~/.ccync/build/lock.json`) is malformed. Fix or remove the offending file; `~/.ccync/build/` is fully rebuildable via `ccync sync`, but `~/.ccync/plugins.json` is machine-local input state — restore it from a `ccync backup` if you have one, or re-add your personal plugins.
- **A plugin doesn't appear in an agent yet** — Confirm the plugin is enabled for that agent's runtime (`ccync list` shows what's installed), then run `ccync sync` to (re-)project. If it still doesn't appear, run `ccync doctor` to check for a stale or missing canonical root.
- **`ccync doctor` reports errors about the canonical root** — This indicates a real staleness or integrity issue (e.g. the canonical root is out of date relative to the lockfile). Run `ccync sync` to rebuild it.
- **Git errors during `add`** — ccync relies on your system's `git`. Ensure `git` is installed and the repository URL is accessible.

## Documentation

This README serves as the entry point (installation → first plugin → synchronization). For further details:

### Using ccync

- [Manual](docs/manual.md) — Comprehensive documentation of every command and flag.

### Working on ccync

- [Architecture](docs/architecture.md) — The crate hierarchy and data flow.
- [Maintainer Guide](docs/devguide.md) — Internals, projection engine mechanics, and state topology.
- [Release Architecture](docs/release.md) — The build, sign, and publish pipeline (including cosign keyless, winget/Homebrew, and submission gates).
- [Naming](docs/naming.md) — Reserved terminology and the 8 canonical runtime keys.
- [Contributing](docs/contributing.md) — Build instructions, tests, and code conventions.

## Roadmap

- **Windows Real-Machine Acceptance** — Validating a clean-machine installation and synchronization on a fresh Windows environment.

## Internal / Dev Tooling

ccync includes a small set of internal developer commands that are dispatchable but not listed in `ccync --help`. These are for use by ccync maintainers during development:

- **`ccync refresh`** — Rebuild the derived layer (canonical render + agent projection) from source without running the full sync lifecycle.
- **`ccync restore-source`** — Roll back the ccync source repo to the latest stable release tag (`v*`) and rebuild derived outputs.

See [`docs/devguide.md`](docs/devguide.md) for full contracts, prerequisites, and guard conditions.

## License

MIT.
