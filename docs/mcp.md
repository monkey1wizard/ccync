---
type: reference
title: MCP definitions, secrets, and recovery
description: Choose routes, preview changes, and diagnose MCP attempts.
tags: [mcp, secrets, recovery]
status: stable
locale: en
translation_key: mcp
---

# MCP definitions, secrets, and recovery

## Personal list and routes

Edit `~/.agents/mcp.json` for direct declarations and route overrides. The required root key is `ccyncMcpServers`; the optional second root key is `hostRoutes`. An empty list still needs `"ccyncMcpServers": {}`. A third-party root `mcpServers`, mixed keys, unknown root keys, unreadable input, or simultaneous old and new inputs cause a conflict. ccync preserves the original and stops before MCP writes. It does not merge or move the files automatically. If neither input exists, direct declarations are empty.

If only the legacy input is present, preview identifies its actual path and the new destination. To switch, copy the old file to a non-active backup, rename its root `mcpServers` to `ccyncMcpServers`, place that copy at `~/.agents/mcp.json` while retaining `hosts` and `hostRoutes`, move the old active input away, then run `ccync sync --dry-run`. Keep the backup. Resolve any conflict before proceeding.

For an isolated example, use this personal list. The names and URL are fictional. The healthy server is a control, not evidence that any real service is healthy.

```json
{
  "ccyncMcpServers": {
    "failing": {"command": "node", "args": [], "hosts": ["codex"]},
    "healthy": {"url": "https://mcp.example.com", "type": "http", "hosts": ["claude"]}
  },
  "hostRoutes": []
}
```

`hosts` is an explicit set. A new stdio server without an explicit route is `unrouted` and is not newly published. An empty set requests withdrawal of provably owned copies. An `enabled: false` declaration is disabled. For a provider or adopted declaration, use a row such as `{"source":"user","server":"failing","hosts":["codex"]}` in `hostRoutes`, replacing `source` with the reported source identity. An override takes precedence over inline `hosts`. Conflicting overrides are refused. Remote servers can use their defaults when no explicit route exists. Selecting multiple stdio hosts can cause each active host session to launch its own process.

The five route keys and native destinations are:

| Key | Native configuration |
| --- | --- |
| `claude` | `~/.claude.json` |
| `codex` | `~/.codex/config.toml` |
| `copilot` | `~/.copilot/mcp-config.json` |
| `opencode` | `opencode/opencode.json` under the platform config directory |
| `agy` | `~/.gemini/config/mcp_config.json` |

Each host is planned independently of agent projection selection. Sources and foreign same-name entries are kept. A collision blocks that host and makes sync fail, while other safe hosts may complete. An unreadable host grants or revokes no ownership. An imported original stays in its source file, including when unsafe to copy. A managed destination is a separate copy and may be withdrawn only with committed proof for that host.

## Portable declarations and secrets

Choose one transport: `command` for stdio or `url` for HTTP/SSE. Use supported fields such as `args`, `env`, `headers`, and `cwd`; do not use helper commands such as `headersHelper`. Portable backup rejects URL userinfo, literal credentials, private absolute paths and unclassified opaque arguments. Even a harmless string can be refused when it cannot be classified safely. Replace machine-specific paths with named references such as `${MCP_LOCAL_DIR}`. Use `${MCP_TOKEN}` for a secret and supply it through the local environment or machine secret settings. Secret references cannot contain inline defaults. Ordinary variables may use supported defaults; an unresolved ordinary variable is reported separately from `missing-secret`.

Resolved secrets are restricted to the running process, machine secret settings, the original source that already contained them, and host files that require them. Declarations, exports, lockfiles, ownership records, journals, diagnostics and backups must not acquire resolved secrets. Backups refuse unsafe direct input, userinfo and placeholder-name collisions before creating output. They can redact supported adopted values. See [backup and restore](setup.md#backup-and-restore).

## Preview, sync, and recovery

Run `ccync sync --dry-run` to inspect per-server and per-host decisions before `ccync sync`. Initialization with `ccync init <source>` also previews imported originals before consent and can proceed to live projection. Preview and doctor are read-only. They report sources, route reasons and launch form, including `unrouted`, `excluded`, `disabled`, `unsupported`, `missing-secret`, `unresolved-ordinary-variable`, `source-preserved`, `blocked` and `failed`. `applied` means a verified write, not a live connection. Missing secrets or unverified launch forms retain existing managed copies and evidence. When the support table is empty, Windows stdio publication is always `blocked`; no host version is verified by this example. Follow-up host qualification supplies the measurements and event capture tool.

An unchanged second sync preserves unchanged host file bytes and does not reload them. A journal records before and after fingerprints rather than secret values. After interruption, a matching after fingerprint can be credited and a matching before fingerprint remains prior state. An unreadable entry or third fingerprint stops recovery and retains the journal. Do not delete the journal to force progress. Resolve the conflict with the source and host evidence intact. An interrupted restore has a separate pending marker. Rerun restore with the same backup; a third fingerprint requires manual resolution.

For a previously removed item, first classify per-host evidence as confirmed missing, uncertain history, or a current explicit choice. Only reselect the exact held item after reviewing it: `ccync sync --reselect-mcp SOURCE SERVER HOST --dry-run`, then `ccync sync --reselect-mcp SOURCE SERVER HOST`. Other holds remain unchanged. Consult [recovery candidates](adoption.md) for the evidence checklist. Live recovery and P7 application require separate authorization for each run.

### Isolated recovery replay

This Windows fixture uses fictional HTTP endpoints and the source label `user`. It creates its own prior records and does not restore user history. Build the debug CLI with `cargo build -p ccync-cli`, then run the following from the repository root:

```powershell
powershell -NoProfile -File verification/mcp/Invoke-Verification.ps1 -Case replay -Mode Fixture -EvidenceRoot <evidence-root>
```

The report points to separate `en` and `zh-Hant` fixture directories. Each contains prior host bytes and SHA-256 digests, command arguments, output and exit codes, snapshots, diffs and an offline checklist. Each run publishes fictional `alpha` and `beta` to Claude and Codex, records that prior state, then removes the copies to create holds. The prior records identify `user alpha claude` and `user alpha codex` as confirmed missing. The unselected `beta` rows represent current choices, and a count without a named server remains uncertain history.

Review the complete `preview` output. For each confirmed item, the fixture runs the exact triple with `--dry-run` and then without it. The two selections are separate operations. Inspect `after` for host bytes and per-host `publications` and `holds`, then compare `second` with `after`. The unselected `beta` holds remain, and the second sync does not expand host files. The fixture also introduces a foreign `gamma` conflict and records the nonzero sync result and preserved source and host bytes in `conflict-diff.json`. This fixture authorization does not authorize any live operation.

Use `offline-checklist.json` with the preview, ownership records and any sanitized event report. Versions and live connections remain unverified. A fixture pass does not qualify a Windows host or prove a real process, connection, or recovery. The support table and its exact qualification evidence govern stdio publication.

## Diagnose repeated attempts

Save the following synthetic, sanitized file as `events.json`. It represents one use operation with two failing attempts and zero target processes or windows. It does not record PIDs. Version `1.0` is fictional and unverified.

```json
{
  "version": 1,
  "run": "run1",
  "events": [
    {
      "seq": 1,
      "time": "2026-01-01T00:00:01Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "0",
      "parent": null,
      "kind": "observe-start",
      "source": "native-config",
      "actor": "unknown",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 2,
      "time": "2026-01-01T00:00:02Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "1",
      "parent": null,
      "kind": "use",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 3,
      "time": "2026-01-01T00:00:03Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "1",
      "parent": 2,
      "kind": "dispatch",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 4,
      "time": "2026-01-01T00:00:04Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "1",
      "parent": 3,
      "kind": "launch-request",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 5,
      "time": "2026-01-01T00:00:05Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "1",
      "parent": 4,
      "kind": "failure",
      "source": "native-config",
      "actor": "agent",
      "result": "error",
      "digest": null
    },
    {
      "seq": 6,
      "time": "2026-01-01T00:00:06Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "2",
      "parent": 5,
      "kind": "dispatch",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 7,
      "time": "2026-01-01T00:00:07Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "2",
      "parent": 6,
      "kind": "failure",
      "source": "native-config",
      "actor": "agent",
      "result": "error",
      "digest": null
    },
    {
      "seq": 8,
      "time": "2026-01-01T00:00:08Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "2",
      "parent": 7,
      "kind": "bound-exceeded",
      "source": "native-config",
      "actor": "agent",
      "result": "ok",
      "digest": null
    },
    {
      "seq": 9,
      "time": "2026-01-01T00:00:09Z",
      "host": "codex",
      "hostVersion": "1.0",
      "session": "s1",
      "server": "failing",
      "operation": "op1",
      "attempt": "0",
      "parent": null,
      "kind": "observe-end",
      "source": "native-config",
      "actor": "unknown",
      "result": "ok",
      "digest": null
    }
  ]
}
```

Run `ccync doctor --mcp-evidence events.json`. The diagnostic reads a file of at most 256 KiB. The report should identify `codex`, `failing`, operation `op1`, two attempts, agent initiation, a user-operation trigger followed by agent retry, and `OutOfBounds`. Events 3–5 and 6–7 establish the two attempt chains. The healthy declaration is an independent control, not part of this event scope. Use `ccync doctor` for current configuration decisions. Neither command proves a connection or a real retry count from static configuration. Do not infer a cause from a window, process count, or sync timing. Missing parent events or observation boundaries leave attribution or stop state uncertain. Event files use only the versioned allowlisted fields shown above, never raw commands, arguments, content, environment or header values. This synthetic example does not explain the historical incident.

For an offline P7 checklist, compare each source, server and host against the preview, ownership evidence and any sanitized event report. Mark versions and live connections unverified unless measured. Keep uncertain or unselected items unchanged. The follow-up test plan will capture real events and measure startup and retry bounds in isolation. It does not authorize live recovery.
