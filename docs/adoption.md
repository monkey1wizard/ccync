---
type: explanation
title: Adoption and reconciliation
description: Import from one of five sources, source provenance, ownership boundaries, and Graphify skill updates.
tags: [adoption, reconciliation, provenance, graphify]
status: stable
locale: en
translation_key: adoption
---

# Adoption and reconciliation

During `ccync init`, ccync imports existing plugins, loose skills, and MCP servers from the chosen source. Portable provider intent uses stable source and component identities. An admitted HTTPS reference stays in portable intent. Other references use an opaque binding key, while the origin stays in the private personal catalog. MCP definitions and operational ownership fields remain private. Adoption provenance is stored in the personal `plugins.json` under `_adoption`, with the consented target snapshot. Lockfile indexes are projections of those records. Unknown catalog fields and records from other origins are preserved. A missing legacy `plugins` array is treated as empty. A present `plugins` value of another type is rejected.

## Source records

Each adoption record keeps source identity, component kind and identity, original name, origin, revision or digest, enabled state, and non-secret policy metadata when available. MCP credentials are not copied into provenance. They are reread from the source only after fingerprint verification. A loose skill is materialized from its recorded origin. A legacy name-only row has no safe origin and requires a rescan. Missing origins do not authorize deleting output.

The private lock isolates `_adoptedItems`, `_looseSkills`, and `_mcpServers` into dedicated namespaces. Sync rebuilds these index rows from `_adoption`. MCP definitions and loose-skill origins are private bindings. Portable serialization refers to admitted identities without copying those bindings or operational ownership evidence. Reinitialization updates matching source and component identities while retaining unrelated records. Absence from a scan is not removal. Legacy `_adoptMaster` reads remain supported.

ccync excludes vendor-shipped items from adoption. This exclusion list relies on verified native installation catalogs rather than dynamic heuristics or command-line patterns. For example, Codex includes the vendor-owned `node_repl` MCP entry, while Claude Code provides no reachable vendor-owned entries in the initial scan. Doctor reports any pre-existing adopted item that conflicts with the current exclusion list, but it does not rewrite configuration automatically.

## Loose-skill conflicts and rescans

Preview groups loose skills by name. Records with the same name and different origins form one conflict. Preview lists every origin, and sync blocks the whole group and writes none of its members. ccync does not keep the first row or choose a winner. Repeated rows from one origin do not conflict. Unrelated imports still proceed.

```text
loose conflict: hello has 2 origins: /origin/a, /origin/b; sync blocks every member and writes none
```

A loose skill with no recorded origin needs a rescan. Preview names it, and sync skips it.

```text
rescan required: loose skill hello has no recorded origin; sync skips it
```

A legacy adoption record with no origin that matches an import stops confirmation before any write.

```text
rescan required: hello (skill) has no recorded origin; confirming stops before any write
```

## Blocked and disabled MCP definitions

Init preview and completion report each MCP definition that the scan blocked, with its name and reason. They count disabled definitions on a separate line, and each disabled definition keeps its disabled policy. The report never shows credential values.

```text
mcp blocked: 1 server(s), not imported; mcp disabled: 1 server(s)
    <name>: <reason> (<origin>)
    disabled by the source: <name>
```

A blocked definition does not remove other valid items from the same source. Ownership of an MCP entry that a source host already defines is outside this repair. See [Scope of this repair](setup.md#scope-of-this-repair).

## Adopted items report version differences only

`ccync upgrade` inspects and reports version statuses for adopted items, but it does not download or apply updates for them. If both an upstream source and a baseline version are known, the command displays `current → latest`. If that information is missing, the command reports `version-unresolved`. Because MCP servers and loose skills do not expose standardized version numbers in this flow, their respective upstream managers must update them. To transfer management of an adopted source to ccync as a personal plugin, run `ccync remove <id>`, then run `ccync add <source>`. Use the management ID that `ccync list` shows; a package name is not accepted as a selector. Because removal also withdraws projected files from target agents, inspect the planned changes before proceeding.

## Ownership during synchronization

During synchronization, ccync re-reads each loose skill from the origin directory recorded at import and constructs a shared snapshot. ccync never overwrites the source agent's original skill files, and it never re-imports a copy that ccync itself published. When ccync withdraws a projected item from another agent, it requires positive evidence that ccync created that target artifact. The fact that an item was adopted does not authorize ccync to delete existing user files.

## Standalone publication, withdrawal, and recovery

An adopted loose skill is standalone content. `ccync sync` publishes it as a plain skill directory in the skill root of each selected target, except the root that already holds it. Codex, Copilot, and OpenCode share `~/.agents/skills`, so one published copy serves every reader of that root. The published skill keeps its source name and gains no `ccync:` namespace.

When a source no longer supplies the skill, sync withdraws only the copy that ccync published. Withdrawal needs proof from every known reader of the shared root. A reader that was never selected still withholds withdrawal, because its removal or replacement evidence is unknown. In that case the copy and its ownership record stay.

If publication or withdrawal is interrupted, ccync keeps its journal and backup. Run `ccync sync` again to retry. Do not delete the journal, the backup, or the published files by hand. Sync reports `standalone published`, `recovery blocked`, or `identity unresolved` for these routes. These results describe files on disk. They do not show that an agent loaded the skill. See [Setup](setup.md) for the recovery steps.

## MCP source verification

MCP adoption records the source file and a fingerprint at preview time. Before storing a definition, ccync reads that same source again and checks the fingerprint. A missing, changed, or unsafe source is blocked. A definition with credentials or other nonportable fields remains in the original source and is not stored in `_mcpServers`. Original files and credentials remain in place. Source evidence does not authorize deletion and is not included in portable backups. Preservation without authorization across all five formats remains unproven.

## Graphify skill update example

The Graphify skill demonstrates three distinct ownership layers. The PyPI package `graphifyy` provides the `graphify` CLI executable. The native Graphify installer manages the primary skill directory at `~/.claude/skills/graphify/`. ccync captures a snapshot of that directory and projects the snapshot to other configured agents.

Update these layers in order:

```text
pip install -U graphifyy
graphify install --platform claude
ccync sync
```

Always pass the `--platform claude` flag. Without this flag, the Graphify installer writes directly into multiple agent directories, producing divergent versions across tools. If a target path uses a directory junction, the installer can even write through the junction into ccync output directories. Update the Claude-owned source first, then run `ccync sync` to project the fresh snapshot.

ccync treats a loose skill as an opaque directory. It does not parse `.graphify_version`, download upstream updates, or modify the source agent's original files. Even when all projected copies share an outdated version, ccync does not flag the version as stale. The maintainer must verify version freshness directly through Graphify.

If `ccync sync` reports `preserved user-owned path` for Graphify, the destination path already exists and ccync lacks ownership proof. Back up the target path and determine its owner before attempting removal. Do not force an overwrite simply to silence the warning.
