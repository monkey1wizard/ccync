# ccync Naming Conventions

> **Enforcement:** ccync ships no `naming-gate` command, but the Glossary below is **not** advisory. A workspace test — `crates/ccync-engine/tests/projection_vocabulary.rs`, run by `cargo test --workspace` — rejects any new use of the retired projection-domain term across `crates/`, `docs/`, and the root `README.md`, against a fixed allowlist of reviewed exceptions. See [`devguide.md`](devguide.md#projection-vocabulary-guard) for the scan scope and the protocol for adding an allowlist entry. Everything on this page outside the Glossary remains a convention: it keeps terminology consistent across the codebase, the catalog, and the documentation, and is not mechanically checked.

## Glossary

One normative glossary for the projection domain. Every other section on this page, and every doc that describes projection behavior, uses these six terms exactly as defined here — no synonyms and no legacy alias for `Agent`.

| Term | Meaning |
| --- | --- |
| **Agent** | The user-facing coding-assistant product a person selects (`claude`, `codex`, `copilot`, `opencode`, `gemini-cli`, `agy`). Five agents map to exactly one projection target; `agy` expands to all three Antigravity faces. |
| **ProjectionTarget** | An independently addressable projection destination. There are eight canonical projection targets — see the table below. |
| **ProjectionEngine** | Coordinates one projection run: resolves the catalog, renders the canonical root, and dispatches every selected `TargetProjector`. |
| **TargetProjector** | The projector that implements projection for one target family — skills, commands, agents, and MCP config onto that target's native surface. |
| **ProjectionPath** | A concrete file, directory, symlink, or config path that a projection run may create, update, or remove. The set of paths can change across targets and ccync versions; nothing here is guaranteed stable. |
| **MCP Host** | A separately governed MCP host configuration destination — one of the four fixed MCP hosts ccync writes to, outside the canonical root. |

## Other Reserved Terms

| Term | Meaning |
| --- | --- |
| **Plugin** | A distinct unit managed by ccync. It typically consists of a CC-plugin directory containing skills, commands, agents, and MCP server configurations. |
| **Catalog** | The `plugins/catalog.json` file — a repo/build-time source embedded into the `ccync` binary at build, not a live file on disk. This is the curated collection of installable plugins and pre-defined profiles. |
| **Profile** | A named collection of plugins specified within the catalog (e.g., `default`, the shipped baseline profile). |
| **Canonical Root** | The `~/.ccync/build/render/` directory. This serves as ccync's primary managed plugin root. |
| **Personal Plugin** | A user-added plugin (installed via `ccync add`), which is cached under `~/.ccync/cache/`. |
| **Projection** | The automated process of writing a plugin's components onto an agent's native configuration surface. |
| **MCP Server** | A Model Context Protocol server that ccync integrates into an agent's MCP host configuration. Distinct from **MCP Host** above — a server is what gets configured, a host is where the configuration lives. |
| **Adopt / Reconcile** | The process of importing a master agent's existing installation state, followed by overwriting that state onto other target agents. |
| **Lockfile** | The `~/.ccync/build/lock.json` file. It records the resolved catalog, specific commit pins, the current adoption state, and the managed artifact registry (`_ccyncProjection`). |
| **`update`** | The public verb that self-updates the **ccync binary itself** (Homebrew `brew update` semantics). Do not confuse it with `upgrade`, or with the internal/dev `refresh` (derived-layer rebuild). The bare term `update` in prose should mean binary self-update. |
| **`upgrade`** | The public verb that upgrades installed **git-source plugins** to their latest upstream commit (Homebrew `brew upgrade` semantics). Reserved for plugin upgrades — never use it for the binary. |
| **`search`** | The public verb that discovers **public-forge repositories** (GitHub/GitLab) by name, via CLI-first (`gh`/`glab`) with API/web fallback. It never means: searching the (currently empty) built-in catalog, generic web/search-engine queries, GitHub code search, or npm/Bitbucket/package-manager indexes — those are explicitly out of scope and must not be folded into this verb later. |

## Canonical Target Keys (Never use bare `gemini` or `antigravity`)

ccync explicitly targets 8 projection targets. While several of these reside under the `~/.gemini/` directory, they represent **entirely different products**. You must use the exact target key provided below; never use a generic `gemini` or `antigravity` key. The target key serves as ccync's internal identifier (`ProjectionTargetId`) — it does **not** necessarily match the on-disk directory name, as those directories are owned by their respective vendors (e.g., Antigravity or Gemini), and ccync does not rename them.

| Target Key | Product | On-Disk Path (Vendor-Owned) |
| --- | --- | --- |
| `claude` | Claude Code | `~/.claude/` |
| `codex` | Codex CLI | `~/.codex/` |
| `copilot` | GitHub Copilot CLI | `~/.copilot/` |
| `gemini-cli` | Google **Gemini CLI** (standalone; unrelated to Antigravity) | bare `~/.gemini/` |
| `agy-cli` | **Antigravity CLI** | `~/.gemini/antigravity-cli/` |
| `agy-ide` | **Antigravity IDE** | `~/.gemini/antigravity-ide/` |
| `agy-gui` | **Antigravity GUI** | `~/.gemini/antigravity/` |
| `opencode` | OpenCode | `~/.config/opencode/` |

> **Note:** `agy` is the established abbreviation for Antigravity — as an **Agent**, it expands to the three `agy-*` **ProjectionTarget**s above. The `gemini-cli` target is distinct from Antigravity; they simply share the `~/.gemini/` parent directory. Illegal bare keys (e.g. a bare `gemini` or `antigravity`) are validated by `ProjectionTargetId::from_str` and rejected with a warning plus fallback — they are not silently tolerated. This table acts as the definitive naming authority for the 8 canonical target keys.

## Identifier Rules (Rust)

Adhere to the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/naming.html) (RFC 430):

- Use `snake_case` for crates, modules, functions, and variables.
- Use `UpperCamelCase` for types.
- Use `SCREAMING_SNAKE_CASE` for constants.
If any conflict arises, these standard Rust rules take precedence.

## House Style

- **Branding:** Always refer to the product and its binary as **ccync** (strictly lowercase) in all user-facing text.
- **Catalog Plugin IDs:** Use `kebab-case`. The ID should directly match the final path segment of the plugin's upstream repository.
- **Lockfile Keys:** Internal lockfile keys utilize established `_camelCase` namespace markers (e.g., `_personalPlugins`, `_adoptedItems`, `_adoptMaster`, `_ccyncProjection`, `_mcpServers`). In accordance with the zero-alias disk contract (D-05), **canonical writes and projection identity** carry no `serde(alias)` fallback or legacy key retention within the lockfile: the projection registry key is strictly `_ccyncProjection`, with no backward compatibility for any legacy prefix. This is narrower than "no aliases anywhere in the system" — outside the lockfile, ccync does carry a small number of read-only, input-only backward-compatible aliases that never become a canonical write target: `config.json`'s canonical `selectedProjectionTargets` selection folds a legacy `selectedRuntimes` (or older `runtimes`) key on read, in both `config.json` and the retired `install-state.json`, without ever writing those legacy spellings back. The `_mcpServers` namespace holds the full MCP server definitions snapshotted during adopt.

## General Notes

- ccync does **not** adopt the vocabulary of the separate workflow-orchestration product; its orchestration-style terms must not appear in ccync's naming or code.
