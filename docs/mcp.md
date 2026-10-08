---
type: reference
title: MCP definitions, secrets, and recovery
description: Author definitions, placeholder resolution, five host writers, and per-host ownership recovery.
tags: [mcp, secrets, recovery]
status: stable
locale: en
translation_key: mcp
---

# MCP definitions, secrets, and recovery

## Definitions and resolution

ccync treats definitions located in `~/.ccync/build/_mcpServers/` as its authoritative MCP server inputs. Each definition can specify `command`, `args`, `url`, `env`, `headers`, and a supported `cwd`. The variable resolver supports `${VAR}`, `${VAR:-default}`, `PLUGIN_ROOT`, `PLUGIN_DATA`, and supported Claude alias syntax. If a required environment variable is missing, ccync identifies and skips that specific server. Other MCP servers and non-MCP projections continue executing normally.

ccync resolves sensitive credential values just before writing configurations to host files. The system never persists resolved plaintext secrets in definition files or lockfile state. The write-ahead recovery journal stores only before-and-after SHA-256 file fingerprints or an absent-file marker. System diagnostics and logs never output authorization tokens, sensitive HTTP header values, or command lines containing secrets.

## Five MCP hosts

ccync updates five fixed MCP host configurations: Claude Code, OpenAI Codex, GitHub Copilot, OpenCode, and Google Antigravity. This host set updates consistently regardless of which agent projection targets are selected. Each host writer merges only servers managed by ccync, preserving unknown top-level schema fields and existing user-configured servers in the native host file. If an existing host file cannot be read or parsed, ccync halts the write for that specific host rather than replacing the file with an empty configuration.

## Ownership and interruption recovery

Before writing native host files, ccync records a write-ahead transaction in the journal. After writing each host file, ccync re-reads the file to verify its new fingerprint. Only after all participating hosts produce verifiable fingerprints does ccync commit ownership records to `mcp/projected-state.json` and clear the transaction journal.

When recovering from an interrupted run, ccync inspects the state of each host file. A host matching the expected after-fingerprint is recognized as successfully updated. A host matching the before-fingerprint remains in its prior state. If a host exhibits an unrecognized third fingerprint or cannot be read, ccync halts automatic recovery and outputs a warning. Removing an MCP server similarly requires per-host evidence proving ccync ownership. See [ADR 04](adr/04-mcp-trust-boundary-and-recovery.md) for the architecture decision governing this contract.
