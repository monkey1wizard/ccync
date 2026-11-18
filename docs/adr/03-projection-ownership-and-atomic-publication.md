---
type: adr
title: Projection ownership and atomic publication
description: Ownership evidence and staged publication protect shared agent files.
tags: [adr, ownership, atomicity]
status: stable
locale: en
translation_key: adr-03-projection-ownership-and-atomic-publication
---

# Projection ownership and atomic publication

## Background

Projection modifies files and directories shared directly with users, coding assistants, and other development utilities. A matching filename or identical content does not prove that ccync created or manages that item. Deleting files without positive ownership records risks catastrophic data loss. Similarly, regenerating the active canonical render directory in place risks leaving corrupt, partially updated files if the process is interrupted.

- Impact scope: canonical rendering, skill/command/agent adapters, managed instruction blocks, item withdrawal, uninstallation, and per-target status tracking.
- Real alternatives: assume ownership of all same-named items, take full ownership of shared native directories, rebuild the live render directory in place, or infer completion status from aggregate success counters.
- Verifiable evidence: `ManagedArtifactRegistry` collision tests verify preservation of unowned files; `render_canonical_root` constructs a sibling staging directory prior to `atomic_swap`; process interruption tests prevent false ownership commits; `ProjectionRunAccumulator` isolates `NotRun` from completed tasks.

## Decision

ccync requires explicit ownership proof before modifying or removing any native agent artifact. It builds the canonical render tree in a temporary sibling staging directory and publishes it via an atomic directory swap (`atomic_swap`). For individual native configuration files and managed instruction blocks, ccync writes changes to a sibling temporary file in the same directory and executes an atomic rename. Target adapters report granular path events and phase-completion evidence, enabling the engine to classify each target accurately. The absence of an error does not constitute proof of completion.

Ordinary sync publishes provider components to the direct target surfaces by default. A combined `ccync` surface is retired in the same sync only after that target's direct publication succeeds. When a target's replacement fails, its combined writer remains as that target's fallback, and the failed target keeps its existing surface. A sync that deselects a target removes the direct copies the lockfile recorded for it, and uninstall removes every recorded direct copy.

## Rejected alternatives

- **Inferring ownership from identical filenames or byte contents:** users and other tools can legitimately create files sharing the same name or content.
- **Claiming entire shared agent directories:** users and other developer tools maintain independent files within these directories.
- **Mutating active render directories in place:** an interrupted build would leave an inconsistent mixture of stale and fresh files. Staging failures must leave active renders unmodified.
- **Relying on aggregate batch counters:** a target without recorded path events or phase evidence remains `NotRun`, even if other targets complete successfully.

## Consequences

When ownership records are absent or ambiguous, ccync retains candidate files rather than risking user data loss. Future target adapters must integrate with the managed artifact registry and emit explicit phase-completion evidence. This decision governs atomic filesystem operations and canonical render swaps; cross-host MCP coordination relies on the separate write-ahead transaction model defined in [ADR 04](04-mcp-trust-boundary-and-recovery.md).
