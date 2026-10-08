<!-- translation_key: security -->

# ccync Security

## Supported versions and reporting

Security fixes apply exclusively to the latest maintained release. Older versions do not receive separate security patches. If you discover a vulnerability, submit a private report through GitHub Security Advisories for this repository. Do not file public GitHub issues for security vulnerabilities. Include the affected version, complete reproduction steps, potential security impact, and any identified mitigations. Maintainers will coordinate verification and responsible disclosure. The project does not commit to fixed turnaround or remediation windows.

## Protected assets and roles

ccync protects user configuration, MCP secrets, plugin repository sources, locked versions, ownership records, and the native configuration files read by coding agents. External source repositories, the local operator, the ccync binary, agent hosts, and third-party plugins operate as distinct trust roles. Fetching source content does not permit automatic script execution. It also does not authorize ccync to modify or delete arbitrary files outside managed boundaries.

## Trust boundaries

- `~/.ccync/config.json` and `plugins.json` serve as trusted user input. `cache/` and `build/` contain derived data that ccync can reconstruct at any time.
- Raw MCP definitions and `${VAR}` variable placeholders remain untrusted until validated through safe resolution. Recovery journals store only before-and-after file fingerprints, never resolved plaintext credentials.
- Each agent's live configuration file is a shared local resource. ccync merges only the entries it manages and never removes existing settings without unambiguous ownership proof.
- Release artifacts must verify against SHA-256 checksums, match the target platform architecture, and fulfill the static CRT linkage requirement. When a package manager installs the ccync executable, ccync disables binary self-update to preserve package manager integrity.

## Attack surfaces and security invariants

### Release publication boundary

The target publication controller is private GitHub. The public repository accepts curated snapshots only through the guarded publication path; GitLab is a backup whose publication writer must be disabled. The controller contract checks the exact public destination, refuses existing or unqueryable tags, and publishes the snapshot branch and matching tag atomically. Its public push credential is supplied only to the final publish step through an ephemeral askpass mechanism. Hosted configuration and release success must be reported only when supported by run and ref evidence.

Untrusted inputs can enter through Git repository checkouts, archive extractions, repository subpath selections, plugin manifests, MCP commands, environment variables, symlinks or directory junctions, configuration parsing, and backup restores. All target file operations must strictly remain within their designated directory roots. When ccync encounters an ambiguous or invalid state, it fails closed and halts the operation.

Every file modification must execute through a sibling temporary-file rename or an atomic directory swap of a staged tree. Deleting an existing file requires explicit proof that ccync owns the target artifact. When a projection target or MCP server encounters an error, the output must identify that specific component. The system never treats unexecuted operations (`NotRun`) as successful.

See [MCP](docs/mcp.md), [Architecture](docs/architecture.md), and [Setup and diagnostics](docs/setup.md) for full security specifications.

[繁體中文](docs/i18n/zh-Hant/SECURITY.zh-Hant.md)
