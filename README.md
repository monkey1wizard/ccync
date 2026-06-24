# ccync

**Cross-agent plugin / MCP / skills manager.** Install a plugin once and project it to every
coding agent you use — Claude Code, GitHub Copilot, Codex, Antigravity, Gemini, opencode.

ccync is the management half of a split: it manages arbitrary third-party plugins, MCP servers,
and skills, and keeps them consistent across all your agents. (The workflow half lives in a
separate product, `gal`.)

> **Status: alpha.** Plugin management — resolve, git-clone, cache, pin, cross-agent adopt /
> reconcile — works today. Full projection of installed plugins onto every agent's skill surface
> is in progress (see [Roadmap](#roadmap)).

## Install

**macOS / Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/install.sh | bash
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/install.ps1 | iex
```

Both scripts verify the binary against `checksums.txt` (SHA-256 mandatory; cosign best-effort).
The binary is placed in `~/.local/bin` (macOS/Linux) or `%USERPROFILE%\bin` (Windows).

**winget (Windows):** `winget install Monkey1Wizard.ccync` *(pending public listing)*

**homebrew (macOS/Linux):** *(pending public tap listing)*

**Build from source**

```sh
git clone https://gitlab.com/monkey1wizard/ccync.git
cd ccync
cargo build --release        # produces target/release/ccync
```

Put the built binary on your `PATH`:

- **macOS / Linux:** `cp target/release/ccync ~/.local/bin/`
- **Windows:** copy `target\release\ccync.exe` to a folder on your `PATH`

Verify:

```sh
ccync --version    # ccync x.y.z
```

## Quick start

```sh
# 1. first-run setup — pick a master agent + adopt its plugins/MCP
ccync init claude        # lists files it will write to; prompts before writing

# 2. project everything to every selected agent
ccync sync

# 3. add a third-party plugin — accepts git URL, local path, archive, or catalog ID
ccync add https://github.com/<owner>/<plugin>  # git URL
ccync add /path/to/plugin                       # local path
ccync add plugin-v1.tar.gz                      # archive (.zip / .tar.gz)
ccync add my-catalog-plugin                     # bare catalog ID

# 4. list what you have installed
ccync list
```

> **First run:** `ccync init` shows the live agent config files it is about to write and asks for
> confirmation. Pass `--yes` to skip the prompt in non-interactive shells.

State lives in `~/.ccync/` (hidden, machine-local). ccync never reads or writes `~/.gal/`.

## Commands

| Command | What it does |
| --- | --- |
| `ccync init [<master>]` | First-run setup — pick a master agent + adopt its plugins/MCP |
| `ccync sync [--dry-run] [--yes]` | The projection engine: resolve catalog → render → project skills/commands/agents/MCP to every agent. `--yes` skips the first-run confirmation. |
| `ccync add <source> [--no-sync]` | Add a personal plugin from any source (git URL, local path, `.zip`/`.tar.gz` archive, or catalog ID) + auto-sync |
| `ccync remove <id>` | Remove a personal plugin + auto-sync |
| `ccync list` | List installed personal plugins |
| `ccync doctor` | Read-only management health check |
| `ccync backup` / `ccync restore` | Export / import machine-local state |
| `ccync uninstall` | Remove ccync's managed state |

Run `ccync --help` for the full surface.

## How it works

- **Catalog** (`plugins/catalog.json`): the curated set of installable plugins + profiles.
- **Resolve** (inside `ccync sync`): catalog + machine config + personal catalog →
  `~/.ccync/state/plugins.lock.json`.
- **Universal install**: `ccync add <source>` accepts four source kinds — git URL, local path,
  archive (`.zip` / `.tar.gz`), or a bare catalog ID — through a single verb and a unified fetch
  pipeline. Archives are pinned by SHA-256 prefix; catalog IDs are resolved to their real source
  before fetching. All sources land in `~/.ccync/local/cache/<id>@<sha-or-hash>/`.
- **Canonical root**: `render_canonical_root` copies every managed plugin's `skills/`, `commands/`,
  `agents/`, and `hooks/` subtrees into `~/.ccync/plugins/ccync/` and merges `.mcp.json` entries.
  On every re-render (including after `ccync remove`), stale component dirs are pruned first.
- **Hooks**: plugins that ship `hooks/hooks.json` have their hooks projected into the canonical
  root and loaded automatically by Claude (via its plugin-root mechanism). Non-Claude agents
  (Codex, Gemini CLI, opencode) have no CC-plugin hook surface — hooks are N/A, not missing.
- **Projection** (inside `ccync sync`): the `projection` engine writes each plugin's skills /
  commands / agents / MCP into every selected agent's native surface.
- **First-run gate**: on a fresh machine (no prior projection), `ccync init` / `ccync sync` / 
  `ccync add` lists the live agent config files it will modify and asks for confirmation before
  writing. Pass `--yes` or set `CCYNC_ASSUME_YES=1` for non-interactive use.

See [`docs/architecture.md`](docs/architecture.md) for the crate tree and
[`docs/manual.md`](docs/manual.md) for the full command reference.

## Platform notes

| | macOS / Linux | Windows |
| --- | --- | --- |
| Home dir env | `HOME` | `USERPROFILE` |
| ccync home | `~/.ccync` | `%USERPROFILE%\.ccync` |
| Binary | `ccync` | `ccync.exe` |
| PATH install | `~/.local/bin` | a folder on `PATH` |

`~/.ccync` is hidden. Browse it with `cd ~/.ccync` (any OS), or enable "show hidden items" in your
file manager.

## Troubleshooting

- **`ccync: command not found`** — the binary isn't on `PATH`. Re-check [Install](#install), or run
  it by full path (`./target/release/ccync`).
- **`ccync add ... resolve step failed: Catalog not found`** — run `ccync init` first; it deploys
  the catalog ccync resolves against.
- **A plugin doesn't appear in an agent yet** — projection onto agent surfaces is the in-progress
  piece (alpha). The plugin is cloned + pinned (`ccync list`); full projection lands with the
  universal installer.
- **`ccync doctor` reports errors about the canonical root** — expected in alpha; it does not mean
  your install is broken.
- **Git errors during `add`** — ccync uses your system `git`; make sure `git` is installed and the
  repo URL is reachable.

## Documentation

This README is the entry point (install → first plugin → sync above). For everything else:

### Using ccync

- [Manual](docs/manual.md) — every command and flag.

### Working on ccync

- [Architecture](docs/architecture.md) — crate tree + data flow.
- [Maintainer guide](docs/devguide.md) — internals, projection engine, state topology.
- [Release architecture](docs/release.md) — build, sign, publish (cosign, winget/homebrew).
- [Naming](docs/naming.md) — reserved terms + the 8 canonical runtime keys.
- [Contributing](docs/contributing.md) — build, test, conventions.

## Roadmap

- **winget / homebrew public listing** — pending submission review.
- **Windows real-machine acceptance** — clean-machine install + sync validation on a fresh Windows box.

## License

MIT.
