---
type: adr
title: Backup profiles and secret handling
description: Portable machine profiles with default modeled-secret redaction and explicit limits.
tags: [adr, backup, secrets]
status: stable
locale: en
translation_key: adr-06-backup-profiles-and-secret-handling
---

# Backup profiles and secret handling

## Background

Migrating configuration to another machine requires transferring user settings, personal plugin registrations, adoption snapshots, and managed user instructions. Conversely, downloaded caches, canonical render outputs, and native agent configuration files can be reconstructed on demand. Archiving the entire `~/.ccync/` directory tree or exporting raw MCP definitions risks leaking plaintext tokens, machine-specific ownership proofs, and transaction records across environments.

- Impact scope: `ccync backup`, `ccync restore`, `config.json#secrets`, MCP environment and header placeholders, dry-run inspection, roster installation, and subsequent sync operations.
- Real alternatives: archive the entire `~/.ccync/` directory, strip secrets only when explicitly requested via flags, back up only `config.json` and `plugins.json`, or restore by directly overwriting native agent configuration files.
- Verifiable evidence: backup test suites verify inclusion of inputs and `AGENTS.md`; default backups and compatibility flags redact modeled secrets; cross-machine test fixtures reject plaintext credentials; `restore --dry-run` reports file differences without writing to disk.

## Decision

A backup represents a portable machine profile rather than a full disk image.

1. **Structured scope:** bundle `config.json`, `plugins.json`, adoption snapshots that routine synchronization cannot re-fetch, and managed user instructions in `AGENTS.md`. Exclude download caches, generated render trees, per-host MCP ownership states, and local transaction journals. The destination machine must regenerate these artifacts independently.
2. **Default redaction:** redact modeled configuration secrets by default. Replace sensitive MCP environment variables and HTTP headers with named placeholders in the format `${CCYNC_SECRET_<SERVER>_<KEY>}`. If ccync detects a recognized plaintext credential that cannot be safely converted, it halts export before writing to disk. The `--strip-secrets` flag is retained for backward compatibility with identical behavior to the default.
3. **Controlled restore:** `ccync restore --dry-run` reports matching, divergent, and unreadable items without modifying disk state. A live restore merges machine configuration inputs but never directly overwrites native agent configuration files. On the destination machine, operators must provision required secret variables, install the team roster, and execute `ccync sync` to generate projections.

## Rejected alternatives

1. **Archiving all of `~/.ccync/`:** cache and build artifacts are derived, while ownership records and journals are strictly machine-specific. Packaging these files increases backup size and secret leakage risk.
2. **Requiring an opt-in secret redaction flag:** omitting a flag could easily commit plaintext credentials to version control. Redaction must operate as the default behavior.
3. **Overwriting native agent configuration files during restore:** this would bypass target adapters and ownership checks, potentially destroying existing user customizations on the destination host.
4. **Backing up only `config.json` and `plugins.json`:** adoption snapshots created during initialization cannot be retrieved via routine sync, meaning a new machine would lose imported local skills and configurations.

## Consequences

- A portable profile transfers between machines without carrying recognized plaintext credentials. Named placeholders indicate which environment variables must be provisioned on the destination host.
- Running `--dry-run` prevents unintended configuration overwrites, but restoration still requires manual credential provisioning followed by `ccync sync`.
- Automated redaction cannot guarantee that arbitrary free-form text is completely secret-free; operators must review custom plugin descriptions and instructions before sharing profiles publicly.
- Adding new sensitive fields requires corresponding backup parsing, placeholder mappings, and refusal-before-write test coverage.
