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

Run `ccync repair [<id>]` when an adopted item lacks recorded source metadata. The command presents candidate sources for review and updates configuration only after explicit selection. If no source can be verified, the item remains marked as `unresolved`. The `Tip: run ccync repair` hint emitted during `upgrade` is guidance; it does not execute automated repairs.

## Add plugins and obtain sources

`ccync add <source> [--no-sync]` accepts catalog IDs, Git URLs, local Git working directories, plain directories, and archives. For Git sources, ccync clones and caches the repository before resolving the plugin root. When specifying a repository subpath, the subpath must remain strictly inside the repository root; ccync rejects absolute paths and `..` directory traversal attempts. Archive extractions enforce the identical path containment boundary.

Plain local directories are imported as static snapshots rather than automatically upgradeable upstream repositories. Passing `--no-sync` records the plugin in the personal sources list without triggering projection, and operates cleanly prior to system initialization.

A plugin can contribute skills, commands, custom agents, and MCP definitions. ccync maps supported Claude-format plugin metadata and components into a unified internal representation. If a source directory is empty or contains no valid components, ccync flags the error rather than reporting a successful installation.

## Remove, list, and inspect

`ccync remove <id>` removes a managed plugin from personal sources, lockfile records, and subsequent projections. File deletions on live agent surfaces take effect after the required write confirmation. If an existing filesystem path with the same name lacks ownership proof, ccync preserves the file.

`ccync list [--upgrade-available]` displays all personal and adopted items. `ccync show <id>` reports an individual item's source configuration, pinned commit, internal components, and current lifecycle status. Both query commands function before machine initialization.

Source management and agent projection represent distinct lifecycle stages. This document covers source registration, repair, and removal. See [Architecture](architecture.md) for projection details, and [Commands](commands.md) for version upgrades and cache cleanup.

## Team roster

The project-level `ccync.roster.json` manifest defines personal plugins shared across a team. It is not a catalog bundle, lockfile, or machine backup. It never contains adopted items, plaintext MCP secrets, or snapshots of native agent configuration files.

`ccync roster dump [--out <path>]` exports currently registered personal plugin sources into a deterministically sorted roster file, defaulting to `ccync.roster.json`. It records portable source URLs and subpaths. If an item lacks a reproducible remote source, ccync reports the limitation rather than substituting temporary local filesystem paths.

`ccync roster install [--in <path>] [--dry-run] [--no-sync] [--yes]` validates manifest syntax and checks for duplicate IDs before fetching plugins through standard `add` workflows. Running `--dry-run` summarizes planned additions, existing plugins, ID conflicts, and errors. The `--no-sync` flag registers sources without modifying agent configuration files. Executing a live initial projection requires user confirmation.

A team roster never authorizes deleting private plugins that are absent from the manifest. When the same plugin ID maps to conflicting sources, the operator must resolve the conflict manually. Identical naming does not constitute ownership proof. Commit the roster to version control and review source changes during code review.
