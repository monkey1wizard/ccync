---
type: adr
title: Source resolution and plugin materialization
description: One guarded intake pipeline for catalog, Git, local, and archive sources.
tags: [adr, sources, plugins]
status: stable
locale: en
translation_key: adr-02-source-resolution-and-plugin-materialization
---

# Source resolution and plugin materialization

## Background

Plugins originate from varied distribution sources, including curated catalog IDs, remote Git URLs, local Git repositories, plain directories, and compressed archives. In multi-project repositories, a plugin frequently resides within a nested subdirectory. If individual target projectors were to fetch and parse external sources independently, parsing rules, caching behaviors, and security validations would inevitably diverge. Furthermore, archive entries or symbolic links that escape the cache root represent critical directory traversal vulnerabilities.

- Impact scope: `add`, `upgrade`, `roster install`, revision-keyed caches, Git subpaths, archive extraction containment, and external plugin compatibility.
- Real alternatives: allow target projectors to fetch sources directly, advance lockfile revisions before downloads succeed, or introduce a separate subpath schema field that lockfile merges could discard.
- Verifiable evidence: `fetch_personal_plugin_at` fetches designated revisions; Git cache keys utilize full commit SHAs and archive keys use content SHA-256 prefixes; path containment tests reject escaping links and paths; `upgrade` updates lockfile pins only after new cache trees are validated.

## Decision

All plugin sources pass through a single guarded intake pipeline. ccync fetches the source, validates directory containment boundaries, classifies supported components, and materializes a complete cache tree under `~/.ccync/cache/<id>@<revision>/`. Only validated cache directories are exposed to the canonical render engine. For Git subdirectories, the `//<subpath>` selector remains part of the canonical `source` string. ccync validates the base repository URL and subpath independently, ensuring that the resolved plugin root and any internal links remain strictly within the repository boundary. The intake pipeline also converts external Agent Plugins and Claude plugin formats into unified internal component representations.

## Rejected alternatives

- **Fetching remote sources inside target projectors:** this design would couple networking, archive extraction, security auditing, and native file emission, preventing deterministic builds and offline projections.
- **Advancing lockfile pins before download completion:** a network failure or validation error would leave lockfiles referencing non-existent cache paths.
- **Extracting archives without containment validation:** absolute paths, drive prefixes, and `..` directory traversal sequences could overwrite sensitive files outside the cache directory.
- **Introducing a separate subpath field without full merge support:** rebuilding or serializing lockfiles could inadvertently drop the field, silently altering the active plugin root.

## Consequences

The canonical render engine always consumes a uniform, pre-validated plugin directory. Source registration (`add`), version upgrading (`upgrade`), and team rosters share a single resolution mechanism. Known caches can be reliably reproduced. If a remote source fails to download, ccync reports an error rather than treating the plugin as empty. Strict path containment and validating cache materialization prior to lockfile advancement operate as mandatory security invariants.

Adopted components keep their source and component identity in `plugins.json` under `_adoption`. Lockfile adoption namespaces are derived indexes. Loose skills use their recorded physical origin, and a legacy name-only record requires a source rescan instead of a name-based directory search. The target snapshot records the selection consented to during initialization.
