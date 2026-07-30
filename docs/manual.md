English | [繁體中文](i18n/zh-Hant/manual.zh-Hant.md)

# ccync Manual

This document details every `ccync` command. ccync is a cross-agent plugin, MCP, and skills manager. Its own state — everything it reads and writes for itself — lives under `~/.ccync/`. It also *projects* that state onto each agent's live configuration surface outside `~/.ccync/` (`~/.claude.json`, `~/.claude/skills/...`, `~/.codex/config.toml`, and similar per-agent paths) — see [Machine Layout](#machine-layout) below for the full picture.

Run `ccync --help` or `ccync --version` in your terminal to see the live command surface. The public CLI consists of exactly **16 commands**:
`init`, `sync`, `add`, `search`, `repair`, `remove`, `list`, `show`, `doctor`, `backup`, `restore`, `uninstall`, `update`, `upgrade`, `cleanup`, and `pin`.

## Lifecycle Commands

### `ccync init [<master>]`

Performs the initial, first-run setup. This command selects a **master agent** (either `claude` or `codex`). If the `<master>` argument is omitted on an interactive terminal, an arrow-key selection prompt appears; on a non-interactive terminal, you must supply the master explicitly. It adopts the master agent's installed plugins into ccync's central source of truth (`~/.ccync/build/lock.json`), and separately snapshots the master's **full MCP server definitions** (command, args, url, env, headers — not just names) into the lockfile's `_mcpServers` namespace, so ccync can reproject every MCP server to your other selected agents without re-reading the master's config each time. It then presents an interactive multi-select where you choose which agents to project to (defaults: `claude`, `codex`, `copilot`; `opencode`, `agy`, and `gemini-cli` are unchecked by default). After recording your agent selection it executes the unified projection engine and shows the same grouped touch-scope disclosure as the First-Run Gate below (selected agent surfaces + the four fixed MCP host files), but without a *second* confirmation prompt — the agent multi-select already served as the intent signal (see the gate's own division-of-labor note). Providing an unknown master agent results in a usage error.

Because `ccync init` writes `~/.ccync/config.json` before it ever calls the projection engine, it is never itself blocked by the Initialization Requirement described under `ccync sync` below — `init` is the command that satisfies that requirement for every other lifecycle command.

Right after the adoption summary, `init` checks whether any freshly-adopted item has no recorded source and, if so, prints the same single `Tip: run \`ccync repair\`` line described under [`ccync upgrade`](#ccync-upgrade-plugin---dry-run) — nothing is printed once every adopted row already has a source, and `init` never searches on its own.

### `ccync sync [--dry-run] [--yes]`

This is the core projection engine. It resolves the catalog (merging the built-in catalog, machine configuration, and your personal catalog) into the lockfile. It then renders the canonical root under `~/.ccync/build/render/` and projects skills, commands, and agents to every *selected* agent on a best-effort basis per surface. MCP configuration is projected separately: it always writes to the four fixed MCP host files, independent of which agents you selected — see [Machine Layout](#machine-layout). You should run this command after modifying your configuration, profiles, or installed plugins. The operation is completely idempotent.

- `--dry-run`: Prints the live agent configuration files that would be modified, without writing to them. A one-time internal layout migration under `~/.ccync/` always runs first and is not gated by `--dry-run` — migration may precede the preview.
- `--yes`: Skips the first-run overwrite-confirmation prompt. You can alternatively set the environment variable `CCYNC_ASSUME_YES=1`.

**First-Run Gate:** On a fresh machine (where no successful projection has occurred before), running `ccync sync` will show a grouped touch-scope disclosure of what it is about to modify, split into two labeled sections: the selected agent surfaces — such as `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and the OpenCode config — previewed as "would touch" (current selection only), and the four fixed MCP host files, labeled "may be written" — a host whose content is already identical to the rendered output is skipped (bytes and mtime preserved). It then pauses to ask for confirmation before continuing with live-surface projection. Declining the prompt stops the command with exit code `1`; depending on the command, earlier catalog/cache/lockfile work may already have happened, but no live agent surfaces are written. In a non-interactive terminal environment, you must pass `--yes` or set `CCYNC_ASSUME_YES=1` to proceed without this prompt.

**Division of labor across the four lifecycle commands:** `sync`, `add`, and `remove` all share this same gate as-is — `remove` needs `--yes` (or `CCYNC_ASSUME_YES=1`) to bypass it non-interactively, exactly like `sync`/`add`. `init` shows the same grouped disclosure (passing `skip_first_run_gate=true`) right after its own agent-selection UI, but skips the *confirmation prompt* — not the disclosure — because that agent-selection UI already served as the intent signal, and a second prompt would be redundant. `upgrade` runs the gate but always auto-confirms (`assume_yes=true`), so it never prompts. `add`'s live-surface render/projection happens strictly *after* the gate passes — a declined gate exits `1` and leaves any command-specific non-live writes intact, but produces zero live-surface writes. The same grouped disclosure renderer also backs `sync --dry-run` (see below), so all three callers (`init`, the generic gate, and `--dry-run`) format the identical scope the identical way.

*(Note: Master adoption via `--import-from` has been relocated to the `ccync init` command.)*

### Initialization Requirement

Before a machine has completed `ccync init`, `~/.ccync/config.json` does not exist. `ccync sync`, a projecting `ccync add` (no `--no-sync`), `ccync remove`, and a non-dry `ccync upgrade` all check for that file's existence and refuse with exit code `1` — naming `ccync init <master>` as the fix — before making any domain mutation (cache, catalog, lockfile, or live-surface writes). A handful of commands stay available even before `ccync init`:

| Command | Requires prior `ccync init`? | Pre-init exception |
| --- | --- | --- |
| `ccync init [<master>]` | No — this is the command that satisfies the requirement | always available |
| `ccync sync` | Yes | `--dry-run` stays available (read-only preview of live-surface writes; migration may still precede it) |
| `ccync add <source>` | Yes, when projecting | `--no-sync` stays available (registers without projecting) |
| `ccync remove <id>` | Yes | none |
| `ccync upgrade [<plugin>]` | Yes, when applying | `--dry-run` stays available (read-only preview that classifies plugins without cloning, pinning, or projecting — no layout migration precedes it) |
| Pre-init-available commands: `list`, `show`, `doctor`, `search`, `repair`, `backup`, `restore`, `pin`, `update` (bare or `--check`), `cleanup`, `uninstall` | No | always available |

`ccync add <source> --no-sync` still registers the plugin in `~/.ccync/plugins.json` pre-init; a later `ccync sync` (after `ccync init`) completes the projection.

`ccync cleanup` and `ccync uninstall` carry no init check at all — `cleanup` fails closed (prunes nothing) when the lockfile it reads is absent, and `uninstall` simply has nothing derived to remove. Bare `ccync update` (unlike `sync`/`add`/`remove`/`upgrade`) is unrelated to `~/.ccync/config.json` and always runs.

### `ccync uninstall`

Removes ccync-managed state and reverses ccync's projections, leaving your input files and shared user directories intact.

**Removes:**

- All derived state under `~/.ccync/` — `cache/`, `build/`, and any legacy `ledger.json` — leaving exactly the two input files (see *Preserves*).
- ccync-owned MCP server entries from live agent configs (Claude `~/.claude.json`, Copilot, OpenCode, Codex `~/.codex/config.toml`), matched by the previously projected managed names — user-owned MCP servers are kept.
- ccync's marketplace / plugin-state records: the `ccync` marketplace and `*@ccync` plugin records in Claude's `installed_plugins.json` / `known_marketplaces.json`, and (best-effort, when `codex` is on PATH) the `ccync@ccync-marketplace` Codex registration. On Claude these records only exist if you registered ccync's shipped manifest yourself (`claude plugin marketplace add ~/.ccync/plugins`) — ccync's default Claude path is session-load-only and never creates them, so on a default install there is simply nothing to remove here.
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

- `--check`: Reports the installed version, the latest available version, and the detected provenance **without** downloading or writing anything. This is a read-only status query, so it always exits `0` — including when provenance is package-manager-managed or unrecognized, in which case it prints the current version plus the same install-channel guidance (`brew upgrade ccync`, `winget upgrade …`, etc.) instead of failing. This is intentionally asymmetric with bare `ccync update` on the same provenance, which **refuses and exits `1`**: `--check` is a query that always succeeds at answering the question; bare `update` is an action that did not happen.

### `ccync upgrade [<plugin>] [--dry-run]`

Upgrades installed **git-source** plugins — cloned from a URL or from a local git working copy — to their latest upstream commit, mirroring `brew upgrade`. For each git plugin it runs `git ls-remote` to compare the remote `HEAD` against the pinned commit; when they differ it re-clones into a fresh `~/.ccync/cache/<id>@<new-sha>`, verifies the cache landed, then atomically re-pins it and runs a `sync` to reproject. Archive-source and plain-directory-snapshot plugins have no upstream "latest" and are reported as `skipped` — refresh a directory snapshot with `ccync remove <id>` then `ccync add <same-path>` again. A single plugin failing (network/clone) does not abort the others.

- No argument upgrades **all** git plugins; `<plugin>` upgrades a single plugin id (an unknown id is an actionable error).
- `--dry-run`: Prints the upgrade diff (which plugins, `<old>` → `<new>`, and skips) **without** cloning, re-pinning, or projecting. Returns exit `3` when at least one plugin is upgradable, `0` when none, `1` on error. `--dry-run` is also the pre-init exception — see [Initialization Requirement](#initialization-requirement): a non-dry `upgrade` refuses on an uninitialized machine, but `upgrade --dry-run` stays available.
- Plugins you have **held** (`ccync pin`) are reported as `held` and skipped — never queried, never re-cloned.

**Adopted items — read-only upgrade semantics:** Adopted items (imported by `ccync init`) are never auto-upgraded by ccync. `ccync upgrade` always reports their status alongside personal plugins, but never clones, re-pins, or re-projects them:

| Adopted kind | Upgrade behaviour |
|---|---|
| Source known, catalog version + baseline both present, catalog newer | `current → latest` if changed. To apply: run Claude's `/plugin` update, or `ccync add <source>` to take over management. |
| Source known, catalog version + baseline both present, not newer | Reported as `unchanged` (omitted from the printed view, same as a personal plugin already at latest). |
| Source known, but catalog version and/or baseline is missing (catalog/marketplace itself may also be unresolvable) | Reported as `version-unresolved (source: <source>)`. This is a source-bearing item — never mislabeled `source unresolved` just because upgrade-comparison evidence is incomplete. |
| No resolvable source at all (no marketplace recorded on the entry, and no recorded source fallback) | Reported as `skipped (source unresolved)`. |
| MCP server | Reported as `skipped (adopted MCP server — config entry, not versioned)`. |
| Loose skill | Reported as `skipped (adopted skill — local content, no upstream)`. |

Exit codes are governed **exclusively by personal plugin state** — adopted outcomes never raise `UpdatesAvailable (3)` or `Error (1)` on their own.

**The `Tip: run \`ccync repair\`` line.** Whenever *any* adopted row (plugin, MCP server, or loose skill) has no recorded source, the view prints exactly one line — `Tip: run \`ccync repair\` to resolve missing adopted-item sources.` — after the adopted section, regardless of how many rows are unresolved. This replaces the old plugin-only, per-item `ccync search <name>` hint: the tip is lock-derived (covers every adopted kind uniformly, not just plugins) and always names the `repair` verb, never `search`. It is static text — `repair` is never auto-run and no URL is ever auto-bound; you decide whether to run it. See [`ccync repair`](#ccync-repair-id).

### `ccync cleanup [--dry-run]`

Prunes orphaned plugin caches under `~/.ccync/cache/`, mirroring `brew cleanup`. After an `upgrade` re-clones a plugin into `<id>@<new-sha>`, the previous `<id>@<old-sha>` directory is left behind; `cleanup` removes those stale dirs. It deletes by default; `--dry-run` lists what it would remove without touching anything.

Safety is **fail-closed** — the lockfile is the sole authority for what is live:

- If the lockfile cannot be read/parsed, the live set is undetermined and **nothing is pruned**.
- A cache dir whose id is no longer in the lockfile (a removed plugin) is pruned.
- A stale-sha dir of a plugin that *does* have a resolved live pin is pruned.
- A plugin that is managed but has **no resolved pin** while owning multiple cache dirs is **ambiguous** — those dirs are **retained with a warning**, never guessed at.

## Plugin Management

### `ccync search <term> [--limit N] [--no-add]`

When you know a plugin by name (e.g. `context7`) but not its repository URL, `ccync search` looks it up on GitHub/GitLab so you can hand the resolved clone URL to `ccync add` as a new **personal** plugin. It never auto-binds a discovered URL to anything and never writes `config.json` — every add is either an explicit confirmation (interactive mode) or left entirely to you (non-interactive mode). To attach a source to an existing **adopted** item instead, use [`ccync repair`](#ccync-repair-id).

- `<term>`: the name to search for (required — an unknown/missing term is a usage error).
- `--limit N`: caps the number of hits considered/rendered. Default `10`. Accepts both `--limit N` and `--limit=N`; a non-integer or missing value is a usage error.
- `--no-add`: forces the non-interactive list-only output even on a real terminal (see below).

**Providers.** Every run queries, in priority order, the `gh` CLI (`gh search repos`) if installed, the `glab` CLI (`glab repo search`) if installed, and an unauthenticated public web/API fallback for both GitHub and GitLab. All four are called unconditionally and their results are merged — this is not strict fallback-only: a working `gh` does not skip `glab` or the web fallback. Both GitHub-facing queries (`gh` and the GitHub web/API fallback) include `fork:true` alongside your search term, so forked repositories are not excluded from results. A single provider failing (not installed, auth/config error, unreadable output) is reported as a visible warning but never discards another provider's results. Hits are deduped by normalized clone URL and ranked deterministically: exact `owner/name` match first, then stars descending, then provider priority (`gh` < `glab` < web), then lexical `full_name`.

**Interactive mode** (a real terminal, `--no-add` not passed): multiple distinct hits present an arrow-key selection listing `full_name (stars stars) - clone_url` for each candidate; a single hit still requires an explicit yes/no confirmation showing its full clone URL — it is never auto-selected. Cancelling the selection or declining the confirmation is a no-op: nothing is added. A confirmed hit is handed off to the existing `ccync add <clone_url>` pipeline using the exact confirmed clone URL.

**Non-interactive mode** (`--no-add`, or stdout is not a terminal — e.g. piped, redirected, or run in CI): renders a stable, parseable table to stdout instead of prompting, and never calls `ccync add`. The format is tab-separated values (TSV): a header line `full_name\tstars\tdescription\tclone_url`, then one row per hit (missing `description` renders as an empty field so the column count stays constant).

**Zero results:** if every queried provider succeeded but none returned a match, `ccync search` prints:

> No results found. Search manually, then run `ccync add <git-url>`.

This is not an error — it exits `0`. (If you ever need to edit ccync's on-disk state by hand, the file to look at is the documented `~/.ccync/plugins.json`; `ccync search` never mentions or touches a `config.json`.)

**Exit codes:**

| Code | Meaning |
| --- | --- |
| `0` | Hits found (interactive add ran or the list-only table was rendered), or no results with the manual-fallback message above |
| `1` | Every provider failed — nothing could be checked |
| `64` | Usage error (missing `<term>`, unknown flag, or an invalid `--limit` value) |

**Example** — an adopted item named `context7` with no recorded source:

```console
$ ccync search context7
? Multiple matches found — pick one to add ›
❯ upstash/context7 (18000 stars) - https://github.com/upstash/context7.git
  someone/context7-clone (3 stars) - https://gitlab.com/someone/context7-clone.git
✔ Multiple matches found — pick one to add · upstash/context7 (18000 stars) - https://github.com/upstash/context7.git
Add https://github.com/upstash/context7.git? (y/N) y
```

Confirming delegates to `ccync add https://github.com/upstash/context7.git`, exactly as if you had typed that command yourself.

### `ccync repair [<id>]`

Resolves a knowable, clonable source for an adopted item that has none — or explicitly overwrites an already-recorded one — via the same user-confirmed search flow `ccync search` uses. **Pre-init and non-projecting**: `repair` is always available regardless of `ccync init` state (see [Initialization Requirement](#initialization-requirement)), and it never triggers a projection pass — it writes only the single `_adoptedItems` row's `source` field.

- No argument: processes every adopted item (plugin, MCP server, or loose skill) with no recorded `source` — batch mode.
- An explicit `<id>`: targets exactly that adopted row, and may **overwrite** an already-recorded source, not just fill in a missing one.
- At most one positional argument; an unknown flag or an extra positional is a usage error.

**Search term construction.** For an adopted *plugin* (stored as `<plugin>@<marketplace>`), the `@<marketplace>` suffix is stripped before searching — you are searching for the plugin itself, not a marketplace-qualified name. Adopted MCP server and loose-skill names are searched in full, unchanged.

**Prompt copy is repair-specific.** Selection ("Multiple matches found — pick the real source") and confirmation ("Record `<url>` as the source?") never reuse `ccync search`'s "Add `<url>`?" wording — a repair is recording metadata on an existing adopted item, not adding a new personal plugin.

**Validation before writing.** A confirmed candidate URL must be a non-empty `https://` URL; anything else (an `http://` URL, a bare host/path, an empty string) is rejected before it ever reaches the write step — no write happens.

**The actual write** goes through ccync's shared atomic-write primitive (`ccync_foundation::platform::atomic_write_bytes` — a tmp-file write followed by a rename, so the lock file is never left half-written): it updates only the one uniquely-named row's `source` field, preserving every other lock namespace untouched. An unknown `<id>` is an actionable error. If more than one adopted row somehow shares the same name, the write **fails closed** — `repair` never guesses which row to update. Every no-write path (unknown id, duplicate name, decline, no candidates, non-interactive mode) leaves the lock file **byte-identical**.

**Non-interactive mode** (stdin or stdout is not a terminal — e.g. piped, redirected, or run in CI): lists the targeted id(s) only, states explicitly that no search or write occurred, and makes **zero** provider calls — `repair` never searches non-interactively, in either batch or explicit-id mode.

**Interactive mode** processes each targeted item independently; one item's provider failure, invalid URL, or write failure does not stop the remaining items from being attempted.

**Exit codes:**

| Code | Meaning |
| --- | --- |
| `0` | No targeted items (nothing to repair), non-interactive listing, a decline, no search results, or every attempted item succeeded |
| `1` | Any attempted item hit a provider-total failure, an invalid URL, a write failure, or an unknown `<id>` |
| `64` | Usage error (unknown flag or more than one positional) |

**Example** — repairing one adopted MCP server with no recorded source:

```console
$ ccync repair codebase-memory-mcp
? Multiple matches found — pick the real source ›
❯ DeusData/codebase-memory-mcp (42 stars) - https://github.com/DeusData/codebase-memory-mcp.git
  someone/codebase-memory-mcp-fork (3 stars) - https://github.com/someone/codebase-memory-mcp-fork.git
✔ Multiple matches found — pick the real source · DeusData/codebase-memory-mcp
Record https://github.com/DeusData/codebase-memory-mcp.git as the source? (y/N) y
ccync repair: recorded source for codebase-memory-mcp
```

### `ccync add <source> [--no-sync]`

Adds a personal plugin from **any supported source**, registers it within `~/.ccync/plugins.json`, pins it into the lockfile, and automatically runs a `sync` to project it to every selected agent. Source detection works automatically:

| Source Type | Example | Cache Key |
| --- | --- | --- |
| Git URL | `https://github.com/owner/plugin.git` | `<id>@<commit-sha>` |
| Local Path — git working copy | `/path/to/plugin` (contains `.git`) | `<id>@<commit-sha>` |
| Local Path — plain directory | `/path/to/plugin` (no `.git`) | `<id>@<sha256-first12>` |
| Archive (`.zip` / `.tar.gz`) | `plugin-v1.tar.gz` | `<id>@<sha256-first12>` |
| Catalog ID | `my-catalog-plugin` (a bare identifier) | Resolved via the embedded catalog |

The plugin ID is derived from the source's final path segment (with `.git` stripped off). Catalog IDs are used exactly as provided.

**How the source type is decided (precedence).** Detection is automatic and ordered — the first rule that matches wins:

1. Ends with `.zip`, `.tar.gz`, or `.tgz` → **Archive**.
2. Starts with `http://`, `https://`, `git@`, or `git://` → **Git** (any git host — GitHub, GitLab, Bitbucket, self-hosted, `git@` SSH; **not** GitHub-only).
3. Contains a `/` or `\`, or starts with `.` or `~`, or is an absolute path → **Local Path**. A directory containing a `.git` file or subdirectory (a normal git working copy or a linked worktree) is fetched like a Git source; a directory with no `.git` is fetched as an **immutable content snapshot** (see below).
4. Otherwise (a bare identifier with no separators) → **Catalog ID**.

> **Gotcha:** a bare word with no slash or dot — e.g. `my-plugin` — is treated as a **Catalog ID**, *not* a relative path. To add a local folder named `my-plugin`, write `./my-plugin` so it is detected as a path.

Archives are extracted to `~/.ccync/cache/<id>@<sha256-first12>/`, strictly preserving the full CC-plugin directory layout. If the archive contains a single, GitHub-style root wrapper directory, it is automatically promoted. Re-running `add` with the same archive bytes (same SHA-256 prefix) hits an already-populated cache directory and returns immediately without re-extracting.

**Plain local directories are immutable snapshots.** A local directory that is not a git working copy (no `.git` file or subdirectory) is not cloned — it is copied byte-for-byte into `~/.ccync/cache/<id>@<sha256-first12>/`, hashed over every accepted file's normalized relative path and bytes (deterministic across Windows and Unix). Re-adding the same directory contents hits the same cache key and returns immediately (already present). A plain directory has no upstream "latest" concept, so `ccync upgrade` always reports it as `skipped`; refresh its contents with `ccync remove <id>` followed by `ccync add <same-path>` again. Symlinks, junctions, and other reparse/link entries anywhere in the directory are **rejected** — the `add` fails with an error instead of copying them; this is deliberate (the snapshot must never follow a link out of the selected tree), and the workaround is to dereference the link (copy the linked content into the tree as real files) or use a real git working copy instead.

**Re-adding an already-added plugin ID is an error, not a no-op.** If the derived plugin ID is already present in `~/.ccync/plugins.json`, `add` refuses with a usage error (exit `64`) and a hint to `ccync remove <id>` first: it does not silently succeed. This is distinct from the cache-level dedup above — that dedup governs re-fetching the *same source content*, not re-registering the *same plugin ID*.

**Hooks:** If a plugin includes a `hooks/hooks.json` file, its hooks are materialized into the canonical root alongside skills/commands/agents. ccync does not execute them or register them with Claude — Claude can load them via a session-scoped `claude --plugin-dir <canonical-root>` invocation. For non-Claude agents, hooks are simply inapplicable. See the [`architecture.md`](architecture.md#hooks-projection) documentation for deeper mechanics.

- `--no-sync`: Registers the plugin without immediately projecting it. You must run `ccync sync` later to complete the projection. This is also the pre-init exception — see [Initialization Requirement](#initialization-requirement): a projecting `add` (no `--no-sync`) refuses on an uninitialized machine, but `add <source> --no-sync` still registers the plugin.
- `--yes`: Skips the first-run overwrite-confirmation prompt.

### `ccync remove <id>`

Removes a managed item from ccync's view and prunes its projected artifacts (skills, commands, agents, hooks, and MCP entries), then automatically performs a `sync`. `remove` has no pre-init exception — see [Initialization Requirement](#initialization-requirement).

- **Personal plugin**: removed from `plugins.json`, re-resolved, all projected artifacts pruned.
- **Adopted item** (plugin, MCP server, or loose skill imported by `ccync init`): removed from the lockfile (`_adoptedItems`, `_looseSkills`, or `_mcpServers`), all projected artifacts pruned. The source agent's live config entry (e.g. Claude's `/plugins`) is **not** deleted — only ccync's local adoption record is cleared. To also remove the item from the source agent, use that agent's own remove command first.

### `ccync list [--upgrade-available]`

Lists all managed items — both personal plugins and adopted items imported by `ccync init` — displaying their origin (`personal` / `adopted`), kind (`plugin` / `mcp` / `skill`), pinned commit (personal plugins only), and source.

When the managed view is empty, the personal-only empty-state message is shown and `ccync add` is suggested.

- `--upgrade-available`: instead of the plain list, runs a **read-only** outdated check. For each personal git plugin it compares the remote `HEAD` against the pinned commit and prints `id  current → latest  source`. Adopted items share the exact same view `ccync upgrade` renders — `changed`/`unchanged`/`version-unresolved`/`skipped` per the same rules, plus the single `Tip: run \`ccync repair\`` line when any adopted row lacks a source (see [`ccync upgrade`](#ccync-upgrade-plugin---dry-run) for the full adopted-item table). Exit code is governed exclusively by personal plugin state: `0` all current, `3` (`UpdatesAvailable`) at least one personal plugin can be upgraded, `1` on error. Held plugins (see `ccync pin`) are reported as `held` and never queried.

### `ccync show <id>`

Prints the full detail of one managed item. An unknown id is an actionable error. Mirrors `brew info` / `winget show`.

- **Personal plugin**: display name, source, install strategy, pinned commit sha, whether it is **held**, and a best-effort component count (skills / commands / agents / hooks).
- **Adopted item**: origin (`adopted`), kind (`plugin` / `mcp` / `skill`), source (when resolved), recorded baseline version (when available), and upgradeability — `up to date`, `update available: <current> → <latest>` with activation instructions, `version-unresolved` (source known, but version-comparison evidence is incomplete), or `not upgradeable: <reason>` (no resolvable source, or a non-versioned MCP/skill).
- **Unresolved source**: when the item — plugin, MCP server, or loose skill — has no recorded source at all, an indented `Tip: run \`ccync repair <id>\`` line follows the upgradeability line, naming this specific item. It never appears once a source is recorded, and nothing is searched or bound automatically.

### `ccync pin <id>` · `ccync pin --remove <id>` · `ccync pin --list`

Prevents a plugin from being upgraded (winget-style pin). `pin <id>` freezes the plugin at its current sha, `pin --remove <id>` releases it, and `pin --list` shows which plugins are currently held. A held plugin is skipped by `ccync upgrade` and `ccync list --upgrade-available` (reported as `held`, never queried).

> **`pin` the verb vs the `pin` field — different things.** The `pin` / `pinnedSha` fields in `plugins.json` / the lockfile record the *locked commit sha* (which version is installed). The `ccync pin` **verb** records a *hold against upgrade*, stored in a separate `held` field. Holding a plugin does not change its sha; it only stops `upgrade` from moving it.

## Diagnostics

### `ccync doctor [--dry-run] [--release-gate]`

Executes a read-only management health check. It validates the canonical root, master-adopted state, MCP projection integrity, Claude plugin cache freshness, and shared skills projection. This command never modifies state, so `--dry-run` and `--release-gate` are accepted but currently add no additional checks and change no behavior — doctor does not compare the render against the lockfile.

## Troubleshooting

### Stranded MCP entries from before per-host ownership tracking

ccync's MCP projection uses per-host, per-server ownership records (`~/.ccync/build/mcp/projected-state.json`, backed by a write-ahead transaction journal) so it can prove, host by host, which live MCP entries it created before removing anything. Earlier versions tracked ownership only in a single global `managed.json` manifest. If that ownership record was lost or predates this per-host model — for example because it was rebuilt, wiped, or belongs to an install from before this ownership tracking landed — ccync cannot safely reconstruct which live MCP entries were its own: the old model never recorded enough per-entry proof to tell a ccync-created entry apart from one you added yourself once the record is gone. Guessing would risk deleting your own configuration, so ccync's fail-closed rule applies here exactly as it does for any other unowned entry: the entry is left alone.

This means an MCP server entry that was projected by a ccync build before this ownership model landed, and that ccync can no longer account for, will not be automatically detected or deleted — not by `ccync sync`, `ccync remove`, `ccync uninstall`, or any other command, now or in the future. This is a deliberate, permanent limitation, not a gap a later release will close. If you have entries like this, clean them up manually, once, using the steps below.

**How to check each host.** Compare `ccync list` (what ccync currently manages) against the live entries actually present in each host file. A named MCP server entry that ccync does not currently manage is a candidate for manual review.

| Host | Live config file | Where servers live |
| --- | --- | --- |
| Claude | `~/.claude.json` | top-level `mcpServers` JSON object |
| Copilot | `~/.copilot/mcp-config.json` | top-level `mcpServers` JSON object |
| OpenCode | `<config dir>/opencode/opencode.json` (`~/.config/opencode/opencode.json` on Linux/macOS, `%APPDATA%\opencode\opencode.json` on Windows) | top-level `mcp` JSON object |
| Codex | `~/.codex/config.toml` | `[mcp_servers.<name>]` TOML sections |

**How to remove only the stranded entry.** Open the host's live config file in a text editor and delete just the one named server entry you have identified as stranded — not the whole file, and not any other entry:

- Claude / Copilot (JSON, `mcpServers`): remove the one `"<server-name>": { ... }` key/value pair from the `mcpServers` object, keeping every other key intact.
- OpenCode (JSON, `mcp`): remove the one `"<server-name>": { ... }` key/value pair from the `mcp` object, keeping every other key intact.
- Codex (TOML): delete the `[mcp_servers.<server-name>]` section (and its nested `[mcp_servers.<server-name>.env]` section, if present), keeping every other `[mcp_servers.*]` section intact.

Save the file with the same structure and encoding it had before your edit.

**When to rerun `ccync sync`.** After manually removing a stranded entry, run `ccync sync` (or let the next lifecycle command project). This does not "clean up" the entry you just removed — you already did that — but it re-establishes clean per-host ownership tracking going forward for the servers ccync does manage, so a future `remove` or `uninstall` on those servers can complete automatically, the way this section describes it should.

## Configuration (`config.json`)

ccync's machine configuration lives at **`~/.ccync/config.json`**. It is plain JSON that you edit by hand; ccync owns no `config set` command. Changes take effect on the next `ccync sync` (or any command that resolves state). A missing file or missing key falls back to explicit defaults — ccync never panics on an absent or malformed config.

**How to edit safely:**

1. Open `~/.ccync/config.json` in any text editor (create it if absent).
2. Keep it valid JSON — a parse error makes ccync silently fall back to defaults, so validate before saving.
3. Run `ccync sync` to apply. Run `ccync doctor` to confirm the config is read as expected.

**Example:**

```json
{
  "secrets": {
    "GITHUB_TOKEN": "ghp_xxx"
  }
}
```

**Keys:**

| Key | Type | Meaning |
| --- | --- | --- |
| `devMode`, `ccyncRoot` | bool, string | **Inactive compatibility fields.** ccync's config model still parses and round-trips them, but production projection has a single mode: it always renders from its canonical root — there is no Dev/Normal split and no `devMode`/`ccyncRoot` resolution in the current code path. Setting these has no observable effect; do not rely on them to change behavior. |
| `secrets` | object | Named credentials (e.g. API keys, tokens). Referenced from MCP server definitions via `${VAR}` placeholders so a value is stored once here rather than duplicated per plugin. **The placeholder keeps the literal value out of the ccync-owned manifest files** (`build/lock.json#_mcpServers`, `build/mcp/managed.json`); resolution happens at projection time, and the resolved plaintext value is what gets written into each of the four live MCP host files, by design — see the carrier callout below. |
| `selectedProjectionTargets` | array of strings | The agents plugins are projected to, in the order chosen at `ccync init`. Entries must be canonical target keys (see [`docs/naming.md`](naming.md)). Written by `ccync init` (and the one-shot legacy migration below); normally you re-run `ccync init` rather than editing it by hand. |

**MCP `${VAR}` substitution:** any `${NAME}` placeholder in a managed MCP server entry is resolved at projection time from the process environment, then overlaid with `config.json#secrets` — on a name collision, `config.json#secrets` wins. This lets one config drive every agent's MCP host without duplicating credentials per agent.

> **Where resolved MCP credentials land.** With no encryption at rest, ccync writes MCP credentials to disk in plaintext in four places: (1) each of the four live MCP host files (`~/.claude.json`, `~/.copilot/mcp-config.json`, `~/.codex/config.toml`, the OpenCode config) — they need the resolved value to authenticate; (2) `~/.ccync/build/lock.json` (the `_mcpServers` namespace) and (3) `~/.ccync/build/mcp/managed.json`, both of which snapshot each *adopted* MCP server's **full definition verbatim** — `env` and `headers` included — at `ccync init` time, before any `${VAR}` substitution happens (substitution happens later and only in memory, so both files hold the master agent's pre-resolution bytes); and (4) `config.json#secrets` itself, which you populate directly. **A literal token in the master agent's config is therefore stored in both (2) and (3) in plaintext regardless of whether you also use a `${VAR}` placeholder elsewhere** — put the placeholder in the master config itself (and the real value in `config.json#secrets` or the environment) to keep the literal token out of them. If you are scrubbing credentials off a machine, or handing a backup directory to someone else: `config.json` and `build/lock.json` are in the backup set (see [Backup & Restore](#ccync-backup---output-dir)); `build/mcp/managed.json` and the four live host files are **not** backed up by `ccync backup` and must be handled separately. Architectural contract: [`architecture.md`](architecture.md#managed-mcp-ownership-and-recovery).

> Note: `devMode`/`ccyncRoot` are inactive compatibility fields (see the Keys table above) — they are not authoritative for mode. The legacy `installMode` field is deprecated — if present it only feeds a `ccync doctor` migration warning and does not control mode.

### Legacy selection keys and the one-shot migration

`selectedProjectionTargets` is the canonical selection key. Older installs may instead carry the legacy `selectedRuntimes` (in `config.json` or the removed `install-state.json`) or the pre-flatten `runtimes` spelling in `config.json`. Two rules govern how these interact:

- **Legacy keys keep working indefinitely.** The read path resolves a legacy key whenever the canonical key is absent, and nothing forces you to re-run `ccync init`. A machine that never re-inits simply keeps its legacy key.
- **Only two write sites ever touch the selection, and both write the canonical key.** `ccync init` and the one-shot old-layout migration (run automatically at startup) write `selectedProjectionTargets` atomically over the raw JSON file — every other key, including `secrets`, survives verbatim — and remove the superseded legacy keys in the same write.

The migration folds a legacy selection into `config.json` only when no canonical key exists yet, with this precedence: existing canonical `selectedProjectionTargets` first, then `config.json`'s own `selectedRuntimes`, then its old-layout `runtimes`, then `install-state.json`'s `selectedRuntimes`. Recovery behavior:

- Entries that are not valid target keys are skipped with a named warning on stderr, and the fold source is retained on disk — `config.json` is copied to `config.json.bak`, or the legacy file is kept as `install-state.json.bak` — so the original selection stays recoverable. A fully valid fold needs no backup: `install-state.json` is simply removed once its value is safely in `config.json`.
- A malformed `install-state.json` or `config.json` aborts the fold non-destructively: nothing is modified and the unreadable file is left in place.

## Machine Layout

Everything ccync owns on your machine lives under `~/.ccync/`, split into three classes:

- **INPUT** — the two files you (or `ccync add`/`ccync init`) write directly; never auto-deleted, never rebuilt by `sync`.
- **CACHE** — fetched plugin bodies, keyed by pin so a re-fetch of the same content is a no-op; rebuildable from network/source.
- **OUTPUT** — everything under `build/`; fully regenerated by `ccync sync` (or `ccync refresh`). Regenerable does not mean every file is *equally* safe to delete by hand — see the per-file notes in the tree below before removing anything under `build/` directly (as opposed to going through `ccync uninstall`).

Two root-level items sit outside those classes — `dist/targets/` and the `plugins/` directory. They are both derived (rebuilt by the next `ccync sync`, removed by `ccync uninstall`), they are simply not under `build/`. You never edit them by hand and they are not in the backup set. Details: [`architecture.md`](architecture.md#state-topology-ccync).

The public plugin catalog is **embedded in the `ccync` binary itself** — there is no catalog file on disk under `~/.ccync/`. Resolution reads the embedded copy directly; nothing is downloaded or deployed to disk for it.

```txt
~/.ccync/                              all ccync machine state
├── config.json          [INPUT]       machine config (devMode/ccyncRoot, secrets)
├── plugins.json         [INPUT]       personal plugin catalog (written by `ccync add`)
├── dist/targets/        [derived]     per-target projection state (claude/copilot/codex/agy)
├── plugins/             [derived]     ccync-owned marketplace manifests (a directory —
│                                      not the INPUT file `plugins.json` above)
├── cache/               [CACHE]       personal plugin bodies, strictly pinned
│   └── <id>@<sha | sha256-first12>/  cloned/extracted plugin bodies
└── build/               [OUTPUT]      fully regenerated by `ccync sync`
    ├── lock.json                      resolved lockfile (_personalPlugins, _mcpServers, _adoptMaster, _ccyncProjection); in the backup set — carries non-rebuildable adoption snapshots (deleting it loses which items were adopted at `ccync init`, not just cache)
    ├── render/                        canonical root: skills/, commands/, agents/, hooks/, .mcp.json — deleting is always safe, next `ccync sync` fully rebuilds it
    ├── mcp/managed.json               aggregated managed MCP manifest — also the legacy-bootstrap baseline for MCP ownership recovery; deleting it forces any pre-per-host-tracking entries into the stranded-entry case below
    ├── mcp/projected-state.json       per-host committed MCP ownership proof — deleting it does not remove live MCP entries, but ccync can no longer prove it owns them, so a future `remove`/`uninstall` may leave them behind as stranded (see Troubleshooting)
    ├── mcp/projection.txn.json        write-ahead MCP transaction journal — present only while a projection is mid-flight or recovering from an interrupted one; deleting it while present risks the same ambiguous-recovery fail-closed state a crash would leave, so leave it alone unless you have already confirmed no projection is in progress
    └── active/<provider>/             per-agent projection bookkeeping — deleting is always safe, next `ccync sync` fully rebuilds it
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

Two of the three files can carry plaintext credentials: `config.json` in its `secrets` field (see above), and `plugins.lock.json` in its `_mcpServers` namespace, which holds each adopted MCP server's definition verbatim — including any literal token the master agent's config contained (see [Where adopted MCP credentials land](#configuration-configjson)). `ccync backup` names whichever of the two it actually copied in a warning that the backup directory may contain secrets and should be stored securely — store it with the same care you would give the original files.

### `ccync restore --from <dir>`

Restores the same three files from a previously created backup directory to their live locations (`plugins.lock.json` goes back to `build/lock.json`). Files absent from the backup — including `plugins.json` in backups taken before it joined the set — are skipped with a note. Existing files at the destination are overwritten. Run `ccync sync` afterwards to rebuild the derived state and re-project onto your agents.

## Exit Codes

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Operational error, including a declined first-run gate or a partial projection that could not complete |
| `3` | Updates available — a read-only outdated check found at least one pending upgrade (`ccync list --upgrade-available`, `ccync upgrade --dry-run`). Distinct from `1` so scripts can tell "updates available" from a real failure. |
| `64` | Usage error (e.g., an unknown or missing command, or an invalid flag) |
