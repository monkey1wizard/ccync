---
type: explanation
title: Adoption and reconciliation
description: Master-agent adoption, source provenance, ownership boundaries, and Graphify skill updates.
tags: [adoption, reconciliation, provenance, graphify]
status: stable
locale: en
translation_key: adoption
---

# Adoption and reconciliation

During `ccync init`, ccync imports existing plugins, loose skills, and MCP servers from the chosen master agent. ccync marks these entries as adopted items in the lockfile and projects them to other targets using the same item IDs. The adoption process leaves the master agent directories in place. It does not convert master-owned sources into ccync personal plugins.

## Source records

ccync records the source type, upstream marketplace metadata, master agent identifier, and initial baseline version when available. For an MCP server, ccync preserves the complete server definition rather than just the name. For a loose skill, ccync records the source directory path under the master agent so that future synchronizations can capture updated snapshots. If an item lacks complete source metadata, ccync still imports it, marks its status as `unresolved`, and allows `ccync repair` to supply the missing source information later.

The lockfile isolates `_adoptedItems`, `_looseSkills`, and `_mcpServers` into dedicated namespaces. An adopted item's `sourceId` identifies its source category and links to the corresponding secondary record. Removing an adopted item removes both its primary entry and its secondary record. Loose skills are copied into the canonical render after initialization and refreshed from the recorded master path during subsequent sync operations without altering the original files.

ccync excludes vendor-shipped items from adoption. This exclusion list relies on verified native installation catalogs rather than dynamic heuristics or command-line patterns. For example, Codex includes the vendor-owned `node_repl` MCP entry, while Claude Code provides no reachable vendor-owned entries in the initial scan. Doctor reports any pre-existing adopted item that conflicts with the current exclusion list, but it does not rewrite configuration automatically.

## Adopted items report version differences only

`ccync upgrade` inspects and reports version statuses for adopted items, but it does not download or apply updates for them. If both an upstream source and a baseline version are known, the command displays `current → latest`. If that information is missing, the command reports `version-unresolved`. Because MCP servers and loose skills do not expose standardized version numbers in this flow, their respective upstream managers must update them. To transfer management of an adopted source to ccync as a personal plugin, run `ccync remove <id>`, then run `ccync add <source>`. Because removal also withdraws projected files from target agents, inspect the planned changes before proceeding.

## Ownership during synchronization

During synchronization, ccync re-reads loose skills from the master agent directory and constructs a shared snapshot. ccync never overwrites the master agent's original skill files. When ccync withdraws a projected item from another agent, it requires positive evidence that ccync created that target artifact. The fact that an item was adopted does not authorize ccync to delete existing user files.

## Graphify skill update example

The Graphify skill demonstrates three distinct ownership layers. The PyPI package `graphifyy` provides the `graphify` CLI executable. The native Graphify installer manages the primary skill directory at `~/.claude/skills/graphify/`. ccync captures a snapshot of that directory and projects the snapshot to other configured agents.

Update these layers in order:

```text
pip install -U graphifyy
graphify install --platform claude
ccync sync
```

Always pass the `--platform claude` flag. Without this flag, the Graphify installer writes directly into multiple agent directories, producing divergent versions across tools. If a target path uses a directory junction, the installer can even write through the junction into ccync output directories. Update the Claude-owned source first, then run `ccync sync` to project the fresh snapshot.

ccync treats a loose skill as an opaque directory. It does not parse `.graphify_version`, download upstream updates, or modify the master agent's original files. Even when all projected copies share an outdated version, ccync does not flag the version as stale. The maintainer must verify version freshness directly through Graphify.

If `ccync sync` reports `preserved user-owned path` for Graphify, the destination path already exists and ccync lacks ownership proof. Back up the target path and determine its owner before attempting removal. Do not force an overwrite simply to silence the warning.
