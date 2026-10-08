---
type: adr
title: MCP trust boundary and recovery
description: MCP fields, secrets, per-host ownership, and fail-closed interruption recovery.
tags: [adr, mcp, security]
status: stable
locale: en
translation_key: adr-04-mcp-trust-boundary-and-recovery
---

# MCP trust boundary and recovery

## Background

An MCP server configuration includes transports, executable paths, arguments, URLs, environment variables, HTTP headers, and variable placeholders. Because the five supported MCP hosts use distinct native configuration formats, a single filesystem rename cannot update all hosts atomically. An erroneous ownership inference could overwrite existing user-configured servers. Furthermore, recording resolved definitions within transaction journals would create redundant plaintext copies of sensitive credentials on disk.

- Impact scope: plugin author input validation, placeholder expansion, five serializers, native host configuration files, ownership state tracking, interruption recovery, and backup credential redaction.
- Real alternatives: pass unrecognized fields through blindly, execute plugin-defined helper scripts, track servers solely by global name, back up entire native host files, or abort the entire projection process when a single server fails to resolve.
- Verifiable evidence: `read_author_mcp_servers` tests schema precedence and warnings; `ProjectionTransaction` persists file fingerprints instead of server definitions; recovery tests cover written, unwritten, and divergent fingerprint states; unresolved server placeholders do not block independent components.

## Decision

The plugin author boundary accepts only explicitly modeled MCP schema fields. ccync logs warnings and excludes unrecognized fields, and it refuses to execute helper scripts such as `headersHelper`. It resolves environment variable placeholders exclusively within trusted fields and records a concrete error for servers that fail to resolve. Each MCP host uses a dedicated serializer and atomic writer. Before writing host files, ccync logs a write-ahead transaction. After each write, it re-reads the host file to verify its cryptographic fingerprint. ccync commits per-host ownership records to `projected-state.json` only when all participating hosts yield verifiable outcomes. The transaction journal records only SHA-256 fingerprints or an absent-file marker, never resolved server definitions.

## Rejected alternatives

- **Passing unknown fields or executing helper scripts:** this could permit remote code execution or credential leakage during background synchronization.
- **Treating `managed.json` as physical ownership proof:** `managed.json` is generated before writes occur and cannot prove which hosts actually committed changes.
- **Persisting entire host files for rollback:** this duplicates unrelated user configurations and credentials, and risks overwriting subsequent manual user edits.
- **Aborting all projections when one server fails:** a single missing environment variable should not block independent servers, skills, commands, or agents.

## Consequences

Following an unexpected interruption, ccync compares host fingerprints and converges to a consistent state on the next run. If a host file displays an unexpected third fingerprint or cannot be read, ccync halts automatic recovery and requires manual operator intervention. Missing ownership records never grant ownership over historical configuration entries. Supporting new MCP transports or placeholder fields requires renewed trust boundary and serializer security reviews.
