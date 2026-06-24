# ccync naming conventions

> **Advisory**, not an enforced gate. (ccync ships no `naming-gate` command.) These conventions
> keep terms consistent across code, catalog, and docs.

## Reserved / canonical terms

| Term | Meaning |
| --- | --- |
| **plugin** | A unit ccync manages: a CC-plugin directory (skills / commands / agents / MCP). |
| **catalog** | `plugins/catalog.json` — the curated set of installable plugins + profiles. |
| **profile** | A named set of plugins in the catalog (e.g. `dart`, `web-dev`, `full`, `default`). |
| **canonical root** | `~/.ccync/plugins/ccync/` — ccync's managed plugin root. |
| **personal plugin** | A user-added plugin (`ccync add`), cached under `~/.ccync/local/cache/`. |
| **projection** | Writing a plugin's components onto an agent's native surface. |
| **agent** (a.k.a. runtime) | A target coding agent. The canonical runtime keys are in the table below — always use those exact keys. |
| **MCP server** | A Model Context Protocol server ccync projects into an agent's MCP host config. |
| **adopt / reconcile** | Import a master agent's install state, then master-overwrite onto others. |
| **lockfile** | `~/.ccync/state/plugins.lock.json` — resolved catalog + pins + adopt state. |

## Runtime keys (canonical — never use bare `gemini` / `antigravity`)

ccync targets 8 runtimes. Several live under `~/.gemini/` but are **different products** — use the
exact key, never a bare `gemini` or `antigravity`. The key is ccync's identifier; it is **not** the
on-disk directory name (Antigravity/Gemini own those dirs, ccync does not rename them).

| runtime key | product | on-disk path (vendor-owned) |
| --- | --- | --- |
| `claude` | Claude Code | `~/.claude/` |
| `codex` | Codex CLI | `~/.codex/` |
| `copilot` | GitHub Copilot CLI | `~/.copilot/` |
| `gemini-cli` | Google **Gemini CLI** (standalone; unrelated to Antigravity) | bare `~/.gemini/` |
| `agy-cli` | **Antigravity CLI** | `~/.gemini/antigravity-cli/` |
| `agy-ide` | **Antigravity IDE** | `~/.gemini/antigravity-ide/` |
| `agy-gui` | **Antigravity GUI** | `~/.gemini/antigravity/` |
| `opencode` | opencode | `~/.config/opencode/` |

`agy` is the established short code for Antigravity. `gemini-cli` ≠ Antigravity: they merely share the
`~/.gemini/` parent. (These keys are adopted in code as of the sync-engine refactor; this table is the
naming authority.)

## Identifier rules (Rust)

Follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/naming.html) (RFC 430):
`snake_case` crates/modules/functions/variables, `UpperCamelCase` types, `SCREAMING_SNAKE_CASE`
constants. These win on any conflict.

## House style

- Brand the product and binary as **ccync** (lowercase) in all user-facing strings.
- Catalog plugin ids are kebab-case and match their upstream repo's last path segment.
- Lockfile internal keys use the established `_camelCase` namespace markers (`_personalPlugins`,
  `_adoptedItems`, `_adoptMaster`, `_ccyncProjection`). Per the zero-alias disk contract (D-05),
  there is **no** `serde(alias)` / legacy-key retention anywhere — the projection registry key is
  `_ccyncProjection` with no `_galProjection` back-compat.

## Notes

- ccync does **not** carry the `gal` workflow vocabulary (no pipeline / golem / planning /
  finalize terms). Those belong to the separate `gal` product.
