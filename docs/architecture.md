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

Users select from five supported agents: `claude`, `codex`, `copilot`, `opencode`, and `agy`. The first four agents map directly to one projection target each. During projection, `agy` expands into `agy-cli`, `agy-ide`, and `agy-gui`, yielding seven projection targets in total. MCP routes are planned independently for five hosts: Claude, Codex, Copilot, OpenCode, and Antigravity, regardless of agent projection selection.

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

The source boundary accepts the built-in catalog, personal plugins, components imported from one of five source agents, Git URLs, local paths, archives, and catalog IDs. Before caching an external source, ccync validates path containment, locates the plugin root, and classifies supported component types. Plugin-defined MCP configurations undergo separate schema validation: unknown fields trigger warnings and are excluded, and helper execution fields such as `headersHelper` are rejected.

Live agent configuration files represent shared local resources. ccync never claims ownership of entire files such as `~/.claude.json` or `~/.codex/config.toml`, nor of a user's home directory. ccync manages exclusively those entries substantiated by registry records, demarcated marker blocks, or per-host state files. Projection, item withdrawal, uninstallation, and failure recovery all enforce this boundary.

## 4. Solution Strategy

ccync compiles external sources into a unified canonical render tree before projecting components into native agent formats:

1. Read the embedded catalog, machine configuration, personal plugin list, and adoption snapshot to generate or update `build/lock.json`.
2. Fetch source repositories into `cache/<id>@<revision>/` indexed by pinned commit SHA or content hash. Subsequent projection reads directly from the cache rather than contacting remote networks.
3. Assemble ccync-managed components in a staging directory adjacent to the active canonical render tree.
4. Once staging finishes successfully, execute `ccync_foundation::platform::atomic_swap` to replace `build/render/`. If the swap encounters an error, preserve the existing directory.
5. Dispatch target adapters to project skills, commands, custom agents, and managed instruction blocks to native agent surfaces. For Claude Code and Codex, a plugin group whose native adapter is verified activates as one native package. Every other route stays loose or component-based. Hooks have no supported route on any target, so each target reports them as unsupported.
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
[ cut over verified native plugin groups (Claude Code, Codex): write private replacements, install, then retire loose copies ]
  │
[ publish loose direct routes, then retire a combined surface only for a target whose direct publication succeeded ]
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

The personal input is `~/.agents/mcp.json`, with required `ccyncMcpServers` and optional `hostRoutes`. A valid legacy input remains usable until the user switches it manually. Concurrent or foreign inputs fail closed. The first-wins declaration merge retains source identity. Imported originals are distinct from destination copies, and a matching name alone does not grant ownership. See [MCP](mcp.md) and [ADR 04](adr/04-mcp-trust-boundary-and-recovery.md).

The managed manifest is derived, not proof of physical ownership. `S/build/mcp/projected-state.json` records committed per-host proof and migrated route intent. The write-ahead `S/build/mcp/projection.txn.json` records fingerprints and transaction metadata without resolved secrets. Each native host is checked independently. Conflicts retain that host and its evidence while safe hosts may complete. A matching after fingerprint confirms a landed write, a before fingerprint preserves prior state, and an unreadable entry or third fingerprint stops recovery with the journal retained. Deletion requires committed ownership on that host.

The restore pending marker is separate from the host journal and holds profile and before/after fingerprints. Other mutations stop while it exists. Restore resumes only with the same backup and refuses divergent destinations. Preview and diagnosis remain read-only. Windows stdio is blocked while the support table is empty. Synthetic evidence supports attribution only within its observed scope, not a claim of real host behavior.

### External tool lifecycle

The lifecycle result envelope uses `schemaVersion: 1`. The exact executable selector honors `PATHEXT` case-insensitively on Windows. See [Commands](commands.md) for the complete result fields and selector rules.

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

The current writer updates marker-delimited blocks in four user-scope `AGENTS.md` files: Codex, Copilot, OpenCode, and Gemini. Six logical targets map to those four paths because the three Antigravity targets share `~/.gemini/AGENTS.md`. The writer preserves bytes outside the markers and updates each block atomically. Missing files are created. Damaged markers are reported and skipped. Claude has no current write path, so `~/.ccync/AGENTS.md` does not reach Claude's user scope. There is no `~/.claude/AGENTS.md` destination. After a successful source read, ordinary sync prunes the legacy `~/.claude/CLAUDE.md` block only when `claude` is selected. The prune applies whether the source is absent or nonempty. Ordinary sync uses the configured target selection. `ccync doctor` classifies the legacy Claude path independently of selection, config, and source health. `ccync uninstall` checks all instruction paths regardless of selection. The detailed contract is in [Instruction sync](commands.md#instruction-sync), with ownership and recovery in [User-scope instructions](setup.md#user-scope-instructions).

Claude Code's instruction loader is separate from ccync's writer. If Claude Code is version 2.1.277 or later, it supports a built-in `AGENTS.md` plugin. Some sessions may lack support before version 2.1.281. The plugin must be enabled. Use `/plugin` to manage its enabled state, and set Project instructions to a suitable file-selection mode. In the default `claude-md-or-agents-md` mode, repository `AGENTS.md` loading is suppressed when the working directory or an ancestor contains `CLAUDE.md`, `.claude/CLAUDE.md`, or `CLAUDE.local.md`. Users can choose `claude-md-and-agents-md` to load both. ccync changes no setting, creates no bridge, import, or symlink, and cannot guarantee that Claude Code loads an instruction file. See the [Claude Code memory documentation](https://code.claude.com/docs/en/memory).

`user_scope_instruction_write_path` resolves current destinations. `legacy_claude_user_instruction_path` resolves only Claude's legacy ccync destination. The lifecycle collects these paths separately, so the legacy destination stays out of the current-write list. Legacy cleanup passes `None` to the existing `projection::managed_block::write_block`, which validates the marker pair and preserves the surrounding bytes. A source read error remains an error. Treating it as an absent source could prune valid instruction blocks.

### Backup and restore

Backups carry portable declarations and route intent, not host ownership. Backup refuses direct MCP input containing raw credentials, URL userinfo or unclassifiable values before creating output. `restore --dry-run` is read-only. New-format `mcp.json` retains its bytes; old-format backups convert the root key. Restore copies neither native host files nor ownership, and conflicting explicit routes stop the restore. Provision machine secrets and preview sync on the destination.

The adapter remote result is deliberately narrow: it reports `Current`, `UpdateAvailable`, `Unsupported`, or `NotChecked`. The engine maps an unsuccessful probe to `CheckFailed` and preserves the distinction from a successful negative result. Cargo proof matches the exact recorded ref and installed SHA, or the exact crates.io package identity and SemVer version. Interpreter-pip proof uses the recorded interpreter and normalized distribution. Cargo Git proof accepts a 7–40-hex installed fragment. No query selects exact remote `HEAD`; `branch` selects `refs/heads/<name>`; `tag` selects the peeled tag with lightweight fallback; a 40-hex `rev` is immutable. Movable refs require exactly one full SHA matching the installed prefix for `Current`. Canonical crates.io checks require the exact package name and greater SemVer precedence for `UpdateAvailable`. Remote full SHAs stay transient and are never persisted by `outdated`. The store reads state v1 records. Ownership migration imports only the legacy current snapshot whose recorded fingerprint and physical manifest match. Unproven historical generations remain unowned. State v2 atomically stores current state and historical generation ownership. Deletion requires exact operation ID, build-relative path, file manifest/digests, and directory inventory proof. Symlinks and Windows reparse points fail ownership checks. Unproven legacy history fails closed. Cleanup retries use an immutable subset and revalidate every path.

Recovery validates a pending same-tool journal before ordinary eligibility. A known manager success continues without rerunning the manager. An unknown manager outcome requires `--retry-manager-step` and a validated journal matching tool, operation, intent, and manifest. A committed operation is treated as complete. The two-stage capability gate stays closed until isolated acceptance and its receipt pass. Live install, update, recovery, projection, and restoration still require explicit authorization. The CLI outcome table and JSON field definitions are in [the command reference](commands.md).

### Provider identity, loading, and recovery

Provider packages preserve source identity through rendering and target publication. Immutable source generations live under `~/.ccync/build/render/providers/<source-key>/<generation>/`; target-specific package views are derived from these generations for publication. The source key comes from resolved source identity, not its display name. Component records retain `source`, original `plugin_name`, optional `upstream_revision`, `kind`, and upstream `component_id`. Native ownership records add the generated package digest, native selector, and verified capability inventory. The lockfile stores committed records in `_providerLifecycle`; pending changes use `~/.ccync/build/projection.txn.json`.

**Identity.** A provider has a canonical package name and a ccync management ID. The canonical name is the name the native client lists and uses in its namespace. The management ID is the storage key that `list`, `show`, `upgrade`, and `remove` accept. Existing storage keys and pins stay unchanged. `authoritative_plugin_name` in `crates/ccync-engine/src/plugin_render.rs` picks the canonical name from the first target manifest that declares one, then the explicit catalog `packageName`, then the front-matter name of a bare skill. A generic basename such as `plugin` or `skills` is not evidence, so the storage ID stays. When a manifest name and a catalog name disagree, that provider is blocked with an error. The `show` command prints `Plugin: <package> | management ID: <id>`, and the management ID remains the selector the command accepts.

**Source-form namespaces.** Native plugin sources retain their declared package identity and native package route on supported Claude Code and Codex adapters. A standalone skill or already materialized component has no native plugin package identity to preserve. Its published identity comes from its source and component names, without a manager-added `ccync:` namespace. Provider-qualified aliases resolve collisions between different sources deterministically. Distinct sources that still collide in a native namespace are reported as conflicts; ccync does not silently merge or discard either source.

Native targets keep their native provider and component namespaces. Flat adapters retain a unique upstream invocation name; collisions use a deterministic provider-qualified alias, with a digest suffix when normalization or truncation would collide. The alias record includes target, full component identity, and published alias. Previously assigned aliases remain reserved across removal and re-addition. Alias lengths respect target limits. Source keys, generation IDs, and aliases are also validated as portable path segments: at most 128 bytes, with path separators, reserved characters, and Windows device names rejected. Generated references are rewritten only when their identity is unambiguous. Opaque upstream text is not rewritten, and ambiguous generated references fail with a diagnostic. A native namespace collision between distinct sources is a conflict; ccync does not drop either provider.

Provider package views exclude active native MCP entrypoints so the existing centralized MCP projection remains the single host configuration path. Source generations retain MCP declarations for that projection. Hook helper resources remain in provider packages. A target view that cannot safely remove or interpret an MCP declaration, or that lacks a required capability, fails for that target rather than silently losing behavior.

Native ownership is established only by a committed ccync record matching the exact selector, source identity, upstream revision, generated digest, and required capabilities. Names and matching content do not prove ownership. Observed state is the registration and digest read from the target now. Desired state is the generation requested by current sources. If observation matches the committed old generation while desired content has changed, this is an owned upgrade. A mismatch against the committed generation is drift or conflict. Foreign, disabled, broken, unreadable, or unverifiable registrations block mutation and remain untouched.

The pending intent records target, selector and expected package-digest fingerprints, scoped live paths, optional saved preimages, previous and desired generations, client scope, source and view digests, enablement intent, and phase. ccync writes it to `~/.ccync/build/projection.txn.json` under the provider lifecycle lock before the native install or removal it describes. Production code writes two phases: `publishing` for a native install or switch, and `pruning` for an uninstall removal. In the reviewed code, `staged` and `verifying` appear only in tests, and `reloading` is a reserved variant with no writer and no loader reload check. Provider rollout validates source and observed state, stages replacement packages, writes this intent, publishes or installs, then rereads the native registration and checks capabilities. It commits ownership only after validation. Old paths are pruned only after every selected consumer has a valid replacement. Recovery observes the recorded selector and digest before retrying; it never blindly repeats install or removal. On failure it restores a proven working generation or reports a recoverable partial state while retaining the remaining loader and ownership evidence. Teardown removes only exact committed ccync-owned selectors. If uninstall cannot read the committed records or the pending intent, it keeps every provider generation and reports a warning instead of pruning.

**Provisional proof and recovery.** A first install or migration has no committed row, so the install branch in `delivery_apply.rs` depends on the pending intent. An existing registration is adopted and committed only when the pending intent names the same selector and operation, and a fresh observation of the installed view matches the view digest the intent recorded (`classify_adopted_view`). A same-name registration without that proof stays a conflict. When an install attempt fails, the earlier intent is restored only when the attempt describes a different operation and no registration-changing CLI call succeeded in it. A same-operation attempt supersedes the earlier intent. After a successful registration change, the attempt's intent stays journaled, and the next run reconciles the observed registration. A foreign drift or an ambiguous intent blocks recovery: the intent and its preimages stay on disk, and ccync reports the conflict.

**Per-client routes.** `native_plugin_capable` in `crates/ccync-engine/src/delivery.rs` is true only for Claude Code and Codex. A native plugin group also has to pass its adapter check (`native_adapter_states`), which allows mutation only when the CLI's `major.minor` version is on the line its live probe verified. Any other or unreadable version keeps the loose route, with `unverified adapter version <version>` or `unreadable adapter version` as the reason. The routes are:

| Target | Route | Native status |
| --- | --- | --- |
| `claude` (Claude Code) | Native plugin package, activated per plugin group through `NativeExecutable::Claude`. | Eligible only on the Claude Code 2.1 line. Any other or unreadable version keeps the loose route. |
| `codex` | Native plugin package, activated per plugin group through `NativeExecutable::Codex`. | Eligible only on the Codex CLI 0.169 line. Any other or unreadable version keeps the loose route. |
| `copilot` (CLI and VS Code bridge) | Loose skill and component routes in shared and private roots. | No native adapter; loose route only. The isolated Copilot CLI listing probe saw one identity. |
| `agy-cli`, `agy-ide`, `agy-gui` | Loose skill and agent routes in the AGY roots. | No native adapter; loose route only. The AGY CLI probe is NotRun, and IDE and GUI are unverified. |
| `opencode` | Component routes. Skills also have a private replacement root at `~/.config/opencode/skills`. | No plugin route in this code. |

Claude Desktop is not Claude Code, and this document makes no claim about Claude Desktop.

**Group switching.** A native group is every route of one plugin on one native-capable target. `plan_native_routes` marks it `Native` only when its adapter is ready. The group works as one unit: if one member fails, the whole group falls back. A group is blocked while a shared copy it would load has a reader that would lose the component. `SHARED_ROOT_CONSUMERS` lists every known reader, selected or not, and each reader counts the same way. A reader keeps the component in three cases. It activates the component natively in the same run. Another shared root it discovers keeps the copy, because no native group retires that copy. It has the component in its private root, such as Copilot's `~/.copilot/skills`, or it is OpenCode, whose replacement root is `~/.config/opencode/skills`. Only a skill copy covers a reader, so a command converted to a skill in a shared root still blocks its group. Every group starts as native. The first blocked group in order falls back, and the check repeats until no native group is blocked. Before any native change, ccync publishes each covering copy and checks that its content matches the skill source in the rendered generation. A provider that is a single skill, with `SKILL.md` at its source root, publishes that root as the skill directory. A loose copy is removed only after `retire_shared_path` proves ownership and accounts for every reader. If a group's activation fails, `NativeRouteStatus::Fallback` records the reason. A group with no committed native record keeps its loose route. A group with a committed native record keeps its prior native route, or its legacy native route under an older selector, and its loose routes are not written. A fallback is never reported as native success.

**Legacy copies and references.** At the start of each real sync, ccync checks each selected target's native skill root for skill directories that an older ccync left without ownership records. If such a directory is not a link and its content matches the current source of the same-named skill exactly, ccync moves it to `~/.ccync/backups/legacy-copies/<unix-seconds>/` and reports the move. The same check covers agent files in each selected target's native agent root. When ccync publishes an agent under an alias, the file at the agent's plain name is moved if it is a regular file, the direct agent writer has no record of it, and its bytes equal the aliased copy in that root. Such a file is either unowned, as an older ccync left it, or recorded only by the plain-name agent writer. An aliased agent route also reserves its plain-name file, so the plain-name writer never adds a second copy beside the aliased one. A plain-name file that only the plain-name writer records is also moved, without a byte match, once the direct writer records that agent under an alias in any selected agent root, because that writer no longer writes or records it. Any other unowned entry stays and is treated as foreign, so the provider component is published under an alias. Every unowned agent file that still holds a provider agent's plain name is reported in each sync, with the command that removes it. ccync does not remove it. Foreign entries only cause aliases. They never take part in resolving references inside agent or command bodies. A `/` starts a command reference only at the start of a body or after whitespace, `(`, `[`, a backtick, or a quote, so file paths and URLs keep their names. A blocked component does not keep the combined surface when its plugin group activates natively on that target and the native loader reads that kind. Claude Code reads skills, agents, and commands from a plugin package. For Codex, ccync relies on skills only.

**Legacy selector migration.** When a committed ccync registration for the same target, provider source and plugin uses an older selector, the new install and the removal of the old registration share one pending intent. The old registration is removed only after the new one verifies, using fresh proof, and then ownership commits. The gate requires the same provider source key, the same upstream revision, an older selector of the form `<name>@ccync-<name>`, a complete ownership proof, and a matching observed view digest. A disabled, unobservable, or unprovable legacy registration blocks the group, and both registrations stay. The gate does not compare the repository path, subpath, or manifest identity, because a committed record does not store them.

**No global atomicity.** ccync cannot make a change across Claude Code, Codex, the filesystem, and running sessions as one transaction. The lifecycle lock and the journal order each of ccync's steps. A crash after a native side effect leaves an intent that recovery must reconcile first. Native CLIs may copy files, rewrite metadata, require a restart, or fail after a side effect, so ccync re-observes native state before every retry or deletion.

The provider lifecycle journal uses schema v3. It reads v1 and v2 journals when their newer intent collections are empty, then writes v3. V1 journals cannot contain standalone publication intents, and v1 or v2 journals cannot contain standalone withdrawal intents. Pending publication or withdrawal intents are retained until recovery proves the operation complete or safely abandons it. The writer does not downgrade a journal while any intent is pending, because an older schema cannot represent every pending operation. An older reader that does not support v3 rejects it rather than dropping intent data.

For standalone delivery, sync and doctor distinguish `standalone published`, `recovery blocked`, and `identity unresolved`. A blocked or unresolved operation retains its journal, ownership proof, and affected files as applicable. A retained or fallback route is reported separately from successful native activation. These CLI states describe filesystem publication and recovery evidence. They do not prove that an attached application or running agent loaded the component.

Each of the seven projection targets receives provider components on its own native discovery roots: skills in the target's skill root, agents in its agent root, and commands in its command root. A command on a target without a native command root is published as an equivalent command-skill in that target's skill root. ccync writes each component at most once per physical root, following the shared-root table below. The former combined surfaces are fallback-only: the `~/.claude/skills/ccync` link, the combined `ccync` marketplace entry, the Copilot plugin copy under `~/.copilot/installed-plugins/ccync-copilot`, and the AGY `plugins/ccync` roots for the CLI, IDE, and GUI faces. Discovery and capability support are target-specific. Missing CLI support, unsupported required features, or unverified discovery is a target failure or `NotRun`, not a successful managed load. Shared paths are retired only after all selected consumers have replacements; no universal shared root or umbrella plugin is assumed.

Current status: ordinary `ccync sync` publishes provider components through direct target surfaces by default. Each run plans the routes. Before the loose routes publish, it attempts native activation for each Claude Code and Codex plugin group whose adapter is ready. Activation writes any private replacement that a shared reader needs, installs the native registration, commits ownership, and then retires the loose copies the group replaces. A group whose adapter is not ready keeps its loose route with a reason. A group whose activation fails keeps its loose route unless it has a committed native record, in which case that native route stays and its loose routes are not written. The remaining routes publish through guarded replacement, and the combined `ccync` surface is retired for a target in the same sync once its direct publication succeeds. A target whose replacement fails keeps the combined writer as its per-target fallback. A sync that retires the Codex combined surface prints the manual Codex removal hint on that run only. A sync that deselects a target removes the direct skill and agent copies the lockfile recorded for it, unless a planned route still names the same path. `ccync uninstall` removes every direct copy the lockfile records, including the AGY skill and agent roots. The Copilot plugin copy and the three AGY `plugins/ccync` roots follow the same rule: ordinary sync removes each one that ccync's ownership record proves once its target's direct publication succeeds, writes none of them again on later syncs, and removes an owned one when its face is deselected. An unowned directory at one of those paths stays and is reported. A target with a component it cannot discover without a user step keeps its combined surface. A combined copy in a shared root is retired once the direct copy in that root was written.

The shared skill roots are read by more than one consumer. Each row below records the root, the consumers that discover it, the targets it serves, and the sources checked. The rows match `SHARED_ROOT_CONSUMERS` in `crates/ccync-engine/src/delivery.rs`.

| Shared root | Consumers that discover it | Targets served | Source URLs | Checked |
| --- | --- | --- | --- | --- |
| `~/.agents/skills` | Codex, Copilot, OpenCode | Codex, OpenCode | <https://learn.chatgpt.com/docs/build-skills>; <https://docs.github.com/en/copilot/concepts/agents/about-agent-skills>; <https://opencode.ai/docs/skills/> | 2026-10-16 |
| `~/.claude/skills` | Claude, OpenCode | Claude, OpenCode | <https://docs.anthropic.com/en/docs/claude-code/skills>; <https://opencode.ai/docs/skills/> | 2026-10-16 |
| `~/.gemini/config/skills` | AGY IDE, AGY GUI | AGY IDE, AGY GUI | <https://antigravity.google/docs/skills> | 2026-10-16 |
| `~/.gemini/config/agents` | AGY CLI, AGY GUI | AGY CLI, AGY GUI | <https://antigravity.google/docs/cli/commands/agents/> | 2026-10-16 |

Copilot CLI reaches `~/.agents/skills`, but Copilot is not served by that root because its VS Code bridge disables that location.

Live discovery probes on 2026-10-17 counted one provider entry each. The Claude probe reported `count=1` with Claude Code 2.1.296. The Codex probe reported `count=1` with codex-cli 0.163.0. A count shows that the CLI discovered an entry. It does not show that a running agent loaded the component. Filesystem and registration checks do not establish effective loading by the agent either.

Filesystem publication and registration checks establish only filesystem and native metadata state. They do not prove that a running agent loaded the component. Loader reload and effective-load checks are reported separately; unrun checks remain `NotRun`.

### Direct ownership and retirement

- **Writers.** Each physical skill root receives each component at most once in a run, even when several targets share that root. A target's selected route is its writer: the loose direct copy, or the native package of a group that activated. A combined copy remains only as the fallback for a target whose direct publication failed or is blocked.
- **Ownership rows.** Each row records one exact path under one kind, such as a direct skill, direct agent, direct command, or source link. Ownership exists only through these rows. A matching name or matching content neither creates a row nor proves ownership.
- **Migration.** Sync migrates the older layout before it loads configuration. A combined surface retires only after its replacement succeeds.
- **Shared-root retirement.** A combined copy in a shared root is removed only after every known reader of that root, selected or not, is accounted for. A reader that is not a ready native target must have a verified replacement; otherwise the copy stays. The copy also stays while any target that depends on it failed or is blocked. The shared-root table above lists the readers of each root.
- **Failure retention.** A failed target keeps its existing surface and fallback. Its failure stays in its own result even when other targets succeed. If removing a recorded copy fails, its row stays, and `build/lock.json` is kept so that a later run can retry.
- **Deselection and uninstall.** Deselecting a target removes the direct copies its rows record, unless a planned route still names the same path. Uninstall removes each recorded direct copy, then each recorded source link, then the authorized legacy children. Except for a source link that a user replaced, a row is forgotten only after its removal succeeds. A replaced source link stays in place, is reported, and its row is forgotten. Uninstall also removes committed native registrations, each only after a fresh observation proves ownership through its view digest. A registration that cannot be proven, or whose adapter is not verified, keeps its committed record and is reported. Deselection does not remove native registrations.
- **Source links.** A machine run records each source link it creates for the Copilot and Antigravity faces in `sourceLinkProjectionPaths`. Removing a source link never removes its target. Link rows survive a later run that does not select their targets.

### Native diagnostics and acceptance status

`ccync doctor` re-observes each committed native record with read-only listing commands (`NativeObservations::observe_committed` in `crates/ccync-engine/src/doctor.rs`) and never mutates state. A healthy CLI is listed once per committed record. A failed or timed-out listing is not repeated for that executable's remaining records within the same observation, and a CLI that cannot start reports `Unavailable`. The diagnostic states in `NativeState` (`crates/ccync-engine/src/managed_view.rs`), from least to most severe, are:

- **Registered, loader NotRun:** owned, enabled, and consistent with the committed record. Filesystem and registration checks do not prove that a running client loaded the component.
- **Deliberately disabled:** zero loads are expected, and the component is not missing.
- **Owned, older generation:** the registration matches an older committed generation.
- **Unavailable, NotRun** and **Unreadable, NotRun:** the CLI or its listing could not be read.
- **Absent:** no registration occupies the selector.
- **Broken:** the client reports that the registration cannot load.
- **Drifted from committed generation:** the registration no longer matches the committed record.
- **Foreign registration:** a registration that ccync does not own occupies the selector.

A fallback route shows its reason, and the loose route stays active. An unreadable or malformed `_providerLifecycle` section produces a warning that native route state is unknown. It never produces silence or full success (`native_section_findings`).

Acceptance status, as of 2026-10-22:

- Isolated-profile live probes ran on Codex CLI 0.163.0, Claude Code 2.1, GitHub Copilot CLI 1.0.101, and OpenCode 1.18.35. Each reported identity and enabled count 1, and disabled count 0. These probes do not show that a desktop, IDE, or App loaded the component.
- OpenCode 2.3.0 offers no command that lists loaded skills, so the probe checks delivery and withdrawal of `SKILL.md` at the isolated skill root and does not observe the loader count.
- The Copilot probe covers the CLI only. The VS Code bridge is not verified.
- The AGY CLI 1.4.4 live probe is NotRun. `agy -p /skills` starts a model turn instead of returning a CLI-handled listing, so the probe cannot read the listing. AGY IDE and GUI are not verified.
- The owner reported on 2026-10-24 that acceptance on real machines is complete for the Codex/ChatGPT and Claude apps, for OpenCode, Copilot CLI and VS Code, AGY IDE and GUI, and unselected shared-root readers. The evidence is not stored in this repository, and this document does not verify it. CLI listings and filesystem checks cannot establish attached UI loading, enabled or disabled counts in an account-bound interface, or migration on the owner's machine.
- Real-machine migration is also pending.

## 7. Deployment View

ccync distributes as a standalone executable. The release pipeline produces artifacts for six platform targets: Windows x64 and arm64, macOS x64 and arm64, and Linux musl x64 and arm64. Each release target provides a standalone binary and a minimal archive containing only the executable and `LICENSE`. Windows MSVC builds link the CRT statically (`+crt-static`) so that clean systems without the Visual C++ Redistributable do not fail at startup with `0xC0000135`.

Private GitHub is the release controller. Its guarded publication path pushes a curated snapshot of `main` and the release tag to the public repository in one atomic push. Release tags use `vX.Y.Z` for stable releases and `vX.Y.Z-rc` or `vX.Y.Z-rcN` for release candidates. When the tag arrives, public GitHub Actions validates that the tag base version matches the crate version, checks the changelog, compiles `checksums.txt`, generates release manifests, WinGet manifests, and a Homebrew formula, and signs release artifacts using keyless cosign via GitHub Actions OIDC. Only stable releases publish to Homebrew and WinGet; preview releases do not enter those public package channels. Post-release acceptance tests verify checksum integrity, cosign signatures, Cargo compatibility, installation scripts, and Homebrew tap availability.

ccync operates without background services or daemons. It inspects and modifies local `~/.ccync/` state, invokes Git, curl, or package managers when needed, and updates local agent configuration files.

## 8. Crosscutting Concepts

### State paths, live surfaces, and portable exports

ccync stores its data under four roots. When an environment variable is not set, ccync uses the default under the home directory. On Linux and macOS, an empty XDG variable also uses the default.

| Root | Windows | Linux and macOS | Contents |
| --- | --- | --- | --- |
| `P` portable | `%USERPROFILE%\.agents\ccync` | `~/.agents/ccync` | Derived exports and portable package snapshots. |
| `C` config | `%APPDATA%\ccync` | `$XDG_CONFIG_HOME/ccync`, default `~/.config/ccync` | User-owned configuration inputs. |
| `S` state | `%LOCALAPPDATA%\ccync\state` | `$XDG_STATE_HOME/ccync`, default `~/.local/state/ccync` | Private authority, build output, and locks. |
| `K` cache | `%LOCALAPPDATA%\ccync\cache` | `$XDG_CACHE_HOME/ccync`, default `~/.cache/ccync` | Machine cache. |

Production code resolves relocated state through the loaded `EffectiveLayout` from `ccync-foundation`. The legacy getter map is:

| Legacy accessor or path | Committed location or replacement | Use |
| --- | --- | --- |
| `machine_config_path` and `CcyncConfig::config_path` | `C/config.json` | Machine configuration accessor. |
| `mcp_input_path` | `~/.agents/mcp.json` | User-owned personal MCP input. |
| `ccync_agents_md_path`, `tools_json_path` | Same-named files under `C` | User-owned configuration inputs. |
| `personal_catalog_path` | `C/plugins.json` | Private source authority. `P/sources.json` is its derived export. |
| `plugins_lock_path` | `S/build/lock.json` | Private lock and ownership authority. `P/lock.json` is its derived export. |
| `ccync_build_root`, `provider_lifecycle_journal_path`, `generated_mcp_path`, and the tool state, artifact, and operation-journal accessors | `S/build` with the same relative paths | Private, persistent state and rebuildable outputs. |
| `ccync_cache_root` | `S/sources` | Persistent source cache. |
| `lifecycle_lock_path` | Fixed legacy lock path, plus `S/lifecycle.lock` | Coordination locks remain fixed at their documented locations. |

The legacy accessors remain for migration and legacy-authority behavior. New production code uses `EffectiveLayout`. The foundation `provider.rs` resolver may use fixed `PathRoot` values. The old `.join("build")` sublayouts in `migrate.rs` are migration inputs only. The production scan in `ccync-engine/tests/legacy_layout_scan.rs` enforces these boundaries.

Live surfaces stay at their registered locations until P7 relocates them. These include `~/.ccync/plugins/`, source links, `~/.ccync/build/render/`, `~/.ccync/build/active/`, `~/.ccync/data/`, and `~/.ccync/install-receipts/`. Copilot's `~/.copilot/ccync` link continues to point to `~/.ccync`. Native agent configuration files and directories remain managed through their live-surface accessors, including Claude and Codex MCP configuration, agent plugin state, skill roots, and user-scope instruction files. `install_receipt_path` and live-surface getters are not configuration-state accessors.

Portable `P/sources.json` and `P/lock.json` are exports derived from private authority. The writer emits them last. Routing and ownership read only private state, never these exports. Manual edits have no effect and a later export replaces them. Package snapshots exported through P5 follow the same rule: only admitted, portable content is included. Unclassified or credential-bearing data remains private.

### Ownership proof

Deleting an existing file requires stronger ownership verification than creating a new projection. An entry in the managed artifact registry, per-host MCP ownership state, Antigravity root ownership records, or a demarcated marker block can establish ccync ownership. A matching filename, similar content, or a standard path alone cannot prove ownership. When ownership proof is missing, ccync preserves the file. While this policy can leave orphaned artifacts, it prevents deleting private user data by mistake.

### Atomicity and failure isolation

Individual configuration files update via temporary sibling files followed by atomic renames. The canonical render tree is staged in an adjacent directory and swapped atomically into place. Because MCP configurations span multiple independent host files, a single filesystem transaction cannot encompass them all. Instead, a write-ahead journal, cryptographic fingerprint checks, and recovery classification guide these host files toward consistency. Diagnostic reports explicitly identify failing targets or servers, ensuring that partial failures cannot hide within aggregate totals.

### Secrets and placeholders

`config.json#secrets` stores local plaintext input. When updating configuration, ccync merges changes into the original JSON structure to preserve unrecognized extension fields. MCP variables support `${VAR}`, `${VAR:-default}`, `PLUGIN_ROOT`, `PLUGIN_DATA`, and controlled Claude aliases. Transaction journals never store resolved credentials. Backups refuse unsafe direct input and can redact supported adopted values. These mechanisms prevent credentials from leaking into exports or persistent state, though they do not encrypt files on the local filesystem.

### Native adapters

A shared internal component model does not imply that every target agent supports identical features. Dedicated adapters exist for Claude Code, Codex, Copilot, OpenCode, and each of the three Antigravity targets. Adapters handle frontmatter normalization, directory path translation, symlink or copy selection, and graceful degradation for unsupported features. Each adapter reports execution status under its own target identifier. A unified internal model must never erase native platform differences. Native plugin packages exist only for Claude Code and Codex; other targets keep loose or component routes.

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
- Native delivery has no atomicity across Claude Code, Codex, the filesystem, and running sessions. Recovery works per journaled intent.
- The OpenCode private skill copy is written before the native install. If the install fails, that copy is not rolled back.
- An older-generation pending intent with no saved preimages can survive a later commit. It is never cleared and keeps its provider generations.
- `ccync doctor` still observes native CLIs twice in one run: once for the route evidence lines and once for the native-routes check. A failed listing is not repeated within one observation, but the second observation remains a recorded follow-up.
- The sync success sentence does not yet account for native fallback. The fallback reason appears in the run warnings.
- When a legacy group is blocked, its shared skill copy stays in place, and Claude Code also reads that copy.
- After a crash between an ownership commit and clearing its intent, a legacy-migration intent with more than one fingerprint can remain journaled with its preimages. Ownership stays intact, and recovery is not blocked.
- Live desktop, IDE, and App loading is not verified for the reported clients. See Native diagnostics and acceptance status.

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
| management ID | The storage key and selector for a managed provider, accepted by `list`, `show`, `upgrade`, and `remove`. It is not always the native package name. |
| canonical package name | The name a native client lists for a provider package, taken from its authoritative manifest, catalog package name, or skill name. |
| native package | A single plugin package that a verified adapter generates and activates for Claude Code or Codex. Other targets do not use one. |
| loose route | A file-based projection written directly into a target's skill, agent, or command root. |
