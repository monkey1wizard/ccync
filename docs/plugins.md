---
type: guide
title: Plugin discovery and management
description: Search, source repair, intake, removal, inspection, and team rosters for ccync plugins.
tags: [plugins, discovery, management, roster]
status: stable
locale: en
translation_key: plugins
---

# Plugin discovery and management

## Search and source repair

`ccync search <term> [--limit N] [--no-add]` searches public GitHub and GitLab repositories. The command aggregates results across CLI tools, APIs, and web search, deduplicating findings by normalized clone URL. In interactive sessions, the operator can select a search result to pass directly to `ccync add`. Passing `--no-add` only displays matching repositories. The `search` command never infers sources for adopted items and never modifies local configuration without explicit user confirmation.

Run `ccync repair [<id>]` when an adopted item lacks recorded source metadata. The command presents candidate sources for review and updates configuration only after explicit selection. If no source can be verified, the item remains marked as `unresolved`. The tip that `upgrade` prints, which suggests running `ccync repair`, is guidance; it does not execute automated repairs.

## Add plugins and obtain sources

`ccync add <source> [--no-sync]` accepts catalog IDs, Git URLs, local Git working directories, plain directories, and archives. For Git sources, ccync clones and caches the repository before resolving the plugin root. When specifying a repository subpath, the subpath must remain strictly inside the repository root; ccync rejects absolute paths and `..` directory traversal attempts. Archive extractions enforce the identical path containment boundary.

Plain local directories are imported as static snapshots rather than automatically upgradeable upstream repositories. Passing `--no-sync` records the plugin in the personal sources list without triggering projection, and operates cleanly prior to system initialization.

A plugin can contribute skills, commands, custom agents, and MCP definitions. ccync maps supported Claude-format plugin metadata and components into a unified internal representation. If a source directory is empty or contains no valid components, ccync flags the error rather than reporting a successful installation.

Commands stay on native command surfaces where available. On targets that expose commands as skills, ccync converts only flat commands whose invocation can be represented without changing argument semantics. Commands using positional arguments or shell-style argument expansion, and namespaced commands, are reported as unsupported before publication. The command is not partially published when its invocation cannot be preserved. Supported flat conversions retain the command body, description, and `$ARGUMENTS` placeholder.

## Remove, list, and inspect

`ccync remove <id>` removes a managed plugin from personal sources, lockfile records, and subsequent projections. File deletions on live agent surfaces take effect after the required write confirmation. If an existing filesystem path with the same name lacks ownership proof, ccync preserves the file.

`ccync list [--upgrade-available]` displays all personal and adopted items. `ccync show <id>` reports an individual item's source configuration, pinned commit, internal components, and current lifecycle status. Both query commands function before machine initialization.

Source management and agent projection represent distinct lifecycle stages. This document covers source registration, repair, and removal. See [Architecture](architecture.md) for projection details, and [Commands](commands.md) for version upgrades and cache cleanup.

## Package names and management IDs

Each plugin has a package name and a management ID. The package name is the canonical name that a native client lists and uses in its namespace, such as `Impeccable`. The management ID is the storage key that ccync commands accept, such as `plugin`. Existing management IDs and pins do not change. The package name is display identity only, and no command accepts it as a selector.

Identity follows source form. A native plugin retains its source package identity on a supported native route. A standalone skill or already materialized component uses its source and component identity directly and does not gain a manager-derived `ccync:` namespace. If separate sources collide in a native namespace, sync reports the conflict and preserves both sources for diagnosis.

`ccync list` shows one line for each plugin whose package name differs from its ID. The suggested command names the management ID:

```text
Package: Impeccable | management ID: plugin (remove with: ccync remove plugin)
```

`ccync show plugin` lists the package name first, then the management ID:

```text
Plugin: Impeccable | management ID: plugin
```

`ccync show Impeccable` fails and prints this error on stderr:

```text
ccync show: 'Impeccable' not found. Run `ccync list` to see installed items.
```

Use the management ID in every command:

```text
ccync show plugin
ccync upgrade plugin --dry-run
ccync remove plugin --yes
```

`ccync remove` updates ccync's own state before it asks for confirmation. It removes the plugin from the personal catalog (or the adopted-item state), updates the ccync lockfile, and renders the canonical output. The confirmation gate then covers only the live agent projection. Without `--yes`, ccync stops at that gate and writes no live agent config. Run `ccync sync --yes` to apply the remaining step. ccync has no enable or disable command. Enabled state belongs to the client's own plugin manager.

## Native packages for Claude Code and Codex

When the adapter for a client is verified, ccync installs each plugin group for Claude Code and Codex as one native package through that client's own plugin manager. The native package keeps the canonical package name and the source assets. Its view removes active MCP entries and excludes hooks, because hooks are outside native package delivery. MCP servers stay with ccync's central MCP writer. The group works as one unit. If any member cannot activate, the group keeps one route for the plugin. A group with no committed native record keeps its loose route. A group with a prior or legacy native record keeps that native route, and the loose copy does not load beside it. Sync reports the reason either way.

A native component declaration that the package view cannot carry safely is refused with a reason. The refusal blocks that view, so the group takes the fallback route described above. The refusal names the target, the plugin ID, and the reason.

Portable package storage is a separate, non-live snapshot under `P/packages/`. ccync admits digest-verified package bytes only when every certified declaration passes this closed rule list:

- Plugin manifests accept string `name`, `version`, `description`, and `license`; string arrays for `keywords`; a string or an object with optional string `name` and `email` fields and an optional portable HTTPS `url` for `author`; portable HTTPS `homepage` and `repository` strings or a `repository` object with a required portable HTTPS `url` and optional string `type`; and safe package-relative `commands`, `agents`, and `skills` paths as strings or arrays of strings. A portable HTTPS URL uses an ASCII host and path, an optional nonempty numeric port, no user information, query, fragment, percent escape, or `.` and `..` path segments. The manifest `mcpServers` value may be a root MCP filename reference or an empty object. `hooks` and `lspServers` are rejected. The manifest `name` must match `^[A-Za-z_][A-Za-z0-9_.-]{0,63}$`.
- Root MCP files accept only an `mcpServers` object. Each server may contain only `type`, `command`, `args`, `url`, `env`, and `headers`. Server names and keys in `env` and `headers` must match the same identifier pattern. Types are `stdio`, `http`, or `sse`; commands cannot contain whitespace; `args` must be an array of strings; and URLs must follow the portable HTTPS rule above. Every `env` and `headers` value must be a `${NAME}` placeholder.
- For MCP `args`, each option whose name, after lowercasing and removing `-`, `_`, and `.`, contains `token`, `secret`, `password`, `passwd`, `bearer`, `credential`, `apikey`, `key`, `auth`, `cookie`, `session`, `signature`, or `private` must have a `${NAME}` placeholder value. Whitespace-bearing argument values are admitted only in the `name: [label ]${NAME}` form.
- The only admitted skill sidecar is exactly `skills/<nonempty-id>/agents/openai.yaml`. Its root is null or a mapping containing only `interface` and `policy`. `interface` accepts only the six string fields `display_name`, `short_description`, `icon_small`, `icon_large`, `brand_color`, and `default_prompt`. Icon paths must be safe relative paths. `policy` accepts only boolean `allow_implicit_invocation`; MCP dependencies reject admission. Duplicate YAML keys reject the sidecar.
- Certified JSON surfaces are plugin manifests and root MCP files. They are parsed once, rejecting duplicate object keys at every depth after JSON escape decoding. Admission validates that parsed value and publishes the original bytes only after validation succeeds.

Hooks and every unsupported or unclassified configuration file reject the whole package and leave its source private. String values not otherwise constrained by this list are uncertified free text by definition. Other uncertified surfaces include opaque assets, files outside the configuration extension list, Markdown bodies and frontmatter, MCP positional and short-option literals without whitespace, and opaque parts of otherwise accepted declarations. Findings on uncertified surfaces are risks for later review or P3 work, not proof of secret absence. Opaque assets remain byte-faithful and are not certified secret-free. Native views continue to strip MCP and hook entrypoints.

Copilot, AGY, and OpenCode have no native package route in ccync. They keep their loose skill or component routes.

A native change reaches a running app only after you fully quit the app, reopen it, and start a new session. ccync does not reload agents. A registration you disabled in the client stays disabled, with zero loads. Sync does not enable it.

Recovery states:

- **Pending intent.** An interrupted native change leaves an entry in `build/projection.txn.json`. The next sync reconciles it against the live registration before any retry. ccync never repeats an install or removal blindly.
- **Adoption.** An existing registration is adopted only when the pending intent names the same selector and a fresh observation matches the recorded view digest. A same-name registration without that proof is a conflict.
- **Recovery blocked.** Foreign drift or an ambiguous intent blocks recovery. ccync keeps the intent, its preimages, and the registration, and reports the conflict. Do not delete ccync files or native registrations by hand.
- **Standalone publication and withdrawal.** Sync and doctor report standalone publication, recovery blocked, identity unresolved, and retained or fallback outcomes distinctly. The provider lifecycle journal is written as schema v3. V1 and v2 remain readable only when they contain no intent types those schemas cannot represent. Pending intents are not downgraded or discarded. Recovery uses the recorded intent and ownership evidence before retrying a write or removal.

`ccync doctor` reports native state read-only. The states are: registered, with loader not run; deliberately disabled; owned from an older generation; unavailable or unreadable; absent; broken; drifted from the committed generation; and foreign. A registration in a client's listing does not prove that the client loaded the package. See [Setup](setup.md#native-package-acceptance-status) for the current acceptance status.

CLI status describes observed registration and filesystem state. It does not establish loading in an attached desktop or IDE interface, account-bound enablement counts, or real-machine migration. Those checks remain owner acceptance work. Tool lifecycle and installation of new tool snapshots remain outside this plugin delivery scope, as do MCP publication changes and hooks.

## Team roster

The project-level `ccync.roster.json` manifest defines personal plugins shared across a team. It is not a catalog bundle, lockfile, or machine backup. It never contains adopted items, plaintext MCP secrets, or snapshots of native agent configuration files.

`ccync roster dump [--out <path>]` exports currently registered personal plugin sources into a deterministically sorted roster file, defaulting to `ccync.roster.json`. It records portable source URLs and subpaths. If an item lacks a reproducible remote source, ccync reports the limitation rather than substituting temporary local filesystem paths.

`ccync roster install [--in <path>] [--dry-run] [--no-sync] [--yes]` validates manifest syntax and checks for duplicate IDs before fetching plugins through standard `add` workflows. Running `--dry-run` summarizes planned additions, existing plugins, ID conflicts, and errors. The `--no-sync` flag registers sources without modifying agent configuration files. Executing a live initial projection requires user confirmation.

A team roster never authorizes deleting private plugins that are absent from the manifest. When the same plugin ID maps to conflicting sources, the operator must resolve the conflict manually. Identical naming does not constitute ownership proof. Commit the roster to version control and review source changes during code review.
