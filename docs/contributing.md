# Contributing to ccync

Thank you for helping us build ccync, the cross-agent plugin, MCP, and skills manager!

This guide is intended specifically for **contributors** who want to develop and improve ccync itself. If you simply want to *use* ccync, please refer to the [README](../README.md) (which covers installation, adding your first plugin, and cross-agent synchronization) or the [Manual](manual.md).

## Prerequisites

Before you begin, ensure you have the following installed:

- [Rust](https://rustup.rs) (the stable toolchain, including `cargo` and `clippy`).
- `git`.

## Build and Verify

To build the project and run the verification suite, use the following commands:

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace
```

All three commands must execute successfully (without errors or warnings) before you open a merge request. 

> **Note:** There is one known test flake that is dependent on the machine's state. If you hit it, check `.dev/plans/` for any active plan tracking it, or the README Roadmap section for known follow-up work.

## Project Layout

Understanding the repository structure will help you navigate the codebase:

- `crates/` — Contains the six primary Rust crates. Refer to [`architecture.md`](architecture.md) for the dependency graph (DAG).
- `plugins/catalog.json` — The curated plugin catalog, which acts as the single source of truth for available plugins.
- `docs/` — Documentation for both users and maintainers.
- `.dev/` — Repository-local working state (such as plans and bug notes), which is primarily for machine-local tracking.

If you are unsure where to start, consult the [Maintainer Guide](devguide.md) to determine which crate owns the functionality you wish to modify.

## Conventions

Please adhere to the following conventions when contributing:

- **Rust Naming:** Follow the official [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/naming.html). For ccync-specific terminology and house style, consult [`naming.md`](naming.md).
- **Branding:** In all user-facing strings, refer to the product strictly as `ccync` (all lowercase).
- **State Management (source-of-truth vs live-surface boundary):** ccync's own state (config, lockfile, cache, canonical render) lives entirely under `~/.ccync/`, and that tree is where `backup`/`restore` operate. But ccync also *projects* that state onto each selected agent's live config surface outside `~/.ccync/` — `~/.claude/skills/...`, `~/.claude.json`, `~/.codex/config.toml`, `~/.copilot/mcp-config.json`, and equivalents for the other supported agents. Writing to those live surfaces is the entire point of "install once, project everywhere"; it is gated by the First-Run Overwrite Visibility Gate and tracked by `ManagedArtifactRegistry` so ccync only ever touches artifacts it created.
- **Error Handling:** Prefer returning `Result` types for expected failures. Avoid using `unwrap()` or `expect()` in production code paths.
- **Testing:** Keep unit tests enclosed within `mod tests`. Always isolate tests from the actual machine state by using the `tempfile` crate and injecting mock paths.

## Adding a Plugin to the Catalog

To add a new plugin to the catalog, append a `curated-upstream` / `git-clone` entry to `plugins/catalog.json` (and update a profile if applicable). You do not need to create a new code path; the existing catalog-resolution and `sync` pipelines handle git-clone sources generically.

You can validate your addition by running:

```sh
ccync sync --dry-run
```

## Merge Requests

When submitting a merge request (MR), please follow these guidelines:

- Keep MRs focused: one logical change per MR. Minimize the diff to only what is necessary.
- Include evidence of your verification. Provide the output from `cargo test` and `cargo clippy`, as well as the terminal output for any `ccync` commands if you altered their behavior.
- If your change affects a command or alters system behavior, update the corresponding documentation within the `docs/` folder.

## Where to Start

If you are looking for tasks to tackle, review the open issues and deferred work tracked in `.dev/plans/` (active plans) and the README Roadmap section.

- **Resolved:** **B-01** (making projection source-optional so installed plugins reach every agent) landed via the universal-installer work — see the README Roadmap section for the follow-up scope.
