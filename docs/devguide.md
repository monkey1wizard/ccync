# ccync maintainer guide

Internals for working on ccync. For the crate DAG and data flow see
[`architecture.md`](architecture.md); for the command surface see [`manual.md`](manual.md).

## Build, test, lint

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace
```

The binary is `ccync` (crate `ccync-cli`, `[[bin]] name = "ccync"`). All state is under `~/.ccync/`.

> Known test note: `setup::health::tests::personal_layer_disabled_no_finding` reads the real
> `~/.ccync` config and is machine-state-dependent (pre-existing flake). See
> `.dev/bugs/ccync-carve-deferred.md` (B-07).

## Where things live

| Concern | Crate / module |
| --- | --- |
| Paths (`~/.ccync/...`), config, ledger, mode | `ccync-foundation` (`paths.rs`, `config.rs`, `ledger.rs`, `mode.rs`) |
| Catalog resolve → lockfile | `ccync-engine::catalog` |
| Personal plugin fetch (git clone, pin) | `ccync-engine::install` (`fetch_*personal*`) |
| Install / update orchestration | `ccync-engine::install` + `ccync-cli::commands::lifecycle` |
| Cross-agent adopt / reconcile | `ccync-engine::{adopt, reconcile}`, `ccync-cli::commands::lifecycle` |
| Per-agent projection + managed-artifact tracking | `projection` (`ManagedArtifactRegistry`, per-agent serializers) |
| MCP host config + live-path enumeration | `mcp`, `ccync-engine::install::generate_managed_mcp` |
| Machine setup / runtime selection / health | `setup` |
| Command dispatch | `ccync-cli::main` (`CommandKind` in `ccync-engine::lib`) |

## Projection engine

`projection` is the heart of ccync's "one install → all agents". Key pieces:

- **Per-agent serializers** write a plugin's skills / commands / agents into each agent's native
  surface (`~/.claude/skills/<name>`, `~/.copilot/skills/...`, Codex `~/.agents/skills`, Antigravity
  native, Gemini, opencode).
- **`ManagedArtifactRegistry`** (`~/.ccync/state/plugins.lock.json#_ccyncProjection`, with
  per-source attribution) tracks exactly which paths ccync created, so prune only deletes ccync's
  own artifacts — never a user's or another tool's files. The `can_mutate()` guard is fail-safe
  (first run with no prior lockfile falls back to a content-marker check).
- **Core-wins collision policy**: an earlier source wins; later collisions are skipped + warned.

## Catalog → lockfile → cache

1. `plugins/catalog.json` (curated `git-clone` companions + profiles) is the single source; it is
   embedded in the binary and deployed to the canonical root on `ccync init`.
2. The catalog resolver (internalized inside `ccync sync`) merges catalog + machine config + personal
   catalog (`~/.ccync/local/catalog.json`) → `~/.ccync/state/plugins.lock.json`. Resolver keys are
   spliced in without clobbering `_ccyncProjection` / other namespaces (corrupt-file fail-safe).
3. `ccync add` git-clones into `~/.ccync/local/cache/<id>@<sha>/`, pinned by commit, recorded under
   lockfile `_personalPlugins`.

## First-run overwrite-visibility gate

Before the first live agent-config write on a new machine, `run_unified_projection`
(`crates/ccync-cli/src/commands/lifecycle.rs`) checks whether `_ccyncProjection` exists in the
lockfile. If absent (first run), it calls `mcp::live_mcp_target_paths()` to enumerate the files
that will be written (`~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`,
opencode config), prints the list, and waits for confirmation. Non-TTY mode requires `--yes` or
`CCYNC_ASSUME_YES=1`; without either the write is skipped cleanly. Subsequent runs skip the gate
(lockfile carries `_ccyncProjection`). All four callers (`init`, `sync`, `add`, `remove`) share the
same gate via `run_unified_projection(dry_run, assume_yes)`.

## Cross-agent adoption

`ccync init [<claude|codex>]` reads the master agent's install state, adopts non-managed items into
the lockfile, records `_adoptMaster`, then runs the unified projection engine. Atomic config writes;
agent-native unrelated entries preserved. Idempotent.

## State ownership boundary

- **Reproducible from source**: this repo (`git clone` + build).
- **Rebuildable derived**: `~/.ccync/plugins/`, `~/.ccync/generated/` — recreate with `ccync init`.
- **Machine-local**: `~/.ccync/config/config.json`, `~/.ccync/state/plugins.lock.json`,
  `~/.ccync/install-state.json` — covered by `ccync backup` / `ccync restore`.

ccync never reads/writes `~/.gal/` (the separate `gal` product's home).

## Release architecture

Build / sign / publish (binary-only archive, GitLab→GitHub split, `release.yml`, cosign
keyless, winget/homebrew manifests, submission gate) lives in its own doc:
[`release.md`](release.md).

## Known deferred work

Live gaps from the carve are tracked in `.dev/bugs/ccync-carve-deferred.md`:

- **B-01** (high): projection still requires a source-optional engine — making it source-optional is
  the core of `feat-ccync-universal-installer`.
- **B-02**: `ccync doctor` flags ccync's non-baked canonical root (re-scope the checks).
- **B-03/B-04**: dead `gal-core` references in `projection` / `setup`.

## Contributing

See [`contributing.md`](contributing.md).
