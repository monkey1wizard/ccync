# Catalog curation — carve disposition

How the pre-carve catalog content was disposed of during the gal→ccync carve, and how to re-add
anything later. ccync ships an **empty baked catalog**: the mechanism stays (`ccync add <source>`
resolves arbitrary plugins), but ccync bundles no domain content of its own.

This consolidates three earlier notes (gal-domain bundle deletion · curated-upstream pre-release
disposition · baked-pack archival), which were all one decision arc.

## Deleted: 6 gal-domain bundles (carve D6, owner ruling 2026-06-21)

The pre-carve repo shipped 6 "domain bundles" under `plugins/gal-domain/`
(`sourceType: official-gal-bundle`, `installStrategy: bundled-local`):

- `gal-game-assets` (2D/3D/pixel/UI game-asset skills)
- `gal-godot` (Godot engine skills)
- `gal-obsidian-extras` (obsidian-cli / markdown / bases / json-canvas / graphics-workflow)
- `gal-web-testing` (webapp-testing skill + Playwright / Chrome DevTools MCP)
- `gal-firebase` (Firebase MCP, MCP-only)
- `gal-pdf` (PDF skill)

**Decision: delete, do not migrate.** ccync is a plugin-agnostic manager — it must not ship its own
bundled domain content. The 6 bundles also carry almost no original code (mostly thin MCP pointers
to external `npx` servers plus upstream-copied or thin-wrapper skills). The carve deletes
`plugins/gal-domain/` and removes the 6 entries from `plugins/catalog.json`. No migration project is
opened. (`feat-pdf-chandra-upgrade`, which upgraded the gal-pdf bundle, is moot once the bundle is
gone and was dropped during the carve.)

## Archived: 8 curated-upstream skill packs (2026-06-23, `refactor-ccync-sync-engine-and-commands`)

These 8 `curated-upstream` skill packs were previously baked into `plugins/catalog.json`. They were
removed when ccync moved to an empty baked catalog; kept here for reference / future re-curation.

Shared metadata: `sourceType: curated-upstream`, `installStrategy: git-clone`,
`checksumPolicy: commit-sha`, `ref: main`, `path: skills`, `componentMap: { skills: true }`,
`supportedProviders: [claude, copilot, codex, agy]`, `allowAutoUpdate: false`,
`localOverridePolicy: source-mode-only`.

| pluginId | displayName | repo | license | profile |
| --- | --- | --- | --- | --- |
| dart-skills | Dart Skills | https://github.com/dart-lang/skills | BSD-3-Clause | dart |
| flutter-skills | Flutter Skills | https://github.com/flutter/skills | BSD-3-Clause | flutter |
| dotnet-skills | .NET Skills | https://github.com/dotnet/skills | MIT | dotnet |
| anthropic-skills | Anthropic Skills | https://github.com/anthropics/skills | MIT | claude-ecosystem |
| golang-skills | Go Skills | https://github.com/samber/cc-skills-golang | MIT | go |
| swift-skills | Swift Skills | https://github.com/twostraws/swift-agent-skills | MIT | swift |
| obsidian-skills | Obsidian Skills | https://github.com/kepano/obsidian-skills | MIT | obsidian |
| rust-skills | Rust Skills | https://github.com/actionbook/rust-skills | MIT | rust |

Community packs (golang / swift / rust) carried a "provider support, license, and drift policy must
be verified per lockfile" caveat.

## Background: why these were never live (resolved by the archival above)

A pre-release note (2026-06-19) flagged that these curated-upstream entries were really the owner's
personal recommendation list, not officially-governed dependencies (trust / license / drift policy
unverified), and that the engine never actually cloned them — git-clone fetch was filtered out, so
the entries were "listed but dormant," zero effect on users. The open question was whether to
**remove**, **demote to a personal layer** (`~/.ccync/local/catalog.json`), or **formally adopt**
them (requiring per-repo trust/drift governance).

That question was answered by the 2026-06-23 archival: the baked catalog is now empty, so none of
these ship publicly. Anyone wanting one re-adds it explicitly.

## Re-adding anything later

Register it the same way as any plugin: point `ccync add` at the upstream repo. No new code path is
needed — the catalog/resolve pipeline handles git-clone sources generically.

```sh
ccync add https://github.com/<owner>/<repo>
```
