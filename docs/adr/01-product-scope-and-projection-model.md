---
type: adr
title: "ADR 01: Product scope and projection model"
description: Defines ccync as an independent cross-agent manager with one production projection flow and canonical targets.
tags: [ccync, adr, projection]
status: stable
locale: en
translation_key: adr-01-product-scope-and-projection-model
---

# ADR 01: Product scope and projection model

## Background

During architectural planning, ccync had two competing design roles: operating as an auxiliary synchronization script coupled to a single master agent's native plugin architecture, or operating as an independent manager governing plugin lifecycles and projecting configurations across multiple coding agents. This decision firmly adopts the independent manager architecture. Although legacy configuration fields such as `devMode` and `ccyncRoot` persist for backward compatibility, production projection does not maintain bifurcated development and normal pipelines.

- Impact scope: machine-state directory layout, source catalog authority, five agent families, seven physical projection targets, projection CLI commands, and documentation terminology.
- Real alternatives: retain the primary agent's local directory as the permanent source of truth, or preserve dual build-output pipelines selected via `devMode`.
- Verifiable evidence: `ProjectionTargetId` models seven canonical targets; `AgentId::expand` expands `agy` into three targets; production `run_update` executes a single projection pipeline; lockfiles, caches, and build outputs reside under `~/.ccync/`.

## Decision

ccync operates as an independent cross-agent configuration manager. It manages source lifecycles and maintains all local machine state under `~/.ccync/`. Production execution follows a single unified projection pipeline rather than divergent development and standard paths. Users select among five high-level agent families, while execution and status reporting resolve seven distinct physical projection targets. The primary master agent provides an initial adoption snapshot during initialization; it does not serve as the permanent source of truth for plugin sources. Standalone imported skills constitute the only exception: subsequent sync operations may refresh their snapshots from the primary agent's directory.

## Rejected alternatives

- Permanent dependency on a single primary agent: native file formats and filesystem locations diverge substantially across agents, meaning one agent's format cannot faithfully represent all targets. Except for loose adopted skills, synchronization must not treat a single agent's mutable filesystem state as the authoritative source of truth.
- Maintaining dual development and production execution pipelines: supporting divergent pipelines would fragment ownership proofs, cache pruning rules, and regression tests. Legacy configuration fields remain parsed, but production projection ignores them.
- Collapsing the three Antigravity targets into a single target: the Antigravity CLI, IDE, and GUI faces differ in lifecycle stages, physical file locations, and configuration formats. Merging them would prevent granular completion tracking and status reporting for each face.

## Consequences

Operators maintain global configuration in a single location (`~/.ccync/`). Target adapters bridge native schema and filesystem differences across tools. Documentation must never describe legacy compatibility fields as active execution switches. Any future agent integration must define an agent identifier, target expansion rules, lifecycle phase responsibilities, and verifiable physical completion evidence. Providing a filesystem destination alone is insufficient.
