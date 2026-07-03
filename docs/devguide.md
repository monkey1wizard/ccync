# ccync Maintainer Guide

This guide covers the internal mechanics for developers working on ccync. For information on the crate dependency graph (DAG) and data flow, please refer to [`architecture.md`](architecture.md). For details on the command-line interface, see [`manual.md`](manual.md).

## Build, Test, and Lint

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace
```

The primary executable is `ccync` (defined in the `ccync-cli` crate as `[[bin]] name = "ccync"`). ccync's own application state (config, lockfile, cache, canonical render) lives under the `~/.ccync/` directory; see State Ownership Boundaries below for how that relates to the live agent-config surfaces ccync also writes.

> **Testing Note:** Some `setup::health` tests read machine-local `~/.ccync` state and can be sensitive to your local environment. If you hit a flaky health-check test, check `.dev/plans/` for any active plan tracking it, or the README Roadmap section for known follow-up work.

## Component Locations

| Concern | Crate / Module |
| --- | --- |
| Core paths (`~/.ccync/...`), configuration, ledger, and execution modes | `ccync-foundation` (`paths.rs`, `config.rs`, `ledger.rs`, `mode.rs`) |
| Catalog resolution to lockfile | `ccync-engine::catalog` |
| Personal plugin fetching (Git cloning, pinning) | `ccync-engine::install` (`fetch_*personal*`) |
| Projection / sync orchestration (internal "update" of derived state via `install::run_update` — distinct from the public `ccync update` self-update verb) | `ccync-engine::install` and `ccync-cli::commands::lifecycle` |
| Binary self-update (`ccync update`) and git-plugin upgrade (`ccync upgrade`) | `ccync-engine::{self_update, plugin_upgrade}` and `ccync-cli::commands::{update, upgrade}` |
| Cross-agent adoption (master-agent state → ccync truth store, `_mcpServers` snapshot) | `ccync-engine::adopt` and `ccync-cli::commands::init` |
| Per-agent projection and managed artifact tracking | `projection` (`ManagedArtifactRegistry`, per-agent serializers) |
| MCP host configuration and live-path enumeration | `mcp`, `ccync-engine::install::generate_managed_mcp` |
| Management health checks aggregated into `ccync doctor` | `setup` (`health` module only — the former interactive session-selection seam was removed) |
| Command dispatching | `ccync-cli::main` (utilizing `CommandKind` in `ccync-engine::lib`) |
| Argument discipline (unknown-flag / extra-positional rejection) | `ccync-cli::commands::args::reject_unknown_flags` |

**Argument discipline (every verb).** Each `cmd_*` handler must call `commands::args::reject_unknown_flags` at its entry, passing its exact allowed flag set, value-bearing flags, and positional cap. This is what makes `docs/manual.md`'s "invalid flag → exit 64" contract true and prevents silent-ignore bugs (a `remove <id> --dry-run` that ignored the flag and performed a real delete). The helper handles both `--flag value` and `--flag=value` forms and treats any single-dash token as a flag. **When adding a new verb, wire the guard** — the `all_verbs_reject_unknown_flag_as_usage` canary test (iterating `CommandKind::ALL`) is a smoke-net, but the authoritative coverage is a per-verb unknown-flag test.

## The Projection Engine

The `projection` crate is the core mechanism enabling ccync's "install once, project everywhere" capability. Its key components include:

- **Per-Agent Serializers:** These write a plugin's skills, commands, and agents into each agent's specific native directory structure (e.g., `~/.claude/skills/<name>`, `~/.copilot/skills/...`, Codex's `~/.agents/skills`, and the native locations for Antigravity, Gemini, and OpenCode).
- **`ManagedArtifactRegistry`:** Located at `~/.ccync/build/lock.json#_ccyncProjection` and featuring per-source attribution, this registry tracks the exact paths created by ccync. This guarantees that pruning operations only delete ccync's own artifacts, never user-created or third-party files. The `can_mutate()` safety guard provides fail-safe protection (if no prior lockfile exists during the first run, it falls back to a content-marker check).
- **Core-Wins Collision Policy:** If collisions occur, the earliest loaded source takes precedence. Subsequent conflicting entries are skipped, and a warning is logged.

## Catalog → Lockfile → Cache Pipeline

1. The central catalog, `plugins/catalog.json` (which contains curated `git-clone` companion entries and profiles), serves as the single source of truth. It is embedded directly in the binary via `include_str!` at compile time; catalog resolution reads the embedded copy directly — there is no disk-deploy step, and no `~/.ccync/plugins/ccync/catalog.json` file is ever written.
2. The catalog resolver (which runs internally during `ccync sync`) merges the embedded catalog, machine configuration, and the personal catalog (`~/.ccync/plugins.json`) into `~/.ccync/build/lock.json`. To prevent corruption, resolver keys are safely spliced into the lockfile without overwriting `_ccyncProjection` or other isolated namespaces.
3. When `ccync add` is executed, it clones Git repositories into `~/.ccync/cache/<id>@<sha>/`, pins them by their specific commit, and records the entry under the `_personalPlugins` section of the lockfile.

## First-Run Overwrite Visibility Gate

Before writing to any live agent configuration file on a new machine, `run_unified_projection(dry_run: bool, assume_yes: bool, skip_first_run_gate: bool)` (located in `crates/ccync-cli/src/commands/lifecycle.rs`) verifies the existence of `_ccyncProjection` in the lockfile.

If it is absent (indicating a first-run scenario) and `skip_first_run_gate` is `false`, the engine calls `mcp::live_mcp_target_paths()` to enumerate every file slated for modification (such as `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and the OpenCode configuration). It then prints this list to the console and awaits explicit user confirmation. In non-TTY environments, this operation requires the `--yes` flag or the `CCYNC_ASSUME_YES=1` environment variable; if neither is provided, the write operation is safely aborted (exit `Success`, with a hint to re-run).

Subsequent runs bypass this gate because the lockfile will now contain the `_ccyncProjection` key.

Division of labor across the four lifecycle commands: `sync`, `add`, and `remove` all share this gate as-is (`remove` needs `--yes` to bypass it non-interactively, same as `sync`/`add`); `init` passes `skip_first_run_gate=true` and skips the gate entirely (it already showed its own agent-selection UI before calling `run_unified_projection`); `upgrade` runs the gate but passes `assume_yes=true`, so it always auto-confirms without prompting. `add`'s live-surface render/projection happens strictly *after* the gate passes — a declined gate leaves catalog/cache/lockfile writes in place but produces zero live-surface writes.

## Cross-Agent Adoption

Running `ccync init [<claude|codex>]` reads the installation state of the specified master agent. It adopts any non-managed items into the ccync lockfile, records the `_adoptMaster` key, and subsequently executes the unified projection engine. Configuration writes are atomic, and any native entries unrelated to ccync within the agent's configuration are carefully preserved. This entire process is idempotent.

## State Ownership Boundaries

- **Reproducible from Source:** This repository itself (via `git clone` and `cargo build`).
- **Rebuildable Output State:** The `~/.ccync/build/` directory. This can be fully recreated by running `ccync sync`.
- **Machine-Local Input State:** `~/.ccync/config.json` and `~/.ccync/plugins.json`. Never auto-deleted by ccync. `ccync backup` / `ccync restore` target both INPUT files **plus one exception from the OUTPUT class**: `build/lock.json` (backed up as `plugins.lock.json`), because it carries the init adoption snapshots (`_adoptedItems` / `_mcpServers`) that cannot be regenerated from the INPUT files. The rest of `build/` and all of `cache/` stay out of the backup set — they are rebuildable.
- **Rebuildable Cache:** `~/.ccync/cache/`. Rebuildable from network/source but may require connectivity.

**Source-of-truth vs live-surface boundary:** ccync's own state (config, lockfile, cache, canonical render) lives entirely under `~/.ccync/`, and that tree is where `backup`/`restore` operate. But ccync also *projects* that state onto each selected agent's live config surface outside `~/.ccync/` — `~/.claude/skills/...`, `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and equivalents for the other supported agents. Writing to those live surfaces is the entire point of "install once, project everywhere"; it is gated by the First-Run Overwrite Visibility Gate above and tracked by `ManagedArtifactRegistry` so ccync only ever touches artifacts it created.

## Release Architecture

Details regarding the build, signing, and publishing processes—including binary-only archives, the GitLab-to-GitHub split, `release.yml`, cosign keyless integration, winget/Homebrew manifests, and submission gates—are comprehensively documented in [`release.md`](release.md).

## Known Deferred Work

Outstanding follow-up work is tracked in `.dev/plans/` (active plans) and the README Roadmap section, not in a standalone deferred-work file.

- **Resolved:** Projection previously required a source-checkout engine; this has since been resolved — see the README Roadmap section for the universal-installer follow-up scope.
- **Resolved:** `ccync doctor`'s canonical-root staleness check previously flagged the dynamically rendered canonical root as anomalous on every run (a no-op false positive). This has been fixed: `ClaudePluginCacheCheck` now receives the real canonical root (`canonical_plugin_root()`) and its staleness check is a genuine, non-no-op health signal.

## Internal Developer Tooling

These commands are dispatchable but intentionally absent from `ccync --help`. They are for use by ccync maintainers only and are not part of the public product surface.

### `ccync refresh [--source <path>]`

Rebuilds the derived layer from source without running the full public `sync` lifecycle. Internally calls the same projection core as `sync` (canonical render + machine projection + MCP projection), but skips the first-run gate, catalog resolve, and interactive prompts.

- **`--source <path>`** — accepted but ignored; the rebuild always reads `~/.ccync` state, not a source tree (same semantics as `ccync refresh --help`).
- Idempotent: running twice in succession produces byte-identical derived outputs.
- Does not modify `~/.ccync/config.json` or `~/.ccync/plugins.json` (machine-local INPUT files).

Use case: after editing source, quickly materialize changes to local machine state for observation or testing without going through the full managed-catalog pipeline.

### `ccync restore-source [--yes]`

Rolls back the ccync source repo to the latest stable release tag (`v*`), creates a backup branch, then rebuilds derived outputs via `refresh`.

**Prerequisites:**

- Must be run from inside the ccync source repo (fingerprint: `Cargo.toml` + `crates/ccync-cli` + `plugins/catalog.json`).
- At least one stable release tag (`v*`, prereleases like `-rc` excluded) must exist. Until one does, `restore-source` safely fails closed (returns an error, no mutation).

**Guards (all must pass):**

1. `is_ccync_source_repo()` fingerprint check.
2. Fail-closed resolution of the latest stable release tag, verified to be an ancestor of HEAD — a tag on a divergent branch is refused, to avoid a sideways `git reset`.
3. Discard scope output (shows uncommitted count and commits ahead of the target tag).
4. `--yes` flag required (without it, returns `Usage` with zero mutation).

**Rollback sequence (A-semantics):**

1. Create backup branch `ccync-restore-backup-<timestamp>` pointing at current HEAD.
2. `git reset --hard <release-tag>`
3. `git clean -fd`
4. Chain `ccync refresh` to rebuild derived layer.

> **Note:** `restore-source` is distinct from the public `ccync restore --from <dir>`, which restores machine-local INPUT files from a backup directory and does not touch the source repo.

> **Ownership note:** ccync only manages files that carry its content header (`# Generated by CCYNC Setup-Machine…`). Any agent file without that header — including legacy agent files left over from a pre-carve install — is treated as user-owned and is **never** removed by ccync; no migration or sync deletes it. This is by design and permanent (not a temporary migration gap): clean up such leftover files manually (`rm`) if you no longer want them.

## Contributing

For guidelines on contributing to the project, please review [`contributing.md`](contributing.md).
