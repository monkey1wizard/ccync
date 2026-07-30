# ccync Architecture

ccync consists of a single Rust binary (`ccync`) and a curated plugin catalog. It resolves the catalog into a lockfile, fetches and caches plugins, and then projects them onto the native surface of every selected coding agent.

## Crate Tree

The architecture is built upon six crates. `ccync-foundation` serves as the dependency root, while `ccync-cli` is the final binary that aggregates all components.

| Crate | Role | Dependencies |
| --- | --- | --- |
| `ccync-foundation` | Handles paths (`~/.ccync/...`), machine configuration, and health primitives. | — |
| `mcp` | Resolves and projects MCP servers into each agent's MCP host configuration. | `ccync-foundation` |
| `projection` | Provides per-agent serializers and the `ManagedArtifactRegistry`. It writes skills, commands, agents, and MCP configurations onto agent surfaces, and handles cross-agent adoption and reconciliation. | `ccync-foundation` |
| `ccync-engine` | The core management engine. Manages catalog resolution, adoption, reconciliation, installation, and the `doctor` command. It also defines CLI types like `ExitCode` and `CommandKind`. | `ccync-foundation`, `projection`, `mcp` |
| `setup` | Management health checks aggregated into `ccync doctor`. The former machine-setup orchestrator and its interactive agent-selection session seam were removed — `ccync init` now writes agent selection into `config.json` directly, and install is `ccync sync`. | `ccync-foundation` |
| `ccync-cli` | The main `ccync` binary containing the command handlers. | `ccync-foundation`, `projection`, `mcp`, `ccync-engine`, `setup` |

```txt
ccync-foundation        (root: paths, config, health)
   ├── mcp
   ├── projection
   ├── ccync-engine     (-> projection, mcp)
   ├── setup            (foundation only)
   └── ccync-cli        (binary; -> all of the above)
```

The dependency graph (DAG) is derived from each crate's `Cargo.toml [dependencies]` section.

## Public / Internal Command Split

`CommandKind` in `ccync-engine` contains all known subcommands — both public (16) and internal (2):

| Category | Commands | Surface |
| --- | --- | --- |
| Public (16) | `init`, `sync`, `add`, `remove`, `list`, `show`, `doctor`, `backup`, `restore`, `uninstall`, `update`, `upgrade`, `cleanup`, `pin`, `search`, `repair` | Listed in `ccync --help` via `CommandKind::PUBLIC` |
| Internal (2) | `refresh`, `rollback` | Dispatchable but absent from `--help`; documented in `devguide.md` |

`CommandKind::ALL.len() == 18`, `CommandKind::PUBLIC.len() == 16`. `print_help()` iterates `PUBLIC` only, so internal commands cannot be discovered via help. Both categories dispatch through the same `classify_args` / `Action::NotWired` path.

The four package-manager-parity additions (`show`, `cleanup`, `pin`, and the `list --upgrade-available` flag) reuse existing engine seams: `list --upgrade-available` / `upgrade --dry-run` share `resolve_upgrades` + a `report_to_exit` three-state code (`0` current / `3` `UpdatesAvailable` / `1` error); `cleanup` prunes orphaned `~/.ccync/cache/<id>@<sha>` dirs via the fail-closed `cache::orphan_cache_dirs`; `pin` writes a `held` flag (distinct from the `pin`/`pinnedSha` *sha* fields) that threads `plugins.json` → `splice_personal_namespace` → lockfile → `resolve_upgrades` (`UpgradeOutcome::Held`). The same pin-truth work made `managed_plugin_dirs` render the exact `<id>@<pin>` cache dir instead of every `<id>@*`.

`ccync update` (binary self-update, in `ccync-engine::self_update`) and `ccync upgrade` (git-source plugin re-resolve, in `ccync-engine::plugin_upgrade`) are the two public verbs added with Homebrew `brew update` / `brew upgrade` semantics. `upgrade` re-resolves each git plugin's remote `HEAD` against its pinned commit, re-clones changed plugins into a fresh `~/.ccync/cache/<id>@<new-sha>` (cache-verified before the pin is rewritten — pin-after-cache), atomically re-pins, and reuses the existing `run_unified_projection` to reproject.

## ccync search — CLI-first Provider Architecture

`ccync search <term>` is a discovery command: it never writes to the lockfile, catalog, or `config.json` on its own. All of its logic — provider adapters, ranking/dedupe, TTY confirmation, and non-interactive rendering — lives entirely in `crates/ccync-cli/src/commands/search.rs`. This is a deliberate "CLI-first" layering choice: no engine-level network module was added, and no new HTTP client dependency was introduced. `ccync-engine` only carries `CommandKind::Search`, the dispatch tag used by `classify_args`; the engine has no knowledge of forges, subprocesses, or ranking.

**Providers, called unconditionally and merged, not fallback-chained.** Every run calls all four provider adapters — `gh` CLI, `glab` CLI, the unauthenticated GitHub web search API, and the unauthenticated GitLab web search API — regardless of whether an earlier provider already produced hits. `coordinate_search` merges every successful provider's output through `normalize_and_rank` (dedupe by normalized clone URL, then rank by exact-name match, stars, provider priority, and lexical order), rather than short-circuiting on the first success. Calling all four every time — instead of treating `gh`/`glab` as authoritative and the web APIs as a last resort — lets same-name-but-different-repo candidates from every forge surface side by side in one ranked list, which is what the confirm/select UX needs to disambiguate.

Provider priority (`gh` < `glab` < `web`) is used only as a tie-breaker in ranking, not as a call order or a "stop once one succeeds" gate. A single provider's failure (missing binary, auth error, unparseable output) is collected as a `ProviderError` and rendered as a visible warning alongside whatever hits the other three providers found — it never discards the rest of the merged result set.

**Injectable-seam pattern.** Each provider follows the same two-layer shape also used by `commands::update`'s `curl_text`/`run_curl` seam: a pure core (`gh_search_hits`, `glab_search_hits`, `github_web_search_hits`, `gitlab_web_search_hits`) that takes an injected `run` closure and does only parsing/mapping, plus a real entry point (`gh_search`, `glab_search`, `github_web_search`, `gitlab_web_search`) that wires the core to an actual `gh`/`glab`/`curl` subprocess call. The two web-API providers additionally share a `run_curl` helper local to `search.rs` (kept separate from `update::run_curl` since that one is private to its own module and returns a different error type). This keeps every parsing/ranking/dedupe rule unit-testable without spawning a subprocess or touching the network.

**Safety boundary with `ccync add`.** `search` never mutates ccync state directly. Once a hit is confirmed (via TTY select/confirm, `interactive_confirm`), the exact confirmed clone URL is handed off to the existing, unmodified `cmd_add` pipeline (`commands::plugin::cmd_add`) — the same entry point documented in `## Source Resolution (ccync add)` below. `search` is purely a discovery input into that pipeline; it introduces no second installation path, and a cancelled selection or declined confirmation is a no-op.

## ccync repair — Layering and the Metadata-Enrichment Boundary

`ccync repair [<id>]` resolves a knowable, clonable source for an adopted item and writes it into that item's own `_adoptedItems[].source` field. Its layering mirrors `ccync search`'s CLI-first split: the command surface (`crates/ccync-cli/src/commands/repair.rs`) owns the search-provider reuse, TTY handling, and write orchestration; `ccync-engine` only carries the data-layer seams (`adopt::adopted_items_without_source`, `adopt::find_adopted_item`, `adopt::write_adopted_source`) plus `CommandKind::Repair`, the dispatch tag. `repair.rs` re-uses `search.rs`'s already-audited provider/ranking functions (`pub(crate)` visibility, same crate) rather than duplicating the `gh`/`glab`/web-API adapters — the only new logic is the query-term construction (`@marketplace` stripped for plugins), repair-specific prompt copy, HTTPS validation, and the write call.

**`repair` is pre-init and non-projecting** — it never checks the Initialization Requirement and never calls `run_unified_projection`. Its only side effect, when a candidate URL is confirmed, is a single-row atomic write to `_adoptedItems[].source` via `ccync_foundation::platform::atomic_write_bytes`. `write_adopted_source` is `adopt.rs`'s *first and so far only* caller of that primitive — the module's other lock-file mutators (`remove_adopted_item` among them) still write through a plain `std::fs::write`, so `repair` raises the durability bar rather than matching an existing one.

**Metadata enrichment is not take-over.** Writing a `source` onto an adopted row changes what ccync *knows* about that item — it does not change *who manages* it. An adopted item stays report-only (`ccync upgrade` never auto-applies it) whether or not `source` is populated; `repair` only improves the evidence `resolve_adopted_upgrades` and the repair-tip seam (`adopt::adopted_items_without_source`) have to work with. The only way to convert an adopted item into a personally-managed one remains unchanged: `ccync remove <id>` followed by `ccync add <source>` (see [Source Resolution](#source-resolution-ccync-add) below) — `repair` never calls `cmd_add`, never writes `_personalPlugins`, and never promotes a row out of `_adoptedItems`.

**Search-result reuse, not a second search implementation.** `repair_one` (`repair.rs`) takes the same `SearchOutcome`/`SearchHit` types `search.rs` produces and applies its own select/confirm/validate/write pipeline on top — a `ccync repair` run and a `ccync search` run hit identical provider adapters and ranking, differing only in the query term, the prompt copy, and what happens after confirmation (a lock-file field write vs. a full `cmd_add` pipeline call).

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

## Managed View and Adopted Item Semantics

`build_managed_view(lock: &Value) -> Vec<ManagedItem>` (in `ccync-engine::managed_view`) is the single read path for commands that present both personal and adopted items together (`list`, `show`, `remove`). It merges `_personalPlugins` and `_adoptedItems` from the lockfile into a flat `Vec<ManagedItem>`:

- Personal items carry `origin: Personal` and read pin / held state from the lockfile.
- Adopted items carry `origin: Adopted` and derive their kind (plugin / mcp / skill) from the `sourceId` tag (a `skill` substring → skill, an `mcp` substring → mcp, otherwise plugin).
- On id collision the personal entry wins; the adopted entry is silently shadowed.

**Adopted item fields** stored in each `_adoptedItems` array entry:

| Field | Meaning |
| --- | --- |
| `name` | Canonical item id |
| `sourceId` | Agent-and-kind tag written by `adopt::source_id` — `claude-plugin`, `claude-mcp`, `claude-skill`, `codex-plugin`, `codex-mcp`, `codex-skill`. Classified by substring (`skill` → skill, `mcp` → mcp, otherwise plugin) |
| `origin` | Always `"adopted"` |
| `baselineVersion` | SHA or version string captured at adoption time (marketplace plugins only); absent for MCP / loose-skill entries |
| `source` | A resolvable, clonable reference for this item — from catalog resolution at adoption time, or written later by `ccync repair`. Absent when unknown. Metadata only; see [`ccync repair`](#ccync-repair--layering-and-the-metadata-enrichment-boundary) for why this is never take-over |

`sourceId` also drives `remove_adopted_item` in `ccync-engine::adopt`: when `ccync remove <id>` targets an adopted item, the engine reads `sourceId` to determine which secondary namespace (`_looseSkills` or `_mcpServers`) to also clear.

**Read-only adopted upgrade semantics.** `ccync upgrade` and `list --upgrade-available` treat adopted items as report-only — no adopted item is ever auto-upgraded. Classification splits *source* resolution from *version* evidence: a source-bearing item is never mislabeled `source-unresolved` just because catalog/baseline version data is incomplete.

| Adopted kind | Behaviour |
| --- | --- |
| Source known (catalog resolution or `source` field), catalog version + baseline both present | `resolve_adopted_upgrades` compares them; reports `current → latest` as `AdoptedUpgradeOutcome::Changed` when newer, `AdoptedUpgradeOutcome::Unchanged` otherwise |
| Source known, but catalog version and/or baseline is missing | `AdoptedUpgradeOutcome::VersionUnresolved { name, source }` — distinct from `Skipped`, since the source *is* known |
| No resolvable source at all (no marketplace, no catalog hit, no recorded `source` fallback) | `AdoptedUpgradeOutcome::Skipped` (source unresolved) |
| MCP server | `AdoptedUpgradeOutcome::Skipped` (config entry, not versioned) |
| Loose skill | `AdoptedUpgradeOutcome::Skipped` (local content, no upstream) |

Whenever any adopted row (of any kind) has no `source`, `upgrade`/`list --upgrade-available`/`init`'s adoption summary each print one lock-derived `Tip: run \`ccync repair\`` line, sourced from `adopt::adopted_items_without_source` — never a per-item hint, and never plugin-only.

To apply an upstream change to an adopted item, remove it with `ccync remove <id>` and re-add it via `ccync add`.

## Hooks Projection

Hooks follow the same canonical-root-only deployment strategy as skills, commands, and agents:

1. `render_canonical_root` (in `ccync-engine/src/install.rs`) copies the `hooks/` subtree of each managed plugin into `~/.ccync/build/render/hooks/`.
2. `apply_claude_lifecycle` (also in `ccync-engine/src/install.rs`, reached through `apply_lifecycle_artifact_chain`) writes Claude's marketplace manifest with a `"ccync"` plugin entry whose `source` field is the **absolute** canonical-root path (not a relative `"./ccync"` string), so the entry always resolves to the real `~/.ccync/build/render/` tree — there is no unmaterialized intermediate directory. The same call then writes the companion Claude target-state file (`~/.ccync/dist/targets/claude/managed.json`), and it is that file — not the marketplace manifest — that carries the lifecycle metadata: `"mode": "session-load-only"` with `sessionLoadCommand: "claude --plugin-dir <plugin-root>"`. ccync materializes the plugin root (hooks included) and records the command that would load it, but does not itself execute hooks or invoke Claude — loading only happens if and when the user (or their own tooling) runs a `claude --plugin-dir <canonical-root>` session. There is no authoritative evidence of Claude auto-loading this path on its own. The marketplace manifest itself stays a supplementary, non-consumed seam (the same state file records `"cli": {"available": false}`), useful for a manual `claude plugin install`/`marketplace add` later, but not required for the session-load path above.
3. Non-Claude agents (such as Codex, Gemini CLI, and OpenCode) do not possess a CC-plugin hook surface. Therefore, hooks are simply inapplicable for those agents, rather than missing.
4. During `ccync remove`, `render_canonical_root` clears the component directories (`skills/`, `commands/`, `agents/`, `hooks/`) and the merged `.mcp.json` before re-rendering from the remaining plugins. This ensures all artifacts of removed plugins are completely pruned. Other files in the canonical root (like lifecycle manifests) remain untouched.
5. The `crates/projection/` directory (a Protected Path) is **never modified** by hook handling.

## Managed MCP Ownership and Recovery

ccync overlays MCP server entries onto four live host files it does not own outright — `~/.claude.json`, `~/.copilot/mcp-config.json`, `~/.codex/config.toml`, and the OpenCode config — each of which also holds entries the user wrote by hand. Deleting a live entry therefore requires **proof of ownership**, not inference. This section is the architectural contract behind that proof; the user-facing consequence (entries ccync can no longer account for are never auto-deleted) is documented in [`manual.md`](manual.md#stranded-mcp-entries-from-before-per-host-ownership-tracking).

Two atomic JSON files alongside `managed.json` carry the proof:

| File | Durable meaning |
| --- | --- |
| `~/.ccync/build/mcp/projected-state.json` | Schema version, plus **per host** the set of server names ccync has *committed proof* it owns |
| `~/.ccync/build/mcp/projection.txn.json` | Write-ahead transaction: an ID, plus for each affected host/name a pre/post **fingerprint** or an `absent` sentinel |

### The invariants

- **Per-host committed ownership.** Ownership is recorded per host, never globally. One host's successful write never grants ownership on another host. ccync may delete a live MCP entry only where `projected-state.json` records committed proof for that exact host *and* name. A sequential-write crash converges on the next non-dry run even if the desired manifest changed in between.
- **The journal never holds entry content.** A fingerprint is a SHA-256 over the deterministic serialization of the host-native server entry subtree. Resolved commands, env values, headers, and tokens exist **in plaintext** in memory, in the live host file that already needed them, and — for MCP servers adopted from a master agent — in the lockfile `_mcpServers` namespace at `~/.ccync/build/lock.json`, which snapshots each adopted server's full definition (`env` and `headers` included) verbatim, and in the merged manifest `~/.ccync/build/mcp/managed.json` rendered from it — placeholder substitution happens later and only in memory, so both files hold the same pre-resolution bytes. A literal secret in the master's config therefore lands in both ccync-owned files; use a `${VAR}` placeholder in the master config to keep the value out of them. ccync writes MCP credentials to disk exactly as it received them, in each host's own native format, with no encryption at rest; they are **never** copied into the journal. Ordering differences between JSON/TOML renderings are normalized away, while value edits and unknown fields inside an affected entry remain significant. This is the top security contract of the MCP layer, and it is enforced by the type system (the journal has no field capable of holding entry content), not by reviewer discipline.
- **Recovery is three-state, and the third state fails closed.** Against a leftover transaction, each affected entry is fingerprinted and classified: equal to **post** → the write landed; equal to **pre** → it did not land, prior ownership stands; **anything else** → *ambiguous*. Ambiguity is never guessed at — ccync preserves the live entry, retains the journal, and stops with `ExitCode::Error`.
- **Unowned same-name entry is a collision, never implicit ownership.** A live entry whose name matches a managed server but which ccync cannot prove it created is a collision: preserve it, name the host and server, state plainly that nothing was deleted, and stop MCP mutation with an actionable error. The same applies to an entry the host's writer cannot address — e.g. a Codex entry under a plain inline `[mcp_servers]` key, which the line-based writer cannot target. Unaddressable is treated as unowned collision: no claim, no overlay, no removal.
- **Legacy bootstrap is exact-match only.** When no `projected-state.json` exists yet, ccync may claim a legacy host/name **only** if the previous `managed.json` manifest, parsed in memory and re-serialized through that host's own serializer, is canonically identical to the live entry. Absent or mismatched stays unowned and is never auto-deleted. This rule is precisely *why* the stranded-entry cleanup section exists in the manual: the pre-per-host model never recorded enough per-entry proof to survive losing its record.
- **Isolation is not tolerance.** A host whose path is unresolvable or whose config is unparseable is *isolated*: never written, ownership unchanged, canonical render and the remaining hosts still complete. But the command still returns `ExitCode::Error` and honestly lists which non-MCP phases did complete — a partial run never reports clean success. The one exception is a pending transaction touching that host: its entries cannot be classified, so the run fails closed and retains the journal.

Note the ordering constraint that forces this design: `run_projection_core` regenerates `build/mcp/managed.json` *before* the live MCP phase, so `managed.json` cannot serve as a previous-state checkpoint. Recovery must therefore run before the canonical render overwrites it.

### Managed MCP recovery

```text
(start) projection request
  |
[ read every live host's entries ]
  |
{ host participates? }            <- path unresolved, or config unparseable
  +-- no --> [ isolate host: never written, ownership unchanged; report it ]
  |            |
  |          { pending transaction touches it? }
  |            +-- yes --> (end ⏹ retain transaction; Error/1)
  |            |
  |            no
  v            v
  +------------+
  |
{ pending transaction? }
  +-- yes --> [ fingerprint each affected live entry ]
  |             |
  |           { actual state? }
  |             +-- post --> [ mark that host/name landed ]
  |             +-- pre  --> [ keep prior ownership ]
  |             +-- third --> (end ⏹ preserve live entry; Error/1)
  |                           |
  |             [ commit recovered per-host ownership ]
  |                           |
  +-- no ---------------------+
  v
{ projected-state exists? }
  +-- no --> [ exact-match bootstrap from prior managed.json ]
  |             +-- match --> [ claim only that host/name ]
  |             +-- other --> [ leave unowned; warn ]
  |
  yes
  v
{ unowned same-name entry, or one this host's writer cannot address? }
  +-- yes --> (end ⏹ preserve live entry; Error/1)
  |
  no
  v
[ atomically persist pre/post fingerprints ]
  |
[ atomically update participating hosts one at a time ]
  |
{ every outcome proven? }
  +-- no --> (end ⏹ retain transaction; Error/1)
  |
  yes
  v
[ commit ownership for participating hosts only; clear transaction ]
  |
{ any host isolated above? }
  +-- yes --> (end ⏹ other phases completed; Error/1)
  |
  no
  v
(end ✅) live state and ownership converge
```

### Deliberately rejected alternatives

Whole-config snapshots, content markers, cross-file rollback, and an automatic legacy-orphan detector were all considered and rejected. Each either copies unrelated user data and secrets into ccync's state, or cannot actually prove ownership — which is the only question that matters here.

## Projection Run Evidence and Status Authority

A projection run's per-target truth comes from **subject-bound evidence**, never from run-global counts. `crates/projection/` produces `ProjectionEvidence` — `common` path results, `by_target` results, `completed_targets`, and run warnings. Evidence carries **facts only** (paths touched, warnings, expected owned-path failures); it never assigns a status. The legacy `InstallReport` (which collapsed a run into lifecycle buckets and treated absence as success) is retired; the old run-global `ProjectionReport` survives only as a `pub(crate)` internal bridge inside `crates/projection/`.

The **engine-owned `ProjectionRunAccumulator`** (`ccync-engine::install`) is the sole authority for per-target status and for the run's exit code. It merges evidence from the run's two phases — the **Canonical** lifecycle apply against the canonical root, and the post-MCP **Machine** projection (`run_machine_update`) — and derives one `ProjectionTargetStatus` per selected target: `NotRun` / `Applied` / `Unchanged` / `Skipped {reason}` / `Failed {path, reason}`.

Key invariants:

- **Absence is never completion.** Every selected target gets a result; a target with no completion evidence derives `NotRun`, and `NotRun` is never reported as success.
- **Three-way phase-completion classification.** Which phase(s) must complete a target depends on which phase structurally owns it: `Claude`/`Codex`/`Copilot` are dual-phase (both phases required); `AgyCli`/`AgyIde` are canonical-only; `Opencode`/`GeminiCli`/`AgyGui` are machine-only. The two phases' completed sets are tracked independently and never merged.
- **Precedence per target:** a recorded failure wins, then a recorded skip, then completion gates `Applied`/`Unchanged`.
- **Net per-path effect decides `Applied` vs `Unchanged`.** A path whose history ends `[Written, Removed]` is a same-run rollback with no lasting effect — this is exactly what the AGY junction rollback produces (it compensates a failed apply with a paired `Removed` event) — and does not count as change evidence; a standalone `Removed`, `Written`, or `Linked` does.
- **The CLI passes evidence, never infers status.** Canonical-phase evidence is carried out of the MCP projection's render closure via a side channel and recorded into the accumulator; the CLI's only status-derived decision is the exit code — any `Failed` target, or a genuine machine-phase fault (which leaves its targets `NotRun`, invisible to a `Failed`-only check), yields `ExitCode::Error`.

The run-level CLI success line was removed with the legacy report; the replacement run-level output contract is deliberately deferred to a later, dedicated change rather than improvised here.

### Author-facing vs internal MCP keys

One boundary rule, easy to get backwards: **author-facing input** (a plugin's or bundle's own `.mcp.json`) is read via `mcpServers`, the standard CC-plugin key, with `servers` still accepted for compatibility. **ccync-generated files** (the canonical root's merged `.mcp.json` and `build/mcp/managed.json`) keep the internal `{"servers": ...}` shape. `read_author_mcp_servers` is the single author-facing reader; adding a second one re-opens the bug where standard plugin MCP entries were silently dropped. Precedence is unchanged and first-wins: the earlier core/bundle entry wins, adopted definitions fill gaps only, and a collision warning names the source that was retained.

## State Topology (`~/.ccync/`)

The full machine-state tree. `plugins/catalog.json` (the repo-relative **curated** catalog) is a source contract and is **not** under `~/.ccync/`; it is embedded directly into the `ccync` binary at compile time (`include_str!`) and resolved straight from that embedded copy — there is no disk-deploy step, and `~/.ccync/build/render/catalog.json` is never written. The machine-local **personal** catalog is the separate `~/.ccync/plugins.json` — do not conflate the two.

Three classes of data live under `~/.ccync/`:

- **INPUT** — user intent; never auto-deleted and cannot be rebuilt by `sync`.
- **CACHE** — fetched plugin bodies; rebuildable from network/source.
- **OUTPUT** — everything under `build/`; fully regenerated by `ccync sync`.

Two root-level items sit **outside** those three classes. Both behave
like OUTPUT — rebuildable, and deleted on `ccync uninstall` along with
everything except the two INPUT files — but neither lives under `build/`:

- `~/.ccync/dist/targets/<target>/managed.json` — per-target projection state
  written during `run_update` for `claude`, `copilot`, `codex`, and `agy`
  (canonical root, projection root, install target, manifest path,
  `generatedAt`). This is where Codex's only artifacts land, since Codex never
  writes into a live external agent home.
- `~/.ccync/plugins/` — the ccync-owned marketplace manifests
  (`.claude-plugin/marketplace.json` for Claude, `.agents/plugins/marketplace.json`
  for Codex). Distinct from the INPUT file `~/.ccync/plugins.json`; do not
  conflate the two.

**Pre-rename cleanup shim.** `dist/targets/` was previously `dist/providers/`,
then `dist/runtimes/`. After writing the current state, `run_update` removes
both stale roots best-effort — a failure is a run warning, never fatal — so an
upgraded install does not keep orphaned trees. This is the established
precedent for retiring a machine-state path: write the new root first, then
sweep the old one on the next projection, never a separate migration command.
`~/.ccync/ledger.json` was retired the same way: `run_update` no longer writes
it, and an existing file from an older install is swept best-effort on the
next projection.

`ccync backup` covers both INPUT files plus one OUTPUT exception — `build/lock.json`, whose init adoption snapshots are not rebuildable (see the table below); CACHE and the rest of OUTPUT are excluded as *content*-rebuildable. That does not make every excluded file consequence-free to lose by hand: `build/mcp/managed.json`, `build/mcp/projected-state.json`, and `build/mcp/projection.txn.json` each carry MCP ownership/recovery evidence whose *loss* (not their bytes) is what matters — see the per-file notes in the table below and [Managed MCP Ownership and Recovery](#managed-mcp-ownership-and-recovery).

```txt
~/.ccync/                              all ccync machine state
├── config.json          [INPUT]       machine config (secrets; devMode/ccyncRoot parsed but inactive)
├── plugins.json         [INPUT]       personal plugin catalog (written by `ccync add`)
├── dist/targets/        [unclassed]   per-target projection state (`<target>/managed.json`)
│                                      for claude / copilot / codex / agy
├── plugins/             [unclassed]   ccync-owned marketplace manifests (Claude + Codex);
│                                      NOT the INPUT file `plugins.json`
├── cache/               [CACHE]       personal plugin bodies, strictly pinned
│   └── <id>@<sha | sha256-first12>/  cloned/extracted plugin bodies
└── build/               [OUTPUT]      fully regenerated by `ccync sync`
    ├── lock.json                      resolved+pinned lockfile (_personalPlugins, adoption, _ccyncProjection, _mcpServers)
    ├── render/                        canonical root (rendered by `ccync sync`)
    │   ├── skills/  commands/  agents/  hooks/
    │   ├── .mcp.json                  merged MCP servers
    │   └── lifecycle manifests        (marketplace.json, agent-session state, …)
    ├── mcp/
    │   ├── managed.json               aggregated managed MCP manifest
    │   ├── projected-state.json       per-host committed MCP ownership (proof required to delete)
    │   └── projection.txn.json        write-ahead transaction; fingerprints only, never entry content
    └── active/<provider>/             active provider projection
            │
            └── projection ──► per-agent live surfaces (outside ~/.ccync/):
                                ~/.claude/agents, ~/.agents/skills, ~/.claude.json,
                                ~/.codex/config.toml, copilot, opencode, agy faces
```

| Path | Class | Contents |
| --- | --- | --- |
| `~/.ccync/config.json` | INPUT | Machine configuration: `secrets` (active), plus `devMode`/`ccyncRoot` — parsed and round-tripped, but inactive for production projection (see [Single-Mode Projection](#key-architecture-decisions) below; there is no Dev/Normal split in the current code path). |
| `~/.ccync/plugins.json` | INPUT | Personal plugin catalog (modified by `ccync add`). |
| `~/.ccync/dist/targets/<target>/managed.json` | *(unclassed)* | Per-target projection state for `claude` / `copilot` / `codex` / `agy`, written during `run_update`. Codex's only artifacts are this file plus its marketplace manifest — it never writes into a live external agent home. Pre-rename `dist/providers/` and `dist/runtimes/` are swept best-effort on each projection. |
| `~/.ccync/plugins/` | *(unclassed)* | ccync-owned marketplace manifests: `.claude-plugin/marketplace.json` and `.agents/plugins/marketplace.json`. A directory — not the INPUT file `~/.ccync/plugins.json`. |
| `~/.ccync/cache/<id>@<sha>/` | CACHE | Git-cloned personal plugins, strictly pinned by their commit SHA. |
| `~/.ccync/cache/<id>@<hash>/` | CACHE | Archive-sourced personal plugins, strictly pinned by their archive SHA-256 prefix. |
| `~/.ccync/build/lock.json` | OUTPUT | Resolved catalog lockfile encompassing `_personalPlugins`, adoption state, `_ccyncProjection`, and the full MCP server definition snapshot (`_mcpServers`). **Backup exception:** although OUTPUT, it is included in the `ccync backup`/`restore` set (as `plugins.lock.json`) because the init adoption snapshots (`_adoptedItems` / `_mcpServers`) cannot be regenerated from the INPUT files; everything else `sync` rebuilds is excluded. |
| `~/.ccync/build/render/` | OUTPUT | Canonical root: `skills/`, `commands/`, `agents/`, `hooks/`, `.mcp.json`, and lifecycle manifests. |
| `~/.ccync/build/mcp/managed.json` | OUTPUT | Aggregated managed MCP manifest. Regenerated by the canonical render *before* the live MCP phase, so it cannot serve as a previous-state checkpoint — see [Managed MCP Ownership and Recovery](#managed-mcp-ownership-and-recovery). |
| `~/.ccync/build/mcp/projected-state.json` | OUTPUT | Per-host committed MCP ownership: the server names ccync has proven it owns, host by host. Losing this file does not strand data, but it does permanently un-own the affected live entries (fail-closed by design). |
| `~/.ccync/build/mcp/projection.txn.json` | OUTPUT | Write-ahead transaction journal for the live MCP phase. Holds pre/post fingerprints or an `absent` sentinel — never resolved entry content (commands, env, headers, tokens). |

**Source-of-truth vs live-surface boundary:** ccync's own state (config, lockfile, cache, canonical render) lives entirely under `~/.ccync/`, and that tree is where `backup`/`restore` operate. But ccync also *projects* that state onto each selected agent's live config surface outside `~/.ccync/` — `~/.claude/skills/...`, `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and equivalents for the other supported agents. Writing to those live surfaces is the entire point of "install once, project everywhere"; it is gated by the First-Run Overwrite Visibility Gate and tracked by `ManagedArtifactRegistry` so ccync only ever touches artifacts it created.

## Key Architecture Decisions

- **Single-Mode Projection:** There is no Dev/Normal split. `run_update`'s mode string is an unconditional `"normal"` (`install.rs`); ccync always renders from its canonical root regardless of `config.json`'s `devMode`/`ccyncRoot` fields. Both fields are still parsed and preserved by the config model — they round-trip on a `sync` — but neither is read anywhere in the production projection path. Treat them as inactive compatibility fields, not a live mode switch.
- **Universal Installation:** `ccync add` handles any CC-plugin source (Git URL, local path, archive, or catalog ID) using a single command and a unified `_personalPlugins` pipeline. No source type requires a special code path.
- **Canonical-Root Hooks:** CC-plugin hooks (`hooks/hooks.json`) use the same generic component-materialization mechanism as skills/commands/agents. ccync materializes these hooks into its canonical root and records a `claude --plugin-dir <canonical-root>` session-load command; it does not execute hooks or invoke Claude itself. This approach requires zero modifications to `crates/projection/` (a Protected Path).
- **Cross-Agent via Projection:** A single installation is projected to every selected agent's native surface via the `projection` engine and the `ManagedArtifactRegistry`. The registry meticulously tracks managed artifacts to ensure pruning operations never inadvertently delete non-ccync files.
- **Independent On-Disk Identity:** ccync's own state lives entirely under `~/.ccync/`, uses its own canonical plugin IDs, and manages its own projection surfaces there — independent of any other tool's state. Projecting that state onto each agent's live config surface outside `~/.ccync/` is a deliberate, gated write (see the source-of-truth vs live-surface boundary above), not an exception to this identity.
- **Lossless `config.json` writes:** Any code that writes back `~/.ccync/config.json` must merge over the **raw JSON object**, updating only the keys it owns and preserving every other key verbatim. The typed `CcyncConfig` struct does **not** model `secrets` — `mcp::resolver` reads the `secrets` object straight from raw JSON, and ccync stores it there in plaintext, verbatim as the user supplied it, with no encryption at rest — so a typed load→modify→serialize round-trip silently drops the user's credentials, and a typed load collapses *any* deserialize error into the default struct, so **key-presence decisions** (e.g. "does this config already carry a canonical selection?") must also be made on the raw map, never the struct, or a physically present selection can look absent and be overwritten. Both authorized selection write sites — `ccync init` and the legacy `install-state.json` fold (`migrate::fold_install_state_into_config`) — go through the single foundation writer `ccync_foundation::config::write_projection_selection_at`: it writes the canonical `selectedProjectionTargets` key, removes the superseded legacy keys (`selectedRuntimes` / `primaryRuntime` / old-layout `runtimes`) in the same publish, and lands atomically (one temp+rename; an injected mid-write failure leaves the file byte-identical). Guarded by `secrets`-preservation and legacy-key-removal regression tests.
