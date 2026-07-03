# ccync Architecture

ccync consists of a single Rust binary (`ccync`) and a curated plugin catalog. It resolves the catalog into a lockfile, fetches and caches plugins, and then projects them onto the native surface of every selected coding agent.

## Crate Tree

The architecture is built upon six crates. `ccync-foundation` serves as the dependency root, while `ccync-cli` is the final binary that aggregates all components.

| Crate | Role | Dependencies |
| --- | --- | --- |
| `ccync-foundation` | Handles paths (`~/.ccync/...`), machine configuration, ledger, mode, and health primitives. | — |
| `mcp` | Resolves and projects MCP servers into each agent's MCP host configuration. | `ccync-foundation` |
| `projection` | Provides per-agent serializers and the `ManagedArtifactRegistry`. It writes skills, commands, agents, and MCP configurations onto agent surfaces, and handles cross-agent adoption and reconciliation. | `ccync-foundation` |
| `ccync-engine` | The core management engine. Manages catalog resolution, adoption, reconciliation, installation, and the `doctor` command. It also defines CLI types like `ExitCode` and `CommandKind`. | `ccync-foundation`, `projection`, `mcp` |
| `setup` | Management health checks aggregated into `ccync doctor`. The former machine-setup orchestrator and its interactive runtime-selection session seam were removed — `ccync init` now writes runtime selection into `config.json` directly, and install is `ccync sync`. | `ccync-foundation`, `projection`, `ccync-engine`, `mcp` |
| `ccync-cli` | The main `ccync` binary containing the command handlers. | `ccync-foundation`, `projection`, `mcp`, `ccync-engine`, `setup` |

```txt
ccync-foundation        (root: paths, config, ledger, mode, health)
   ├── mcp
   ├── projection
   ├── ccync-engine     (-> projection, mcp)
   ├── setup            (-> projection, ccync-engine, mcp)
   └── ccync-cli        (binary; -> all of the above)
```

The dependency graph (DAG) is derived from each crate's `Cargo.toml [dependencies]` section.

## Public / Internal Command Split

`CommandKind` in `ccync-engine` contains all known subcommands — both public (14) and internal (2):

| Category | Commands | Surface |
| --- | --- | --- |
| Public (14) | `init`, `sync`, `add`, `remove`, `list`, `show`, `doctor`, `backup`, `restore`, `uninstall`, `update`, `upgrade`, `cleanup`, `pin` | Listed in `ccync --help` via `CommandKind::PUBLIC` |
| Internal (2) | `refresh`, `restore-source` | Dispatchable but absent from `--help`; documented in `devguide.md` |

`CommandKind::ALL.len() == 16`, `CommandKind::PUBLIC.len() == 14`. `print_help()` iterates `PUBLIC` only, so internal commands cannot be discovered via help. Both categories dispatch through the same `classify_args` / `Action::NotWired` path.

The four package-manager-parity additions (`show`, `cleanup`, `pin`, and the `list --upgrade-available` flag) reuse existing engine seams: `list --upgrade-available` / `upgrade --dry-run` share `resolve_upgrades` + a `report_to_exit` three-state code (`0` current / `3` `UpdatesAvailable` / `1` error); `cleanup` prunes orphaned `~/.ccync/cache/<id>@<sha>` dirs via the fail-closed `cache::orphan_cache_dirs`; `pin` writes a `held` flag (distinct from the `pin`/`pinnedSha` *sha* fields) that threads `plugins.json` → `splice_personal_namespace` → lockfile → `resolve_upgrades` (`UpgradeOutcome::Held`). The same pin-truth work made `managed_plugin_dirs` render the exact `<id>@<pin>` cache dir instead of every `<id>@*`.

`ccync update` (binary self-update, in `ccync-engine::self_update`) and `ccync upgrade` (git-source plugin re-resolve, in `ccync-engine::plugin_upgrade`) are the two public verbs added with Homebrew `brew update` / `brew upgrade` semantics. `upgrade` re-resolves each git plugin's remote `HEAD` against its pinned commit, re-clones changed plugins into a fresh `~/.ccync/cache/<id>@<new-sha>` (cache-verified before the pin is rewritten — pin-after-cache), atomically re-pins, and reuses the existing `run_unified_projection` to reproject.

## Source Resolution (`ccync add`)

The `ccync add <source>` command supports four types of sources, all of which are processed through the same `_personalPlugins` pipeline:

| Source Type | Detection Logic | Fetch Mechanism | Cache Key |
| --- | --- | --- | --- |
| Git URL | Starts with `http://`, `https://`, `git@`, or `git://` | `git clone --depth 1` | `<id>@<commit-sha>` |
| Local Path | Contains `/` or `\`, or starts with `.` or `~` | `git clone --depth 1` (treated as a git working copy on disk) | `<id>@<commit-sha>` |
| Archive | Suffixed with `.zip`, `.tar.gz`, or `.tgz` | Read bytes, calculate SHA-256, and extract | `<id>@<sha256-first12>` |
| Catalog ID | A bare identifier (no path separators) | Looks up via `embedded_catalog_json()` to fetch the resolved source | Same as the resolved source type |

**Archive Extraction** (`fetch_archive_plugin` in `ccync-engine/src/install.rs`):
The engine reads the archive bytes and calculates their SHA-256 hash (the first 12 hexadecimal characters serve as the cache key). It extracts the contents to a temporary directory, automatically promotes a single GitHub-style root wrapper directory if present, and finally renames the directory to `~/.ccync/cache/<id>@<hash>`. This process is idempotent: if `<id>@*` already exists, it immediately returns `AlreadyPresent`.

**Path-Traversal Guard:**
Extraction is strictly protected against path-traversal vulnerabilities (e.g., zip-slip or tar-slip). Both extractors verify that every entry remains within the extraction directory before writing:

- The ZIP extractor relies on `enclosed_name()`.
- The TAR.GZ extractor validates each entry through `is_contained_relative`, explicitly rejecting absolute paths, drive prefixes, and `..` components. It fails closed if any unsafe entry is detected.

This guarantees that a maliciously crafted archive cannot write outside the cache root, upholding ccync's strict invariant to "never touch a non-managed path" during installation.

**Catalog ID Resolution** (`resolve_catalog_source` in `ccync-engine/src/catalog.rs`):
The engine reads `embedded_catalog_json()`, locates the entry by its `pluginId`, and returns the `source` for `bundled-local` plugins, or `upstream.repo` for all others. The resolved source is then passed transparently into the standard Git or archive fetch pipeline.

## Data Flow

```txt
plugins/catalog.json  --resolve-->  ~/.ccync/build/lock.json
        │                                   │
   (curated + personal)               (resolved + pinned)
        │                                   │
        V                                   V
  ccync add                    ~/.ccync/cache/<id>@<sha or hash>/
  (git / local / archive / catalog-id)      │
        │                                   │
        └────────> render_canonical_root ───┴───> ~/.ccync/build/render/
                   (skills / commands / agents / hooks / .mcp.json)
                           │
                      projection ──────>  per-agent surfaces
                                         (~/.claude/skills, ~/.copilot/skills, …)
```

## Hooks Projection

Hooks follow the same canonical-root-only deployment strategy as skills, commands, and agents:

1. `render_canonical_root` (in `ccync-engine/src/install.rs`) copies the `hooks/` subtree of each managed plugin into `~/.ccync/build/render/hooks/`.
2. `write_surface_lifecycle_artifacts` (also in `ccync-engine/src/install.rs`) writes Claude's marketplace manifest with a `"ccync"` plugin entry whose `source` field is the **absolute** canonical-root path (not a relative `"./ccync"` string), so the entry always resolves to the real `~/.ccync/build/render/` tree — there is no unmaterialized intermediate directory. That manifest's lifecycle metadata records `"mode": "session-load-only"`; Claude actually loads the plugin root (and therefore `hooks/hooks.json`) via a `claude --plugin-dir <canonical-root>` session-load flow, not via a marketplace-driven install. The marketplace manifest itself stays a supplementary, non-consumed seam (`"cli": {"available": false}`), useful for a manual `claude plugin install`/`marketplace add` later, but not what makes hooks work today.
3. Non-Claude agents (such as Codex, Gemini CLI, and OpenCode) do not possess a CC-plugin hook surface. Therefore, hooks are simply inapplicable for those runtimes, rather than missing.
4. During `ccync remove`, `render_canonical_root` clears the component directories (`skills/`, `commands/`, `agents/`, `hooks/`) and the merged `.mcp.json` before re-rendering from the remaining plugins. This ensures all artifacts of removed plugins are completely pruned. Other files in the canonical root (like lifecycle manifests) remain untouched.
5. The `crates/projection/` directory (a Protected Path) is **never modified** by hook handling.

## State Topology (`~/.ccync/`)

The full machine-state tree. `plugins/catalog.json` (the repo-relative **curated** catalog) is a source contract and is **not** under `~/.ccync/`; it is embedded directly into the `ccync` binary at compile time (`include_str!`) and resolved straight from that embedded copy — there is no disk-deploy step, and `~/.ccync/build/render/catalog.json` is never written. The machine-local **personal** catalog is the separate `~/.ccync/plugins.json` — do not conflate the two.

Three classes of data live under `~/.ccync/`:

- **INPUT** — user intent; never auto-deleted and cannot be rebuilt by `sync`.
- **CACHE** — fetched plugin bodies; rebuildable from network/source.
- **OUTPUT** — everything under `build/`; fully regenerated by `ccync sync`.

`ccync backup` covers both INPUT files plus one OUTPUT exception — `build/lock.json`, whose init adoption snapshots are not rebuildable (see the table below); CACHE and the rest of OUTPUT are excluded as rebuildable.

```txt
~/.ccync/                              all ccync machine state
├── config.json          [INPUT]       machine config (devMode/ccyncRoot, secrets)
├── plugins.json         [INPUT]       personal plugin catalog (written by `ccync add`)
├── cache/               [CACHE]       personal plugin bodies, strictly pinned
│   └── <id>@<sha | sha256-first12>/  cloned/extracted plugin bodies
└── build/               [OUTPUT]      fully regenerated by `ccync sync`
    ├── lock.json                      resolved+pinned lockfile (_personalPlugins, adoption, _ccyncProjection, _mcpServers)
    ├── render/                        canonical root (rendered by `ccync sync`)
    │   ├── skills/  commands/  agents/  hooks/
    │   ├── .mcp.json                  merged MCP servers
    │   └── lifecycle manifests        (marketplace.json, runtime state, …)
    ├── mcp/
    │   └── managed.json               aggregated managed MCP manifest
    └── active/<provider>/             active provider projection
            │
            └── projection ──► per-agent live surfaces (outside ~/.ccync/):
                                ~/.claude/agents, ~/.agents/skills, ~/.claude.json,
                                ~/.codex/config.toml, copilot, opencode, agy faces
```

| Path | Class | Contents |
| --- | --- | --- |
| `~/.ccync/config.json` | INPUT | Machine configuration (`devMode`, `ccyncRoot`, `secrets`). |
| `~/.ccync/plugins.json` | INPUT | Personal plugin catalog (modified by `ccync add`). |
| `~/.ccync/cache/<id>@<sha>/` | CACHE | Git-cloned personal plugins, strictly pinned by their commit SHA. |
| `~/.ccync/cache/<id>@<hash>/` | CACHE | Archive-sourced personal plugins, strictly pinned by their archive SHA-256 prefix. |
| `~/.ccync/build/lock.json` | OUTPUT | Resolved catalog lockfile encompassing `_personalPlugins`, adoption state, `_ccyncProjection`, and the full MCP server definition snapshot (`_mcpServers`). **Backup exception:** although OUTPUT, it is included in the `ccync backup`/`restore` set (as `plugins.lock.json`) because the init adoption snapshots (`_adoptedItems` / `_mcpServers`) cannot be regenerated from the INPUT files; everything else `sync` rebuilds is excluded. |
| `~/.ccync/build/render/` | OUTPUT | Canonical root: `skills/`, `commands/`, `agents/`, `hooks/`, `.mcp.json`, and lifecycle manifests. |
| `~/.ccync/build/mcp/managed.json` | OUTPUT | Aggregated managed MCP manifest. |

**Source-of-truth vs live-surface boundary:** ccync's own state (config, lockfile, cache, canonical render) lives entirely under `~/.ccync/`, and that tree is where `backup`/`restore` operate. But ccync also *projects* that state onto each selected agent's live config surface outside `~/.ccync/` — `~/.claude/skills/...`, `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and equivalents for the other supported agents. Writing to those live surfaces is the entire point of "install once, project everywhere"; it is gated by the First-Run Overwrite Visibility Gate and tracked by `ManagedArtifactRegistry` so ccync only ever touches artifacts it created.

## Key Architecture Decisions

- **Universal Installation:** `ccync add` handles any CC-plugin source (Git URL, local path, archive, or catalog ID) using a single command and a unified `_personalPlugins` pipeline. No source type requires a special code path.
- **Canonical-Root Hooks:** CC-plugin hooks (`hooks/hooks.json`) rely on Claude's plugin-root mechanism. ccync materializes these hooks into its canonical root, which Claude seamlessly reads. This approach requires zero modifications to `crates/projection/` (a Protected Path).
- **Cross-Agent via Projection:** A single installation is projected to every selected agent's native surface via the `projection` engine and the `ManagedArtifactRegistry`. The registry meticulously tracks managed artifacts to ensure pruning operations never inadvertently delete non-ccync files.
- **Independent On-Disk Identity:** ccync's own state lives entirely under `~/.ccync/`, uses its own canonical plugin IDs, and manages its own projection surfaces there — independent of any other tool's state. Projecting that state onto each agent's live config surface outside `~/.ccync/` is a deliberate, gated write (see the source-of-truth vs live-surface boundary above), not an exception to this identity.
- **Lossless `config.json` writes:** Any code that writes back `~/.ccync/config.json` (e.g. `ccync init`'s runtime-selection write, the legacy `install-state.json` fold) must merge over the **raw JSON object**, updating only the keys it owns and preserving every other key verbatim. The typed `CcyncConfig` struct does **not** model `secrets` (which `mcp::resolver` reads straight from raw JSON), so a typed load→modify→serialize round-trip silently drops the user's credentials. Writers mirror `migrate::fold_install_state_into_config`; `init::write_runtime_selection_at` is the reference implementation, guarded by a `secrets`-preservation regression test.
