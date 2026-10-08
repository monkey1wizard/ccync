---
type: architecture
title: ccync architecture
description: System boundaries, six crates, projection flow, deployment, and quality goals.
tags: [ccync, architecture, arc42]
status: stable
locale: en
translation_key: architecture
---

# ccync Architecture

## 1. Introduction and Goals

ccync is a cross-agent configuration manager compiled as a single Rust binary. It unifies plugins, skills, custom commands, custom agents, and MCP servers into a single source of truth under `~/.ccync/`. Rather than forcing agents to use identical configuration schemes, ccync compiles a shared canonical render tree and projects it into each agent's native configuration format. It does not replace native agents or assume that their underlying configuration formats are interchangeable.

Key design goals include:

- Install or adopt once, then project configurations to Claude Code, OpenAI Codex, GitHub Copilot, OpenCode, and Google Antigravity.
- Protect existing user files. ccync removes an item only when it possesses unambiguous proof of ownership.
- Enforce atomic file writes, staged directory swaps, and recoverable write-ahead transactions across MCP hosts so that unexpected interruptions never leave corrupt state.
- Guarantee idempotency so that reapplying identical inputs yields a stable state. When an operation fails, diagnostic outputs explicitly identify the affected target or server.

Users select from five supported agents: `claude`, `codex`, `copilot`, `opencode`, and `agy`. The first four agents map directly to one projection target each. During projection, `agy` expands into `agy-cli`, `agy-ide`, and `agy-gui`, yielding seven projection targets in total. MCP configuration always updates five fixed host files: Claude, Codex, Copilot, OpenCode, and Antigravity, regardless of which agents are selected for projection.

## 2. Constraints

- The system compiles into a single executable across six workspace crates, without external databases or resident daemon processes.
- `~/.ccync/` establishes the boundary for ccync's local state. Live configuration files read by agents reside outside this boundary. Writes to these shared files must preserve configurations owned by users or other tools.
- Native agent formats are not interchangeable. While shared components originate from the canonical render tree, each target agent requires a dedicated native adapter.
- `plugins/catalog.json` is embedded into the binary at build time. User-managed personal sources reside in `~/.ccync/plugins.json`. These catalogs maintain distinct lifecycles and validation requirements.
- Production projection operates through a single unified pipeline rather than separate development and normal execution paths. While `config.json` preserves certain legacy compatibility fields, the engine ignores them during production projection.
- This document defines system architecture, canonical rendering, native projection, and ownership boundaries. See [Setup and diagnostics](setup.md) and [Commands](commands.md) for operational details, and [MCP](mcp.md) for secret boundaries.

## 3. Context and Scope

User intent, external source repositories, local ccync state, and live agent configuration files occupy distinct trust boundaries:

```text
User intent
  |  config.json / plugins.json / roster
  v
ccync source boundary ---- curated catalog / git / archive / local path
  |  resolve + fetch + validate
  v
~/.ccync/ machine state ---- cache / lock / canonical render / ownership proof
  |  target adapters + MCP serializers
  v
Agent-native surfaces ---- users and other tools also own existing configuration
```

The source boundary accepts the built-in catalog, personal plugins, adopted master agent configurations, Git URLs, local paths, archives, and catalog IDs. Before caching an external source, ccync validates path containment, locates the plugin root, and classifies supported component types. Plugin-defined MCP configurations undergo separate schema validation: unknown fields trigger warnings and are excluded, and helper execution fields such as `headersHelper` are rejected.

Live agent configuration files represent shared local resources. ccync never claims ownership of entire files such as `~/.claude.json` or `~/.codex/config.toml`, nor of a user's home directory. ccync manages exclusively those entries substantiated by registry records, demarcated marker blocks, or per-host state files. Projection, item withdrawal, uninstallation, and failure recovery all enforce this boundary.

## 4. Solution Strategy

ccync compiles external sources into a unified canonical render tree before projecting components into native agent formats:

1. Read the embedded catalog, machine configuration, personal plugin list, and adoption snapshot to generate or update `build/lock.json`.
2. Fetch source repositories into `cache/<id>@<revision>/` indexed by pinned commit SHA or content hash. Subsequent projection reads directly from the cache rather than contacting remote networks.
3. Assemble ccync-managed components in a staging directory adjacent to the active canonical render tree.
4. Once staging finishes successfully, execute `ccync_foundation::platform::atomic_swap` to replace `build/render/`. If the swap encounters an error, preserve the existing directory.
5. Dispatch target adapters to project skills, commands, custom agents, hooks, and managed instruction blocks to native agent surfaces.
6. Plan MCP configurations across the five supported hosts, recording a write-ahead transaction and verifying file fingerprints after writing.
7. Aggregate execution results using `ProjectionRunAccumulator` to classify each target and phase as `NotRun`, `Applied`, `Unchanged`, `Skipped`, or `Failed`. Aggregated success counters never replace granular per-target status reports.

Rebuildable render outputs, shared live configuration files, and ownership records remain strictly separated. An operational failure on one target remains visible even when other targets succeed. If deletion lacks conclusive ownership proof, ccync preserves the existing file.

## 5. Building Block View

| Crate | Responsibility | Direct internal dependencies |
| --- | --- | --- |
| `ccync-foundation` | Paths, configuration, platform, health, target types, and shared data models | None |
| `mcp` | Variable resolution, planning, transactions, five host serializers, and atomic writers | `ccync-foundation` |
| `projection` | Canonical component projection, target adapters, managed artifact registry, and Antigravity faces | `ccync-foundation` |
| `ccync-engine` | Catalog, fetch and install, adoption, upgrade, roster, doctor, and command and exit models | `ccync-foundation`, `projection`, `mcp` |
| `setup` | Management health checks aggregated by `doctor` | `ccync-foundation` |
| `ccync-cli` | Binary entry point, argument classification, and command orchestration | The other five crates |

The workspace `Cargo.toml` and each crate's module definitions establish the authoritative dependency graph. `ccync-foundation` contains shared primitives without dependencies on upper layers. `ccync-cli` coordinates binary entry points and command execution. `mcp` manages MCP variable resolution, serialization, and host writes. Separately, `projection` projects skills, commands, custom agents, and instruction blocks.

ccync provides 19 public CLI commands as documented in [Commands](commands.md). Internal development commands such as `refresh` and `rollback` remain callable in source builds but are omitted from public `--help`. Source code defines these sets through `CommandKind::PUBLIC` and `CommandKind::ALL`.

## 6. Runtime View

### Sync and projection

```text
(start) ccync sync
  │
[ migrate layout; load config ]
  │
{ --dry-run? }
  ├── yes ──▶ [ resolve catalog; show preview ] ──▶ (end >>|) no projection
  │
  no
  ▼
{ initialized machine and valid target selection? }
  ├── no ──▶ (end ×) exit error before projection
  │
  yes
  ▼
[ refresh catalog lockfile (best effort) ]
  │
{ first-run gate allows projection? }
  ├── no ──▶ (end ×) projection declined
  │
  yes
  ▼
[ read prior MCP manifest and live host entries ]
  │
[ recover or bootstrap MCP ownership before render ]
  │
{ MCP preparation succeeded? }
  ├── no ──▶ (end ×) exit error before render
  │
  yes
  ▼
[ stage and atomically swap canonical root ]
  │
[ project canonical targets; build managed MCP manifest ]
  │
[ plan MCP changes; journal, write, verify, commit ownership ]
  │
{ MCP projection returned an error? }
  ├── yes ──▶ (end ×) exit error
  │
  no
  ▼
[ project machine surfaces ]
  │
[ project managed instruction block ]
  │
[ report target evidence and MCP host failures ]
  │
{ target Failed/NotRun, or run/host/block failure? }
  ├── yes ──▶ (end ×) exit error
  └── no ──▶ (end √) exit success
```

ccync tracks separate canonical rendering and machine projection phases. Claude, Codex, and Copilot require verified completion across both phases. `agy-cli` and `agy-ide` require only the canonical phase. OpenCode and `agy-gui` require only the machine projection phase. Without positive completion evidence for each required phase, a target remains marked as `NotRun`. The absence of an explicit error does not indicate success.

`ProjectionEngine` builds the staged canonical render tree in a temporary sibling directory. Only after all components are fully assembled does it atomically swap the directory into `~/.ccync/build/render/`. If an interruption occurs, the existing render directory remains intact. Whether a plugin contributes projectable artifacts depends on its component types and target adapter support, not merely its presence in the catalog.

`ManagedArtifactRegistry` tracks every individual artifact projected by ccync. When an existing filesystem path lacks unambiguous ownership records, ccync preserves the file and reports `preserved user-owned path`. Running `ccync sync --dry-run` displays planned filesystem changes without creating, modifying, or deleting physical files.

### MCP ownership and recovery

MCP updates utilize a write-ahead transaction log. The journal stores only before-and-after file fingerprints or an `absent` marker. It never records resolved commands, environment variables, headers, or tokens. After writing each host configuration file, ccync re-reads the file to verify its new fingerprint. Only after all participating hosts produce verifiable fingerprints does ccync commit ownership records to `projected-state.json` and clear the journal.

Before executing a projection, `prepare_projection_recovery` checks whether a prior run was interrupted. If a host file matches the expected after-fingerprint, recovery confirms ccync ownership. If the file matches the before-fingerprint, recovery leaves it unchanged. If a host file exhibits an unknown third fingerprint or cannot be read, ccync halts automatic recovery and reports the conflict. If a single server definition contains an unresolved secret placeholder, ccync skips that specific server and names it in the output. Other MCP servers and non-MCP components continue processing normally.

### External tool lifecycle

The tool manager treats configured package sources and local machine state as separate trust boundaries. It validates source identity and capabilities before execution. It invokes package managers through typed operations and reports results without exposing credentials.

The manager acquires locks in this order: global manager lock, then per-tool lock. Every lock uses a persistent lock file. The manager releases the lock but does not remove the file, so concurrent processes always coordinate through the same path.

The lifecycle journal records typed stages for an operation: intent, execution started, and result recorded. On restart, recovery inspects the last durable stage and reconciles observed tool state before it retries or reports an unknown outcome. The journal stores a typed, secret-free retry intent. It must never store raw argv, credentials, or resolved secrets.

If a process stops after an external command may have run but before ccync records its result, the manager returns `manager-outcome-unknown`. It does not report success or repeat a non-idempotent operation without reconciliation.

The manager deletes or replaces an installed item only when it has positive snapshot ownership evidence for that item. A matching name or current package-manager listing alone does not prove ownership.

Status keeps local installation evidence separate from the remote update observation. Offline and local modes do not perform network checks. They preserve the prior remote result and timestamp as stale. Online mode re-probes local state, then replaces the remote result with exactly one typed outcome: `Current`, `UpdateAvailable`, `Unsupported`, `NotChecked`, or `CheckFailed`. It marks the result fresh and updates `last_check` after a completed or failed attempt. A fresh result never inherits an earlier `UpdateAvailable`. `outdated` uses this same status path. A fresh available update or actionable local drift returns exit code `3`; a remote check failure returns `1`; no known action returns `0`.

Discovery and command-based tracking share an exact executable selector. The selector accepts a command name or path and does not infer a package from a fuzzy name match. No candidate, multiple candidates, and probe errors remain distinct outcomes. The engine reconstructs Cargo and interpreter-pip adapters from validated tracked identity, including executable, source, interpreter, distribution, delivery, and manager-takeover data. Other adapter kinds fail closed when recorded reconstruction is unsupported. Remote adapters return a narrow typed result. They cannot claim persisted or unsupported observations as if a remote probe had established them. Cargo checks require safe credential-free HTTPS Git identity with a recorded commit fragment, or canonical crates.io identity and exact package/version evidence. Interpreter-pip checks use the recorded interpreter and normalized distribution.

A new upgrade is reachable only after a fresh remote result proves `UpdateAvailable`. `Current`, `Unsupported`, `NotChecked`, `CheckFailed`, stale evidence, ambiguity, or missing identity cannot authorize a package-manager mutation. The lifecycle journal records typed intent, execution start, and result without raw arguments, credentials, or resolved secrets. Recovery inspects the durable operation and observed state. It resumes only a valid same-tool journal operation. Unknown or conflicting state fails closed and requires inspection instead of automatic retry.

Tool state v2 tracks generation ownership independently of the current package-manager listing. Cleanup records a resumable typed plan and immutable proven inventory. It validates generation proof before replacing or deleting an artifact and recognizes already committed operations during recovery. Unproved, mismatched, symlinked, or conflicting artifacts remain untouched. Cleanup completion and snapshot projection follow the journal's recorded recovery state so an interrupted action can continue without rediscovering deletion targets.

CLI output preserves typed outcomes in JSON and keeps diagnostics on stderr, leaving stdout for JSON or command data. Exit codes remain meaningful with JSON enabled: `0` for successful inspection or no known action, `3` for an update or actionable drift, `1` for operational or incomplete-probe failure, and `64` for invalid usage. A typed check failure is not converted to a successful current state.

Lifecycle capability gates are two-stage. A gate remains `Gated` until its isolated acceptance cells pass and the receipt records the evidence. A candidate or passing unit test alone does not authorize production mutation. Live install, upgrade, recovery, projection, or restoration still requires the operation's explicit authorization. `--yes` accepts only the displayed operation where supported; it does not broaden authorization. Missing authorization leaves live acceptance cells `NotRun` and keeps the capability closed.

The lifecycle capability levels are E0, E1, and E2. E0 supports inspection, E1 supports safe operations with recovery, and E2 supports the full managed lifecycle. A platform or package manager may claim only a level that its verified capabilities support.

| Capability gate | Authority for current state |
| --- | --- |
| Each platform and package-manager capability gate | The final acceptance receipt lists the current state. An unproven Phase B gate remains `gated`. |

This table is the single authority for gate-state reporting in this architecture. The acceptance receipt supplies the current state so this document does not duplicate a state list that can become stale.

### Managed instruction block

A dedicated pair of HTML comments delimits ccync's managed block within user-level `AGENTS.md` files. The writer preserves all content outside the markers and updates only the enclosed content atomically. If the target file does not exist, ccync creates it. If the boundary markers are missing or malformed, ccync reports the conflict and skips the file. Because all three Antigravity targets share `~/.gemini/AGENTS.md`, ccync updates that shared file exactly once per run.

### Backup and restore

A machine configuration backup bundles `config.json`, `plugins.json`, the lockfile snapshot required for adopted items, and user-level instruction files. By default, `backup` strips recognized credentials and substitutes sensitive MCP `env` and `headers` values with named `${CCYNC_SECRET_<SERVER>_<KEY>}` placeholders. If the command detects an unmodeled plaintext credential that cannot be safely redacted, ccync refuses to generate the backup archive. Running `restore --dry-run` reports matching, divergent, or unreadable items without writing to disk. Restoring a profile does not copy native agent configuration files. Following a restore, the operator must provision required environment variables, install the team roster or plugin sources, and run `sync`. A backup represents a portable configuration package rather than a full system image.

The adapter remote result is deliberately narrow: it reports `Current`, `UpdateAvailable`, `Unsupported`, or `NotChecked`. The engine maps an unsuccessful probe to `CheckFailed` and preserves the distinction from a successful negative result. Cargo proof matches the exact recorded ref and installed SHA, or the exact crates.io package identity and SemVer version. Interpreter-pip proof uses the recorded interpreter and normalized distribution. Cargo Git proof accepts a 7–40-hex installed fragment. No query selects exact remote `HEAD`; `branch` selects `refs/heads/<name>`; `tag` selects the peeled tag with lightweight fallback; a 40-hex `rev` is immutable. Movable refs require exactly one full SHA matching the installed prefix for `Current`. Canonical crates.io checks require the exact package name and greater SemVer precedence for `UpdateAvailable`. Remote full SHAs stay transient and are never persisted by `outdated`. The store reads state v1 records. Ownership migration imports only the legacy current snapshot whose recorded fingerprint and physical manifest match. Unproven historical generations remain unowned. State v2 atomically stores current state and historical generation ownership. Deletion requires exact operation ID, build-relative path, file manifest/digests, and directory inventory proof. Symlinks and Windows reparse points fail ownership checks. Unproven legacy history fails closed. Cleanup retries use an immutable subset and revalidate every path.

Recovery validates a pending same-tool journal before ordinary eligibility. A known manager success continues without rerunning the manager. An unknown manager outcome requires `--retry-manager-step` and a validated journal matching tool, operation, intent, and manifest. A committed operation is treated as complete. The two-stage capability gate stays closed until isolated acceptance and its receipt pass. Live install, update, recovery, projection, and restoration still require explicit authorization. The CLI outcome table and JSON field definitions are in [the command reference](commands.md).

## 7. Deployment View

ccync distributes as a standalone executable. The release pipeline produces artifacts for six platform targets: Windows x64 and arm64, macOS x64 and arm64, and Linux musl x64 and arm64. Each release target provides a standalone binary and a minimal archive containing only the executable and `LICENSE`. Windows MSVC builds link the CRT statically (`+crt-static`) so that clean systems without the Visual C++ Redistributable do not fail at startup with `0xC0000135`.

When a SemVer tag is pushed, GitHub Actions validates crate versions and the changelog, compiles `checksums.txt`, generates release manifests, WinGet manifests, and a Homebrew formula, and signs release artifacts using keyless cosign via GitHub Actions OIDC. Only stable releases publish to Homebrew and WinGet; preview releases do not enter those public package channels. Post-release acceptance tests verify checksum integrity, cosign signatures, Cargo compatibility, installation scripts, and Homebrew tap availability.

ccync operates without background services or daemons. It inspects and modifies local `~/.ccync/` state, invokes Git, curl, or package managers when needed, and updates local agent configuration files.

## 8. Crosscutting Concepts

### Ownership proof

Deleting an existing file requires stronger ownership verification than creating a new projection. An entry in the managed artifact registry, per-host MCP ownership state, Antigravity root ownership records, or a demarcated marker block can establish ccync ownership. A matching filename, similar content, or a standard path alone cannot prove ownership. When ownership proof is missing, ccync preserves the file. While this policy can leave orphaned artifacts, it prevents deleting private user data by mistake.

### Atomicity and failure isolation

Individual configuration files update via temporary sibling files followed by atomic renames. The canonical render tree is staged in an adjacent directory and swapped atomically into place. Because MCP configurations span multiple independent host files, a single filesystem transaction cannot encompass them all. Instead, a write-ahead journal, cryptographic fingerprint checks, and recovery classification guide these host files toward consistency. Diagnostic reports explicitly identify failing targets or servers, ensuring that partial failures cannot hide within aggregate totals.

### Secrets and placeholders

`config.json#secrets` stores local plaintext input. When updating configuration, ccync merges changes into the original JSON structure to preserve unrecognized extension fields. MCP variables support `${VAR}`, `${VAR:-default}`, `PLUGIN_ROOT`, `PLUGIN_DATA`, and controlled Claude aliases. Transaction journals never store resolved credentials. Machine backup archives replace recognized secrets with placeholders by default. These mechanisms prevent credentials from leaking into exports or persistent state, though they do not encrypt files on the local filesystem.

### Native adapters

A shared internal component model does not imply that every target agent supports identical features. Dedicated adapters exist for Claude Code, Codex, Copilot, OpenCode, and each of the three Antigravity targets. Adapters handle frontmatter normalization, directory path translation, symlink or copy selection, and graceful degradation for unsupported features. Each adapter reports execution status under its own target identifier. A unified internal model must never erase native platform differences.

## 9. Architecture Decisions

- [ADR 01: Product scope and projection model](adr/01-product-scope-and-projection-model.md)
- [ADR 02: Source resolution and plugin materialization](adr/02-source-resolution-and-plugin-materialization.md)
- [ADR 03: Projection ownership and atomic publication](adr/03-projection-ownership-and-atomic-publication.md)
- [ADR 04: MCP trust boundary and recovery](adr/04-mcp-trust-boundary-and-recovery.md)
- [ADR 05: Machine lifecycle, adoption, and CLI contract](adr/05-machine-lifecycle-adoption-and-cli-contract.md)
- [ADR 06: Backup profiles and secret handling](adr/06-backup-profiles-and-secret-handling.md)
- [ADR 07: Release and distribution](adr/07-release-and-distribution.md)

## 10. Quality Requirements

| Scenario | Trigger | Expected, verifiable result |
| --- | --- | --- |
| Canonical rendering fails partway through | Source tree cannot be read or staging fails | Live render remains byte-for-byte unchanged. No successful `Applied` block is emitted. |
| A live surface contains a same-named external item | Sync, remove, or uninstall reconciles it | Without positive ownership proof, ccync preserves the item and reports a warning or skipped evidence. |
| Writing the second MCP host fails | The first host was written | The transaction remains. The next run recovers by fingerprint and does not treat an unwritten host as owned. |
| A secret placeholder cannot be resolved | One server lacks a required variable | ccync skips and names that server, then continues other servers and projection. |
| The user repeats the same input | Lock, cache, and live configuration have converged | ccync does not rewrite a byte-identical host. The target result is `Unchanged`. |
| A backup moves to another machine | The profile has no secrets | Restore lists variables to reprovision and does not transfer literal secrets. |
| A release is installed on clean Windows | No Visual C++ redistributable is present | The static-CRT executable starts. A CI import probe catches regressions. |

Acceptance combines workspace tests, clippy, integration tests, and release pipeline checks. A check reported as `NotRun` is not a passing verification.

## 11. Risks and Technical Debt

- Several adoption paths continue to use standard file writes for lockfile updates. Only the source repair workflow currently utilizes the shared atomic writer.
- If historical ownership records are lost, ccync cannot determine whether older MCP entries were managed by ccync, and it preserves those entries indefinitely.
- `doctor --dry-run` and `--release-gate` accept their respective flags but do not yet alter the active check suite.
- If the local plugin cache is deleted, certain `upgrade` or `sync` paths can misinterpret a missing source directory as an empty plugin, potentially causing subsequent projections to remove active files incorrectly.
- If Antigravity falls back to physical directory copying, it does not automatically switch back to symbolic links when environment capabilities are restored. Its three targets also apply different conservative cleanup policies when deselected.
- Preview releases cannot fully exercise the WinGet submission path reserved for stable releases; end-to-end WinGet validation requires an official stable release.

See [Deferred work](deferred-work.md) for complete backlog tracking. This section covers risks directly relevant to architectural evaluation.

## 12. Glossary

| Term | Definition |
| --- | --- |
| agent | A user-selectable target product. There are five. `agy` expands to three targets during projection. |
| projection target | A target that can be projected and reported independently. There are seven. |
| MCP host | A host product that receives MCP server configuration. There are always five. |
| canonical render | Shared standard output at `~/.ccync/build/render/`, rebuildable from the lockfile and cache. |
| live surface | Native configuration files or directories read by agents and possibly changed by users or other tools. |
| ownership proof | Evidence that ccync must have before changing or deleting a specific agent artifact. |
| adoption | Bringing existing plugins, skills, or MCP definitions from the primary agent under ccync's management. It does not imply automatic upgrades. |
| personal plugin | A user-supplied plugin that ccync fetches, pins, upgrades, and projects. |
| projection transaction | A write-ahead record of before-and-after fingerprints during writes across MCP hosts. |
| machine profile | A backup and restore package for local inputs and adoption snapshots, not a full image of the machine or agent files. |
