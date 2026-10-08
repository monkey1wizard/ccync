---
type: guide
title: Contributing to ccync
description: Workspace verification, safe changes, documentation sync, and commits.
tags: [contributing, testing, rust]
status: stable
locale: en
translation_key: contributing
---

# Contributing to ccync

ccync is organized as a six-crate Rust workspace. Before making changes, locate the crate that owns the relevant subsystem. The foundational crate `ccync-foundation` must remain independent and never depend on upper-layer crates.

| Crate | Responsibility |
| --- | --- |
| `ccync-foundation` | Paths, configuration, and shared models. |
| `mcp` | MCP merge, writes, and recovery. |
| `projection` | Canonical output and agent-specific projections. |
| `ccync-engine` | Catalogs, adoption, installation, and command types. |
| `setup` | Health checks. |
| `ccync-cli` | Binary entry point and command handlers. |

## Local verification

```text
cargo test --workspace
cargo clippy --workspace
```

Run focused unit tests during development, followed by full workspace checks before committing changes. Review the repository's Rust conventions before modifying Rust source code. Limit code formatting to modified files so that extraneous whitespace changes do not obscure diffs. If a change impacts the release pipeline, run the packaging verification scripts on the appropriate platform. In pull requests and audit logs, mark unexecuted verification steps as `NotRun` rather than assuming they passed.

`crates/ccync-engine/tests/projection_vocabulary.rs` enforces terminology discipline by rejecting newly introduced retired projection terms while maintaining an allowlist of reviewed historical exceptions. Its allowlist matches by file path and token rather than line number. Do not widen the allowlist to mask terminology regressions; inspect the rationale for each existing exception before modifying the list.

ccync provides two internal developer commands that are omitted from public `--help`. The `ccync refresh` command rebuilds derived outputs from existing `~/.ccync/` state without re-executing source resolution. The `ccync rollback [--yes]` command is destructive: within the ccync source repository, it checks out an earlier stable tag, creates a backup branch, discards uncommitted changes and subsequent commits, and then executes `refresh`. Always inspect the displayed discard scope before passing `--yes`.

## Safety and test discipline

Writes to an agent's live configuration must preserve existing user content and execute atomically. Before introducing any file deletion logic, add automated tests covering positive ownership proof and same-name path collisions. When extending MCP configuration fields, verify that unknown fields in existing host configuration files survive merge operations, while unknown fields supplied by third-party plugins are logged and excluded. Verify secret redaction and write-ahead journal recovery after simulated process interruptions.

For external sources, add tests for directory traversal prevention, archive extraction boundaries, and graceful error recovery. Represent expected errors using the `Result` type, and reserve panics strictly for unrecoverable internal logic faults.

## Documentation and commits

### Release changes

The target authority is private GitHub for publication, with GitLab as a writer-disabled backup. Hosted cutover remains `NotRun` until run and ref evidence exists. Do not add GitLab release triggers or manual GitLab fallback instructions. Preserve the publication boundary, explicit ref policy, and S/P/D recovery ownership in the private runbook. Stable releases require a matching changelog section and base-version metadata in the same source commit. RC tags may use bare `-rc` or numbered `-rcN` and retain the base crate version; Homebrew and WinGet steps remain stable-only.

When modifying public commands or core system behaviors, update the corresponding topic documentation and synchronize the affected rows in `docs/structure/structure-map.ndjson`. Reference authoritative rules via markdown links rather than duplicating explanations across multiple files. Structure commit messages according to scoped Conventional Commits, and preserve unrelated user changes in the workspace. Any modification to a protected path requires a recorded architecture review before implementation.
