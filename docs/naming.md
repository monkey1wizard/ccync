# ccync Naming Conventions

> **Advisory:** These conventions are not enforced by a strict gateway check (ccync does not ship a `naming-gate` command). Instead, they serve as guidelines to ensure that terminology remains consistent across the codebase, the catalog, and the documentation.

## Reserved and Canonical Terms

| Term | Meaning |
| --- | --- |
| **Plugin** | A distinct unit managed by ccync. It typically consists of a CC-plugin directory containing skills, commands, agents, and MCP server configurations. |
| **Catalog** | The `plugins/catalog.json` file. This is the curated collection of installable plugins and pre-defined profiles. |
| **Profile** | A named collection of plugins specified within the catalog (e.g., `default`, the shipped baseline profile). |
| **Canonical Root** | The `~/.ccync/build/render/` directory. This serves as ccync's primary managed plugin root. |
| **Personal Plugin** | A user-added plugin (installed via `ccync add`), which is cached under `~/.ccync/cache/`. |
| **Projection** | The automated process of writing a plugin's components onto an agent's native configuration surface. |
| **Agent** (or **Runtime**) | A target coding assistant. The canonical runtime keys are detailed in the table below—always use these exact keys. |
| **MCP Server** | A Model Context Protocol server that ccync integrates into an agent's MCP host configuration. |
| **Adopt / Reconcile** | The process of importing a master agent's existing installation state, followed by overwriting that state onto other target agents. |
| **Lockfile** | The `~/.ccync/build/lock.json` file. It records the resolved catalog, specific commit pins, the current adoption state, and the managed artifact registry (`_ccyncProjection`). |
| **`update`** | The public verb that self-updates the **ccync binary itself** (Homebrew `brew update` semantics). Do not confuse it with `upgrade`, or with the internal/dev `refresh` (derived-layer rebuild). The bare term `update` in prose should mean binary self-update. |
| **`upgrade`** | The public verb that upgrades installed **git-source plugins** to their latest upstream commit (Homebrew `brew upgrade` semantics). Reserved for plugin upgrades — never use it for the binary. |

## Canonical Runtime Keys (Never use bare `gemini` or `antigravity`)

ccync explicitly targets 8 runtimes. While several of these reside under the `~/.gemini/` directory, they represent **entirely different products**. You must use the exact runtime key provided below; never use a generic `gemini` or `antigravity` key. The runtime key serves as ccync's internal identifier—it does **not** necessarily match the on-disk directory name, as those directories are owned by their respective vendors (e.g., Antigravity or Gemini), and ccync does not rename them.

| Runtime Key | Product | On-Disk Path (Vendor-Owned) |
| --- | --- | --- |
| `claude` | Claude Code | `~/.claude/` |
| `codex` | Codex CLI | `~/.codex/` |
| `copilot` | GitHub Copilot CLI | `~/.copilot/` |
| `gemini-cli` | Google **Gemini CLI** (standalone; unrelated to Antigravity) | bare `~/.gemini/` |
| `agy-cli` | **Antigravity CLI** | `~/.gemini/antigravity-cli/` |
| `agy-ide` | **Antigravity IDE** | `~/.gemini/antigravity-ide/` |
| `agy-gui` | **Antigravity GUI** | `~/.gemini/antigravity/` |
| `opencode` | OpenCode | `~/.config/opencode/` |

> **Note:** `agy` is the established abbreviation for Antigravity. The `gemini-cli` is distinct from Antigravity; they simply share the `~/.gemini/` parent directory. Illegal bare keys (e.g. a bare `gemini` or `antigravity`) are validated against `VALID_RUNTIMES` (`claude`, `codex`, `copilot`, `gemini-cli`, `opencode`, `agy-cli`, `agy-ide`, `agy-gui`) and rejected with a warning plus fallback — they are not silently tolerated. This table acts as the definitive naming authority for the 8 canonical runtime keys.

## Identifier Rules (Rust)

Adhere to the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/naming.html) (RFC 430):

- Use `snake_case` for crates, modules, functions, and variables.
- Use `UpperCamelCase` for types.
- Use `SCREAMING_SNAKE_CASE` for constants.
If any conflict arises, these standard Rust rules take precedence.

## House Style

- **Branding:** Always refer to the product and its binary as **ccync** (strictly lowercase) in all user-facing text.
- **Catalog Plugin IDs:** Use `kebab-case`. The ID should directly match the final path segment of the plugin's upstream repository.
- **Lockfile Keys:** Internal lockfile keys utilize established `_camelCase` namespace markers (e.g., `_personalPlugins`, `_adoptedItems`, `_adoptMaster`, `_ccyncProjection`, `_mcpServers`). In accordance with the zero-alias disk contract (D-05), there are **no** `serde(alias)` fallbacks or legacy key retentions anywhere in the system. The projection registry key is strictly `_ccyncProjection`, with no backward compatibility for any legacy prefix. The `_mcpServers` namespace holds the full MCP server definitions snapshotted during adopt.

## General Notes

- ccync does **not** adopt the vocabulary of the separate workflow-orchestration product; its orchestration-style terms must not appear in ccync's naming or code.
