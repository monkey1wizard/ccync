---
type: reference
title: ccync naming and terminology
description: Authoritative definitions for agents, projection targets, MCP hosts, plugin lifecycle, and target keys.
tags: [naming, vocabulary, targets]
status: stable
locale: en
translation_key: naming
---

# ccync naming and terminology

## Core terms

The following terms establish the authoritative vocabulary across ccync code and documentation.

| Term | Definition |
| --- | --- |
| Agent | A coding assistant product configured by the user. The five supported agents are `claude`, `codex`, `copilot`, `opencode`, and `agy`. |
| ProjectionTarget | An independently projected target destination with dedicated execution status. There are seven targets. |
| ProjectionEngine | The engine component that resolves plugin sources, builds the canonical render, and coordinates target-specific projectors. |
| ProjectionPath | A concrete filesystem path that a projection run can create, modify, or remove. |
| MCP Host | One of five fixed host products that receive MCP server configurations, distinct from individual MCP servers configured within that host. |
| Personal Plugin | A plugin registered through `ccync add`, cached locally, and managed by ccync. |
| Adopted Item | A pre-existing component imported from the master agent during initialization. ccync preserves its source snapshot but does not apply automated upgrades. |
| Roster | The project configuration file `ccync.roster.json` tracking personal plugins shared across a team. Managed via `ccync roster dump` and `install`; distinct from catalog bundles. |

## Seven projection target keys

The seven canonical projection target keys are `claude`, `codex`, `copilot`, `opencode`, `agy-cli`, `agy-ide`, and `agy-gui`. Users configure the high-level agent code `agy`, which projection automatically expands into the three Antigravity targets. Never use bare `gemini` or `antigravity` as a target key. Standalone Google Gemini CLI support has been retired, and configuration located under `~/.gemini/` now belongs to Antigravity.

## Reserved verbs and identifier rules

These verbs carry distinct meanings across CLI interfaces and documentation:

- `update` updates the **ccync binary itself**.
- `upgrade` advances Git-sourced plugins to newer upstream revisions.
- `search` discovers plugin repositories across public GitHub and GitLab hosts.
- `bundle` identifies the catalog's built-in `bundled-local` distribution mechanism.
- `roster` references the project-level manifest of team-shared personal plugins.

Always write the product and executable name `ccync` in lowercase.

Rust identifier conventions: functions and modules use `snake_case`, types use `UpperCamelCase`, and constants use `SCREAMING_SNAKE_CASE`. Plugin IDs use kebab-case. Where this summary is silent, follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/naming.html).
