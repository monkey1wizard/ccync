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

## Projection Vocabulary Guard

`crates/ccync-engine/tests/projection_vocabulary.rs` enforces the [`naming.md`](naming.md#glossary) glossary on every `cargo test --workspace`. It is the only mechanical enforcement of that glossary — ccync still ships no `naming-gate` CLI command.

**What it scans.** Three roots: `crates/**/*.rs`, `docs/**/*.md`, and the root `README.md`. Symlinks are never followed, `target/` is skipped, and the guard's own source is exempt (it necessarily quotes every allowed token as string data). Adding a new crate source file or a new doc page puts it under the guard automatically — no registration step.

**What it rejects.** Any identifier-like token (a maximal run of `[A-Za-z0-9_]`) that case-insensitively contains the retired projection-domain word, unless the exact pair is in the embedded `ALLOWED` array.

**Granularity is `(file path, matched token)`, not `(file, line).`** Once a pair is allowlisted, every occurrence of that exact token in that file passes. This is a deliberate trade: it keeps the list near 100 entries instead of hundreds of line coordinates, and it still blocks the growth vector that matters — any *new* file or *new* token.

**Adding an allowlist entry.** Add an exact `(path, token)` pair, and justify it in one line in the commit message. It must fit one of four reviewed categories:

1. a legacy config-key boundary spelling (`selectedRuntimes` / `primaryRuntime` / old-layout `runtimes` — read forever, never renamed);
2. a pre-existing engine/CLI identifier outside the rename's blast radius (the earlier chains renamed the projection-domain *type* model, not every function and test name);
3. a literal quote of a live CLI output string;
4. genuine execution-environment or third-party meaning (C `Runtime` linkage, `VCRUNTIME140.dll`).

The bare-word entries already in `ALLOWED` (e.g. `("crates/ccync-engine/src/install.rs", …)`) are **grandfathered**, not a pattern to copy: admitting them per-file was the only way to seal the guard without a repo-wide rename. Widening an existing bare-word entry to a new file needs the same scrutiny as a rename.

**The guard cannot pass vacuously.** `assert_scan_roots_exist` fails if any of the three scan roots is missing, and `MIN_SCAN_HITS` is a conservative floor (set well below the real token count) so a scan that silently found nothing — unreadable root, `repo_root()` resolving outside the checkout — fails loudly instead of reading as clean. A companion test seeds a synthetic violation, and another rejects duplicate `ALLOWED` entries, so the detector and the list are both proven sound.

**If it fires:** rename the token per the glossary map first. Allowlisting is the exception, not the default fix.

## Component Locations

| Concern | Crate / Module |
| --- | --- |
| Core paths (`~/.ccync/...`) and configuration | `ccync-foundation` (`paths.rs`, `config.rs`) |
| Catalog resolution to lockfile | `ccync-engine::catalog` |
| Personal plugin fetching (Git cloning, pinning) | `ccync-engine::install` (`fetch_*personal*`) |
| Projection / sync orchestration (internal "update" of derived state via `install::run_update` — distinct from the public `ccync update` self-update verb) | `ccync-engine::install` and `ccync-cli::commands::lifecycle` |
| Binary self-update (`ccync update`) and git-plugin upgrade (`ccync upgrade`) | `ccync-engine::{self_update, plugin_upgrade}` and `ccync-cli::commands::{update, upgrade}` |
| Cross-agent adoption (master-agent state → ccync truth store, `_mcpServers` snapshot) | `ccync-engine::adopt` and `ccync-cli::commands::init` |
| Adopted-item source discovery/lookup/repair-tip seams (`ccync search`/`ccync repair`/`ccync upgrade` share this) | `ccync-engine::adopt` (`adopted_items_without_source`, `find_adopted_item`, `write_adopted_source`) |
| Public repository discovery (`ccync search`) and adopted-item source repair (`ccync repair`) — CLI-first, no engine-level network module | `ccync-cli::commands::{search, repair}` |
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

### Two-Row Ownership and the Prune Authorization Ladder

`_ccyncProjection` records each projected path **twice**: once as a member of a managed-artifact category (`skillProjectionPaths`, `commandProjectionPaths`, …) and once as a `sourceAttribution` row naming its owning source. The two rows are written by different code paths and can desynchronize, so neither is by itself a proof of ownership:

- A **category row without an attribution row** is the pre-attribution shape (a lockfile written before attribution tracking, or a path recorded through `mark` rather than `mark_with_source`). `owning_source_of` returns `None`, which must fall through to normal authorization — never be read as "foreign", or every legacy entry becomes unprunable.
- An **attribution row without a category row** is an orphan: the path was pruned or replaced somewhere that did not clear its attribution alongside it. `ManagedArtifactRegistry::persist` garbage-collects these, but only for sources installed this pass; a foreign source's orphan is retained because this pass has no authority to judge it. The GC runs **before** persist's `prior == next` equality short-circuit, or an orphan-only change would be short-circuited away and never written.
- A **false attribution row** is the dangerous shape: a path recorded as ccync-owned that ccync never actually created. `ensure_dir_link` returns `bool` precisely to prevent it — `false` means a genuine user-owned directory was preserved untouched, and every call site guards `mark_with_source` on that return. An unconditional mark after a preserve is what writes a user's own directory into ccync's ownership records.

Deletion authority is a ladder, tried in order: a ccync content marker on a real path; a symlink whose target is proven to resolve inside its attributed owner's root; a symlink whose path appears in a prior category row (`can_mutate`); a plain file recorded as managed (`is_managed`). The second rung exists only for orphan symlinks and is deliberately limited to symlinks — it authorizes removing the link itself, never anything the link points at. Its dangling-target case reads the raw `read_link` value and requires it to be absolute and free of `..` components, and compares it to the owner root **without** canonicalizing, because on Windows `canonicalize` yields a `\\?\` verbatim path that would never prefix-match the caller's plain root.

### Bundle-Uninstall Forward Contract

`installed_source_ids` (built from `ctx.opts.sources` / the `sources` passed into `prune_stale_links`) is the full currently-installed source list for a projection pass — not "installed before this uninstall." A source being uninstalled must still appear in it, with zero `keep_names`, during its own final projection pass.

This is why stale-link pruning and attribution GC treat that pass's own entries as ordinary (unprotected) instead of foreign: `prune_stale_links` only skips a path when its recorded owner is foreign — attributed to a source absent from `installed_source_ids` entirely, meaning the pass has no authority over it. Dropping the removed source out of `installed_source_ids` early would make its own stale entries look foreign and leave them stranded on disk instead of pruned. The same list feeds `ManagedArtifactRegistry::persist`'s orphan GC, which likewise only retains attribution rows owned by a source outside `installed_source_ids`.

## Catalog → Lockfile → Cache Pipeline

1. The central catalog, `plugins/catalog.json` (which contains curated `git-clone` companion entries and profiles), serves as the single source of truth. It is embedded directly in the binary via `include_str!` at compile time; catalog resolution reads the embedded copy directly — there is no disk-deploy step, and no `~/.ccync/plugins/ccync/catalog.json` file is ever written.
2. The catalog resolver (which runs internally during `ccync sync`) merges the embedded catalog, machine configuration, and the personal catalog (`~/.ccync/plugins.json`) into `~/.ccync/build/lock.json`. To prevent corruption, resolver keys are safely spliced into the lockfile without overwriting `_ccyncProjection` or other isolated namespaces.
3. When `ccync add` is executed, it clones Git repositories into `~/.ccync/cache/<id>@<sha>/`, pins them by their specific commit, and records the entry under the `_personalPlugins` section of the lockfile.

## First-Run Overwrite Visibility Gate

Before writing to any live agent configuration file on a new machine, `run_unified_projection(dry_run: bool, assume_yes: bool, skip_first_run_gate: bool, gate_abort_context: Option<&GateAbortContext>)` (located in `crates/ccync-cli/src/commands/lifecycle.rs`) verifies the existence of `_ccyncProjection` in the lockfile.

If it is absent (indicating a first-run scenario), the engine builds a read-only `TouchScope` via `build_touch_scope` and **prints the grouped touch-scope disclosure unconditionally**. The disclosure is not gated on `skip_first_run_gate` — only the *confirmation prompt* that follows it is. `skip_first_run_gate == true` (init's case) suppresses the prompt and nothing else, so `init` gains visibility without gaining a second prompt.

**The disclosure is two groups from two different owners, not one flat list.** This split is a truth claim, not formatting:

| Group | Rendered label | Path owner | Scope |
| --- | --- | --- | --- |
| Selected agent surfaces | `Runtime surfaces (would touch, current selection):` | `ccync_engine::install::planned_engine_touches` | Varies with the config's agent selection |
| Fixed MCP host files | `MCP host files (may be written; skipped when already identical):` | `mcp::live_mcp_target_paths()` | The fixed four hosts, independent of agent selection |

The two verbs differ because the guarantees differ. Agent surfaces are a **preview** ("would touch" — the live CLI label, quoted verbatim above, still reads `Runtime surfaces`; renaming that literal string is a code change out of this plan's scope) — `run_unified_projection`'s `projection::run_machine_update` call is best-effort per target, so a listed surface may end up untouched; a genuine machine-phase fault is reported to stderr and folded into an `Error` exit by the engine-owned run accumulator (see [`architecture.md`](architecture.md#projection-run-evidence-and-status-authority)), it is no longer swallowed into a success exit. MCP hosts are "**may be written**" — `write_json_provider_delta` returns `Unchanged` (bytes *and* mtime preserved) on a semantic no-op, so a converged machine writes none of those four paths. Neither group may claim more than it can prove.

`render_touch_scope` is the single renderer for all three consumers — the `--dry-run` report, the generic first-projection confirmation gate, and `init`'s pre-projection disclosure — so the same scope can never be formatted two different ways. A CLI-local second target-to-path table would violate this contract: the disclosure planner must reuse the engine/projection path owner.

The confirmation prompt, when it runs, awaits explicit user confirmation. In non-TTY environments it requires the `--yes` flag or the `CCYNC_ASSUME_YES=1` environment variable; if neither is provided, the write is aborted **before any live-surface write and returns `ExitCode::Error` (exit `1`)** — distinguishable from a full success — after printing a truthful three-part summary from `gate_abort_context`: which ccync state was *already written*, which surfaces were *not written*, and the `ccync sync --yes` next step. The abort message deliberately never claims "no files written" — that literal phrasing is delegated to `--dry-run` output only, and even there it is narrower than it reads: it covers live-surface writes, not the one-time internal layout migration that always runs first and is not gated by `--dry-run` (see [`manual.md`](manual.md#ccync-sync---dry-run---yes)).

Subsequent runs bypass this gate because the lockfile will now contain the `_ccyncProjection` key.

Division of labor across the four lifecycle commands: `sync`, `add`, and `remove` all share this gate as-is (`remove` needs `--yes` to bypass it non-interactively, same as `sync`/`add`); `init` passes `skip_first_run_gate=true`, which skips **only the confirmation prompt** — it still prints the same grouped disclosure, because it already showed its own agent-selection UI before calling `run_unified_projection` and that UI is the intent signal; `upgrade` runs the gate but passes `assume_yes=true`, so it always auto-confirms without prompting. Because the surfaces already written before the gate differ per verb, each caller passes its own `GateAbortContext` constant (`SYNC_`/`ADD_`/`REMOVE_GATE_ABORT_CONTEXT`) so the abort summary is truthful: `sync` has updated the lockfile/resolved state; `add` has updated cache + personal catalog + lockfile; `remove` has additionally already run the canonical render. `add`'s live-surface render/projection happens strictly *after* the gate passes — a declined gate leaves those command-specific catalog/cache/lockfile writes in place but produces zero live-surface writes.

## Cross-Agent Adoption

Running `ccync init [<claude|codex>]` reads the installation state of the specified master agent. It adopts any non-managed items into the ccync lockfile, records the `_adoptMaster` key, and subsequently executes the unified projection engine. Configuration writes are atomic, and any native entries unrelated to ccync within the agent's configuration are carefully preserved. This entire process is idempotent.

### Adopted Source Recording

Adopted items are written into three isolated namespaces inside `~/.ccync/build/lock.json`:

- **`_adoptedItems`** — marketplace plugins discovered in the master agent's live config. Each entry carries a `sourceId` field — an agent-and-kind tag such as `claude-skill` or `claude-mcp` (written by `adopt::source_id`) that identifies which secondary namespace it also occupies: a `skill`-tagged entry has a corresponding entry in `_looseSkills`, an `mcp`-tagged entry in `_mcpServers`. Each entry also carries `name`, `origin: "adopted"`, an optional `source` (a resolvable clonable reference — from catalog resolution at adoption time, or written later by `ccync repair`; absent when unknown), and, for marketplace plugins, a `baselineVersion` snapshot taken at adoption time.
- **`_looseSkills`** — free-standing skill directories found in the master agent's skills surface but not traceable to any catalog entry. Each entry records the raw source path so ccync can re-materialize the directory into the canonical root during sync.
- **`_mcpServers`** — MCP server entries from the master agent's configuration. Stored as a key-value map; keys match the agent-side MCP server identifiers.

The `sourceId` field links an `_adoptedItems` entry to its secondary namespace: `ccync engine::adopt::remove_adopted_item` reads `sourceId` to determine which secondary namespace to also clear when the item is removed via `ccync remove`.

**Filling a missing `source` after the fact.** `ccync init` resolves a plugin's `source` from the local marketplace catalog when it can (`read_marketplace_plugin_source`), but that resolution can miss (unknown marketplace, missing local clone, plugin absent from the catalog) or simply not apply (MCP servers and loose skills have no marketplace catalog at all). `ccync repair [<id>]` is the dedicated follow-up for that gap: it writes a user-confirmed `source` onto an existing `_adoptedItems` row via `adopt::write_adopted_source` (a unique-name, fail-closed atomic write — see [`architecture.md`](architecture.md#ccync-repair--layering-and-the-metadata-enrichment-boundary)), independent of `ccync init`/`ccync sync` and without triggering a projection. `adopt::adopted_items_without_source` is the shared seam that both `ccync repair`'s batch mode and the `upgrade`/`list --upgrade-available`/`init` repair-tip all read.

### Loose-Skill Materialization

During `ccync init`, after the `_adoptedItems` / `_looseSkills` snapshot is written, `adopt_loose_skills` runs to copy each loose skill directory into the canonical root (`~/.ccync/build/render/skills/`). This makes adopted loose skills immediately visible to every downstream agent without an extra `ccync sync` step.

`adopt_loose_skills` is a pure copy step — it never modifies the source agent's skill directory. On subsequent `ccync sync` calls the canonical root is rebuilt from the lockfile, so loose skills re-materialize from the `_looseSkills` snapshot automatically.

## State Ownership Boundaries

- **Reproducible from Source:** This repository itself (via `git clone` and `cargo build`).
- **Rebuildable Output State:** The `~/.ccync/build/` directory. `ccync sync` recreates the *content* every file under `build/` should hold — but `build/lock.json`'s init adoption snapshots (`_adoptedItems` / `_mcpServers`) are not re-derivable this way (`sync` never re-queries a master agent; only `ccync init` does), and `build/mcp/{managed,projected-state,projection.txn}.json` carry MCP-ownership proof whose *loss* (not their bytes) matters for recovery — see the exceptions in the next bullet and [`manual.md`](manual.md#machine-layout).
- **Machine-Local Input State:** `~/.ccync/config.json` and `~/.ccync/plugins.json`. Never auto-deleted by ccync. `ccync backup` / `ccync restore` target both INPUT files **plus one exception from the OUTPUT class**: `build/lock.json` (backed up as `plugins.lock.json`), because it carries the init adoption snapshots (`_adoptedItems` / `_mcpServers`) that cannot be regenerated from the INPUT files. The rest of `build/` and all of `cache/` stay out of the backup set.
- **Rebuildable Cache:** `~/.ccync/cache/`. Rebuildable from network/source but may require connectivity.

**Source-of-truth vs live-surface boundary:** ccync's own state (config, lockfile, cache, canonical render) lives entirely under `~/.ccync/`, and that tree is where `backup`/`restore` operate. But ccync also *projects* that state onto each selected agent's live config surface outside `~/.ccync/` — `~/.claude/skills/...`, `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and equivalents for the other supported agents. Writing to those live surfaces is the entire point of "install once, project everywhere"; it is gated by the First-Run Overwrite Visibility Gate above and tracked by `ManagedArtifactRegistry` so ccync only ever touches artifacts it created.

## Release Architecture

Details regarding the build, signing, and publishing processes—including binary-only archives, the GitLab-to-GitHub split, `release.yml`, cosign keyless integration, winget/Homebrew manifests, and submission gates—are comprehensively documented in [`release.md`](release.md).

## Known Deferred Work

Where follow-up work lives, by altitude:

- **Product-level, user-visible** → the README Roadmap section (e.g. Windows clean-machine acceptance, which is hardware-gated).
- **In-flight** → `.dev/plans/` active plans.
- **Maintainer-level engineering backlog** → this section. Items land here when the plan that found them closes: a plan file is deleted at lifecycle close, so anything still owed must be recorded somewhere durable first. Each item is self-contained (file:line + why it was deferred) so a later plan can act on it without the originating conversation. There is still no standalone deferred-work file.

- **Resolved:** Projection previously required a source-checkout engine; this has since been resolved — see the README Roadmap section for the universal-installer follow-up scope.
- **Resolved:** `ccync doctor`'s canonical-root staleness check previously flagged the dynamically rendered canonical root as anomalous on every run (a no-op false positive). This has been fixed: `ClaudePluginCacheCheck` now receives the real canonical root (`canonical_plugin_root()`) and its staleness check is a genuine, non-no-op health signal.

### Open engineering backlog

Recorded at the close of `fix-fresh-user-audit` (2026-07-16). None are release-blocking; each needs its own plan.

1. **`irm | iex` clobbers caller variables and functions.** `packaging/install.ps1:29-30` assign `$Repo`/`$InstallDir` at script scope, which under `iex` *is* the caller's scope; the file also defines 9 functions into the caller's session (proven: function count 45→53; a caller's own `Write-Info 'probe'` printed `ccync-install: probe` after `iex`). The installer-preference contract (preferences do not leak) is met — this is beyond its text. The fix is to wrap the file body in `& { ... }` or prefix all names; deferred because it reworks the structure that was just stabilized and endorsed.
2. **Codex `toml_string` control-character escaping gap.** `crates/mcp/src/serializers/codex.rs:141-144` escapes only `\` and `"`; a resolved value containing a control character renders unparseable TOML. Fail-closed (the fingerprint fires during planning, before any write), but such a user can never sync at all.
3. **Codex uninstall does not consult `unaddressable`.** `crates/mcp/src/lib.rs:970-997` — `run_mcp_remove_managed_inner` calls `write_codex_delta` for every owned name. If a user restructures `~/.codex/config.toml` into an inline `[mcp_servers]` key after ccync committed ownership, the line-based writer strips nothing, returns `Ok`, uninstall reports `hosts_updated: [codex]`, then deletes `projected-state.json` — stranding the entry permanently. Pre-existing; the `unaddressable` concept that would close it exists but is applied only to the projection path.
4. **`write_json_provider_delta` inserts `"mcpServers": {}`** into a `~/.claude.json` that lacks the key, rewriting (and, with `serde_json` lacking `preserve_order`, reordering) the file once even with an empty desired manifest. Pre-existing; compounds the `preserve_order` item.
5. **`write_atomic`'s fixed `<dest>.tmp` sibling** collides between concurrent processes writing the same destination — belongs with the file-locking backlog item.
6. **Unbounded in-RAM tree load in the snapshot walker.** `collect_dir_snapshot` (`crates/ccync-engine/src/install.rs:643-687`) holds the entire tree in memory (`Vec<(String, Vec<u8>)>`) with no size cap and no ignore list; the plain-directory source is exactly where `node_modules`/`target/` live. Deferred because a streaming fix must preserve the read-once/hash-and-copy-the-same-bytes property that makes the walker TOCTOU-free — a design task, not an edit. Failure mode is loud (OOM/thrash), not silent data loss.
7. **Dead second author-facing `.mcp.json` reader in the Protected Path.** `crates/projection/src/lib.rs:167-189` `decompose_plugin` accepts only `"servers"`, violating the one-author-facing-reader rule (see [`architecture.md`](architecture.md#author-facing-vs-internal-mcp-keys)). It is dead code (no production caller) inside `crates/projection/` (a Protected Path), so deleting it or repointing it to `read_author_mcp_servers` needs its own plan with a recorded architect review.
8. **Empty directories invisible to the snapshot.** The walker (`crates/ccync-engine/src/install.rs:669-670`) neither hashes nor recreates empty directories, so two trees differing only by empty dirs share a cache key and the snapshot silently drops them.
9. **Temp-dir prefix collision in `fetch_personal_plugin`.** `crates/ccync-engine/src/install.rs:447` treats any cache entry matching `starts_with("<id>@")` as `AlreadyPresent`, which can match `@__copying__`/`@__fetching__`/`@__extracting__` staging residue left by a hard kill. The snapshot fetcher's own path is more careful (exact `target.exists()`); align the incumbents with it. This is distinct from the *intended* short-circuit on this same prefix — a genuine, already-populated `<id>@<sha>`/`<id>@<sha256-first12>` cache dir correctly skips re-fetch/re-extraction for Git, archive, and plain-directory sources alike; only the staging-residue false positive above is the debt.
10. **Copilot-agents deselect-cleanup asymmetry.** Deselected `gemini`/`opencode` commands get cleaned via else-branches; deselected copilot agents do not — their files are left in place. Fail-safe (leaves files rather than deleting them), but asymmetric. Needs its own architect-reviewed prune change, since it is a prune/registry/mutation-authorization change.
11. **Render / managed-MCP failures still exit 0 with a warning.** Pre-existing best-effort contract. Now that the ownership model makes data loss impossible, the remaining harm is staleness only — but the exit code is still not truthful about it.

Recorded at the close of the docs-vs-implementation reconciliation plan (2026-07-21). Documentation-only fixes landed in this plan for all three; the underlying code/behavior gap each describes remains open.

12. **Refresh/sync timestamp non-determinism has no regression coverage.** The per-target lifecycle writers driven by `apply_lifecycle_artifact_chain` (`install.rs`) stamp a fresh `"generatedAt"` into each target-state manifest on every write, so two successive `ccync refresh`/`ccync sync` runs are content-idempotent but not byte-identical — yet nothing tests this boundary. `refresh_with_writes_canonical_and_is_idempotent` (`refresh.rs`) only compares `(relative_path, file_size)` pairs per file, which cannot detect a timestamp-only content diff. Evidence: `install.rs` `"generatedAt": chrono::Utc::now().to_rfc3339()` call sites; `refresh.rs`'s `dir_snapshot`/`collect_dir` test helpers. Owner: unassigned — needs its own plan. Status: open (docs corrected in this plan; code/test gap unchanged). Remediation: add a regression test that explicitly documents the timestamp exception (e.g. asserts every *other* byte is identical), or make the manifest timestamp stable when nothing else in the render changed.
13. **`devMode`/`ccyncRoot` are parsed but functionally dead.** The config model still parses and round-trips both fields, but `run_update`'s mode string is hardcoded to `"normal"` (`install.rs:538-540`) — no production code path reads either field to change behavior. Evidence: `install.rs:538-540`. Owner: unassigned. Status: open (docs now label both fields inactive, this plan; the fields themselves are still accepted input). Remediation: either remove both fields (a config that already sets them today observes zero behavior change either way) or reinstate a real dev-mode code path if the split is still wanted.
14. **`ccync doctor --dry-run`/`--release-gate` are accepted but no-op.** `doctor.rs` parses both flags; `run_doctor` discards them unconditionally (`let _ = opts.dry_run; ... let _ = opts.release_gate;`, `crates/ccync-engine/src/doctor.rs:34,62`) — no check varies by either flag today. Evidence: `crates/ccync-engine/src/doctor.rs:34,62`. Owner: unassigned. Status: open (docs now disclose the no-op, this plan). Remediation: either wire `--release-gate` to a stricter CI-oriented check set (its apparent original intent) or remove both flags from the CLI surface.

Recorded at the close of the same plan (2026-07-22), from a defect the landing gate caught in the plan's own output.

15. **Rust identifiers cited in prose have no continuous existence guard.** `crates/ccync-engine/tests/projection_vocabulary.rs` guards *vocabulary* tokens across `crates/`, `docs/`, and `README.md`, but nothing checks that a function or type named in documentation prose still exists in the tree. Both failure directions have already occurred: `docs/architecture.md` cited `write_surface_lifecycle_artifacts` from 2026-07-03 and went silently false when that symbol was retired for `apply_lifecycle_artifact_chain` on 2026-07-19; `docs/devguide.md` then acquired two fresh citations of the same already-retired symbol on 2026-07-22. Neither was caught by `cargo test --workspace` — only by the landing gate, which harvests plan evidence prose rather than `docs/`. Evidence: retirement commit `ad94a481`; the surviving explanatory comment at `crates/ccync-engine/src/install.rs:4056`. Owner: unassigned. Status: open (all three citations corrected 2026-07-22; the missing guard is unchanged). Remediation: extend the vocabulary guard, or add a sibling test, to resolve backticked `snake_case`/`CamelCase` identifiers appearing in `docs/`+`README.md` against declarations under `crates/`, with an allowlist for the many legitimate non-Rust backticked tokens (config keys, JSON fields, CLI flags).

16. **Lockfile mutations in `adopt.rs` are non-atomic except the newest one.** `crates/ccync-engine/src/adopt.rs` rewrites `build/lock.json` from four sites (adoption write-back, loose-skill materialization, MCP snapshot, and `remove_adopted_item`) through a plain `std::fs::write`, which truncates the destination before writing — a crash or a full disk mid-write leaves a truncated lockfile carrying the non-rebuildable `_adoptedItems`/`_mcpServers` adoption snapshots. `write_adopted_source` (added by `ccync repair`) is the module's only caller of `ccync_foundation::platform::atomic_write_bytes` (tmp + rename). The inconsistency is not merely cosmetic: `docs/architecture.md` and `docs/manual.md` both asserted, until this plan's landing review, that repair used "the same primitive the rest of ccync's lock-file mutations use" — the uniformity reads as true from any single call site. Evidence: `adopt.rs` `std::fs::write` sites vs. its single `atomic_write_bytes` call. Owner: unassigned. Status: open (the two false doc claims corrected at this plan's landing; the four non-atomic write sites unchanged). Remediation: route every `adopt.rs` lockfile write through `atomic_write_bytes`, in its own change — each site has its own error-path and test surface, and `backup`/`restore` treat this file as the one non-rebuildable OUTPUT.

Recorded at the close of `chore-remove-ledger-residue` (2026-07-27), from a defect that plan's own landing review caught.

17. **Line-granular grep allowlists hide a retired concept's surviving clause.** Retirement plans in this repo express their acceptance as "`grep -n <concept> <file>` matches only the allowed lines" — but the allowlist unit is a *line*, while the stale prose unit is a *clause*. `docs/manual.md:56` was allowlisted wholesale because its "any legacy `ledger.json`" clause stays true after the retirement; the same line's trailing "No uninstall ledger is written." survived the entire nine-task plan and every per-task audit, and is only true in a world where some *other* operation still writes one. Nothing mechanical can see this: the line is on the allowlist, so its full text is never re-read. The same shape is available to every prior retirement here (`dist/runtimes/`, `install-state.json`, the `gal`→`ccync` rename), each of which left surviving legacy-cleanup mentions on allowlisted lines. Evidence: the removal landed at this plan's landing review as `6188d2bf` (+ mirror `575f5f2f`), not during the manual-update task that rewrote the rest of that file. Owner: unassigned. Status: open (this instance corrected; the acceptance-criterion shape unchanged). Remediation: state retirement acceptance at clause granularity — for each allowlisted line, assert *what* the surviving mention says, not merely that the line is permitted to mention it — or have the landing review re-read the full text of every allowlisted line rather than the grep summary.

18. **A line-number-anchored allowlist drifts silently when the file above it changes.** Probe 1 in `packaging/test-release-artifacts.sh` used to allowlist this file's one deliberate mention of the retired project name by pinning `DEVGUIDE_ALLOWED_LINE` to an ordinal line number and stripping an exact phrase from that line before re-checking. This was clause-granular (the remedy item 17 above prescribed) but positionally fragile: inserting or deleting any line above the pinned number shifted the target. The common case was fail-closed and loud — the real mention moved off the pinned line and the probe went red — but there was a silent case, where whatever sat on that line carried its own mention of the retired name and got allowlisted without anyone asking for it. Evidence: introduced by `e91fa0d7` during the `chore-degal-residue` guard-hardening work; the drift itself resurfaced live during the `feat-release-chain-parity` plan's own release-smoke test run (the phrase had moved to line 189 by the time that plan started, while the probe was still pinned to 173, so the probe was already red before this item was fixed). Owner: unassigned. Status: **closed** (`feat-release-chain-parity`) — the allowlist now matches the exact phrase wherever it appears in the file, with no line number involved at all, so it survives edits anywhere above or below it.
19. **The two residue probes disagree about whether the generated agent-instruction adapters are in scope.** Probe 1's scan root is `crates/` plus `docs/` plus five explicit packaging files; the eight tracked adapter files at the repo root and under the dot-directories are kept out on purpose, because they are regenerated tooling output and scanning them would turn the harness red after every regeneration. Probe 1b, added to the same harness in the same plan, iterates `git ls-files` and excludes only `.dev/**` — so those same eight adapters *are* in its scan root, against the rationale stated one probe above it. This is latent rather than active: the adapters carry zero plan-ID matches today. It becomes real the moment a regeneration copies a plan-ID-bearing line out of `.dev/project.md`, which still carries several such references in its own prose — at which point the harness goes red over content this repo cannot durably edit, since the next regeneration overwrites it. Evidence: `6eb4565d`, the `chore-degal-residue` commit that added the second probe; exposure measured at that plan's landing review — 8 tracked adapters, 0 current matches. Owner: unassigned. Status: open. Remediation: pick one scan root for both probes — either exclude the adapter paths from Probe 1b the way Probe 1 excludes them, or bring them into both and accept the regeneration coupling; do not leave the two probes disagreeing about the same files.

### Carried verification condition — unix walker arm is NotRun

The snapshot walker's two `#[cfg(unix)]` tests and its **non-Windows production arm** have never compiled or run anywhere: ccync's development box is Windows-only and a cross-target check was attempted and blocked. They were reviewed by reading and judged compile-plausible and low-risk, but they are labelled **NotRun, not pass** — an unexercised arm is not a passing arm. They are owed exactly one unix run (CI or WSL). Do not treat the walker's unix path as verified until that run exists.

### Known partial satisfaction — MCP `--dry-run` reporting

`ccync sync --dry-run` inspects and reports MCP state, but only *half* of what the originating requirement asked for. This is a recorded architectural limit, not an oversight:

| Half | Status | Notes |
| --- | --- | --- |
| Never write / commit / clear ownership, transaction file, or live config | **Satisfied** | Substantively, not vacuously — dry-run performs a real read-only inspection of MCP/live-surface state (the one-time internal layout migration is a separate, unconditional step that always runs first — see the dry-run caveat above) |
| Report legacy bootstrap and pending transaction | **Satisfied** | |
| Report planned deletion and collision | **Not satisfied** | Reported explicitly as "not determined (requires render)" rather than as an empty list |

The reason is structural. Planned deletions and collisions come from `planner::plan_host`, which needs `desired` — and `desired` exists only *after* the render (`crates/mcp/src/lib.rs:714-726`). The render is `ccync_engine::install::run_update`: the whole projection, which writes live surfaces like `~/.claude/skills/ccync`. A dry run that called it would no longer be a dry run. `generate_managed_mcp` cannot be reused standalone either — its core input is the `canonical_root/.mcp.json` that `render_canonical_root` writes, and it unconditionally writes `managed.json`. The only code that computes the desired MCP set lives in `render_canonical_root`, inside the Protected Path `crates/projection/`.

Closing this requires a new pure-function `desired_manifest(config)` seam that merges plugin/bundle/adopted MCP in memory — a cross-crate refactor touching a Protected Path, which needs its own plan and its own architect review.

Reporting "not determined" rather than an empty list is deliberate: an empty list would be a false claim of "nothing to delete, no collisions", which is exactly the kind of unproven assertion the MCP ownership model exists to prevent.

## Internal Developer Tooling

These commands are dispatchable but intentionally absent from `ccync --help`. They are for use by ccync maintainers only and are not part of the public product surface.

### `ccync refresh`

Rebuilds the derived layer from existing `~/.ccync` state without running the full public `sync` lifecycle. Internally calls the same projection core as `sync` (canonical render + machine projection + MCP projection), but skips the first-run gate, catalog resolve, and interactive prompts.

- Content-idempotent, not byte-identical: re-running with unchanged state re-renders the same components, but the per-target lifecycle writers driven by `apply_lifecycle_artifact_chain` stamp a fresh `"generatedAt"` timestamp into each target-state manifest on every run (`install.rs`), so successive runs are not byte-for-byte identical even when nothing else changed.
- Does not modify `~/.ccync/config.json` or `~/.ccync/plugins.json` (machine-local INPUT files).

Use case: after editing source, quickly materialize changes to local machine state for observation or testing without going through the full managed-catalog pipeline.

### `ccync rollback [--yes]`

Rolls back the ccync source repo to the latest stable release tag (`v*`), creates a backup branch, then rebuilds derived outputs via `refresh`.

**Prerequisites:**

- Must be run from inside the ccync source repo (fingerprint: `Cargo.toml` + `crates/ccync-cli` + `plugins/catalog.json`).
- At least one stable release tag (`v*`, prereleases like `-rc` excluded) must exist. Until one does, `rollback` safely fails closed (returns an error, no mutation).

**Guards (all must pass):**

1. `is_ccync_source_repo()` fingerprint check.
2. Fail-closed resolution of the latest stable release tag, verified to be an ancestor of HEAD — a tag on a divergent branch is refused, to avoid a sideways `git reset`.
3. Discard scope output (shows uncommitted count and commits ahead of the target tag).
4. `--yes` flag required (without it, returns `Usage` with zero mutation).

**Rollback sequence (A-semantics):**

1. Create backup branch `ccync-rollback-backup-<timestamp>` pointing at current HEAD.
2. `git reset --hard <release-tag>`
3. `git clean -fd`
4. Chain `ccync refresh` to rebuild derived layer.

> **Note:** `rollback` is distinct from the public `ccync restore --from <dir>`, which restores machine-local INPUT files from a backup directory and does not touch the source repo.

> **Ownership note:** ccync only manages files that carry its content header (`# Generated by CCYNC Setup-Machine…`). Any agent file without that header — including legacy agent files left over from a pre-carve install — is treated as user-owned and is **never** removed by ccync; no migration or sync deletes it. This is by design and permanent (not a temporary migration gap): clean up such leftover files manually (`rm`) if you no longer want them.

## Contributing

For guidelines on contributing to the project, please review [`contributing.md`](contributing.md).
