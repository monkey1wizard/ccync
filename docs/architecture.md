# ccync architecture

ccync is a single Rust binary (`ccync`) plus a curated plugin catalog. It resolves a catalog into
a lockfile, fetches/caches plugins, and projects them onto every selected coding agent's native
surface.

## Crate tree

Six crates. `ccync-foundation` is the dependency root; `ccync-cli` is the binary that aggregates
everything.

| Crate | Role | Depends on |
| --- | --- | --- |
| `ccync-foundation` | Paths (`~/.ccync/...`), machine config, ledger, mode, health primitives | — |
| `mcp` | Resolve + project MCP servers into each agent's MCP host config | `ccync-foundation` |
| `projection` | Per-agent serializers + `ManagedArtifactRegistry`; writes skills/commands/agents/MCP onto agent surfaces; cross-agent adopt/reconcile | `ccync-foundation` |
| `ccync-engine` | Management engine: `catalog`, `adopt`, `reconcile`, `install`, management `doctor`; CLI types (`ExitCode`, `CommandKind`) | `ccync-foundation`, `projection`, `mcp` |
| `setup` | Runtime-selection option types + interactive session seam (reused by `ccync init`) + management health checks | `ccync-foundation`, `projection`, `ccync-engine`, `mcp` |
| `ccync-cli` | The `ccync` binary; command handlers | `ccync-foundation`, `projection`, `mcp`, `ccync-engine`, `setup` |

```txt
ccync-foundation        (root: paths, config, ledger, mode, health)
   ├── mcp
   ├── projection
   ├── ccync-engine     (-> projection, mcp)
   ├── setup            (-> projection, ccync-engine, mcp)
   └── ccync-cli        (binary; -> all of the above)
```

The DAG is sourced from each crate's `Cargo.toml [dependencies]`.

## Source resolution (`ccync add`)

`ccync add <source>` accepts four source kinds; all land in the same `_personalPlugins` pipeline:

| Kind | Detection | Fetch | Cache key |
| --- | --- | --- | --- |
| Git URL | `https://`, `git@`, `git://` prefix | `git clone --depth 1` | `<id>@<commit-sha>` |
| Local path | contains `/`, `\`, `.` or `~` prefix | directory walk | `<id>@<commit-sha>` |
| Archive | `.zip`, `.tar.gz`, `.tgz` suffix | read + SHA-256 + extract | `<id>@<sha256-first12>` |
| Catalog ID | bare identifier (no separators) | looks up `embedded_catalog_json()` → fetches resolved source | same as resolved kind |

Archive extraction (`fetch_archive_plugin` in `ccync-engine/src/install.rs`): reads the archive
bytes, hashes them (SHA-256 first 12 hex chars = cache key), extracts to a temp dir, promotes a
single GitHub-style root wrapper directory if present, then renames to
`~/.ccync/local/cache/<id>@<hash>`. Idempotent: `<id>@*` already present → `AlreadyPresent`.

**Extraction is path-traversal-guarded (zip-slip / tar-slip).** Both extractors contain every
entry inside the extraction dir before writing: the zip path uses `enclosed_name()`; the tar.gz
path gates each entry through `is_contained_relative` (rejects absolute, drive-prefix, and `..`
components) and fails closed on any unsafe entry. A crafted archive cannot write outside the cache
root — this upholds ccync's "never touch a non-managed path" invariant at the install boundary.

Catalog ID resolution (`resolve_catalog_source` in `ccync-engine/src/catalog.rs`): reads
`embedded_catalog_json()`, finds the entry by `pluginId`, and returns `source` for
`bundled-local` plugins or `upstream.repo` for all others. The resolved source is then fed into
the git/archive fetch pipeline unchanged.

## Data flow

```txt
plugins/catalog.json  --resolve-->  ~/.ccync/state/plugins.lock.json
        │                                   │
   (curated + personal)               (resolved + pinned)
        │                                   │
        V                                   V
  ccync add                    ~/.ccync/local/cache/<id>@<sha or hash>/
  (git / local / archive / catalog-id)      │
        │                                   │
        └────────> render_canonical_root ───┴───> ~/.ccync/plugins/ccync/
                   (skills / commands / agents / hooks / .mcp.json)
                           │
                      projection ──────>  per-agent surfaces
                                         (~/.claude/skills, ~/.copilot/skills, …)
```

## Hooks projection

Hooks follow the same canonical-root-only path as skills/commands/agents:

1. `render_canonical_root` (in `ccync-engine/src/install.rs`) copies each managed plugin's
   `hooks/` subtree into `~/.ccync/plugins/ccync/hooks/`.
2. Claude loads hooks from its plugin root; because `marketplace.json` maps `"./ccync"` to the
   canonical root, `hooks/hooks.json` is found there automatically.
3. Non-Claude agents (Codex, Gemini CLI, opencode) have no CC-plugin hook surface → hooks are N/A
   for those runtimes, not a projection gap.
4. On `ccync remove`, `render_canonical_root` clears the component dirs (skills/, commands/,
   agents/, hooks/) and the merged `.mcp.json` before re-rendering from the remaining plugins,
   ensuring removed-plugin artifacts are fully pruned. Other canonical-root files (catalog.json,
   lifecycle manifests) are left untouched.
5. `crates/projection/` (Protected Path) is **not touched** by hook handling.

## State topology (`~/.ccync/`)

| Path | Contents |
| --- | --- |
| `~/.ccync/config/` | machine config (`config.json`), MCP host config |
| `~/.ccync/state/plugins.lock.json` | resolved catalog lockfile + `_personalPlugins` + cross-agent adopt state |
| `~/.ccync/plugins/ccync/` | canonical root: `skills/`, `commands/`, `agents/`, `hooks/`, `.mcp.json`, `catalog.json`, lifecycle manifests |
| `~/.ccync/local/cache/<id>@<sha>/` | git-cloned personal plugins, pinned by commit SHA |
| `~/.ccync/local/cache/<id>@<hash>/` | archive-sourced personal plugins, pinned by archive SHA-256 prefix |
| `~/.ccync/local/catalog.json` | personal plugin catalog (`ccync add` writes here) |
| `~/.ccync/generated/mcp/managed.json` | aggregated MCP manifest |
| `~/.ccync/ledger.json`, `~/.ccync/install-state.json` | install ledger + runtime selection |

ccync's state is entirely under `~/.ccync/`. It does not use or modify `~/.gal/` (the separate
`gal` product's home).

## Decisions

- **Universal install.** `ccync add` accepts any CC-plugin source (git URL, local path, archive,
  catalog ID) through a single verb and a unified `_personalPlugins` pipeline. No source kind
  gets a special code path; gal is just another installable plugin.
- **Hooks are canonical-root-only.** CC-plugin hooks (`hooks/hooks.json`) are a Claude plugin-root
  mechanism. ccync materializes them into the canonical root which Claude reads as its plugin root.
  No changes to `crates/projection/` (Protected Path) are required or made.
- **Cross-agent by projection.** One install, projected to every selected agent's native surface
  via the `projection` engine + `ManagedArtifactRegistry` (which tracks managed artifacts so prune
  never deletes non-ccync files).
- **Independent on-disk identity.** ccync uses `~/.ccync/`, its own canonical plugin id, and its
  own projection surfaces — fully separate from `gal`.
