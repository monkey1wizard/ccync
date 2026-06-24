# Contributing to ccync

Thanks for helping build ccync — the cross-agent plugin / MCP / skills manager.

This is the **contributor** guide (working on ccync itself). If you just want to *use* ccync, see
the [README](../README.md) (install → first plugin → cross-agent sync) and [manual.md](manual.md).

## Prerequisites

- [Rust](https://rustup.rs) (stable, with `cargo` + `clippy`).
- `git`.

## Build & verify

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace
```

All three should be green before you open a merge request. (One known machine-state-dependent test
flake is documented in `.dev/bugs/ccync-carve-deferred.md`.)

## Project layout

- `crates/` — the six Rust crates (see [architecture.md](architecture.md) for the DAG).
- `plugins/catalog.json` — the curated plugin catalog (single source).
- `docs/` — user + maintainer docs.
- `.dev/` — repo-local working state (plans, bug notes); mostly machine-local.

Start with the [maintainer guide](devguide.md) to find which crate owns the area you're changing.

## Conventions

- **Rust naming**: follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/naming.html);
  see [naming.md](naming.md) for ccync term/house style.
- **Branding**: user-facing strings say `ccync` (lowercase).
- **State**: ccync writes only under `~/.ccync/`; never read/write `~/.gal/`.
- **Errors**: prefer `Result` for expected failures; no `unwrap()` in production paths.
- **Tests**: keep unit tests in `mod tests`; isolate machine state with `tempfile` + injected paths.

## Adding a plugin to the catalog

Add a `curated-upstream` / `git-clone` entry to `plugins/catalog.json` (and a profile if relevant).
No new code path is needed — `resolve-catalog` / `install` handle git-clone sources generically.
Validate with `ccync resolve-catalog --dry-run`.

## Merge requests

- One logical change per MR; keep the diff focused.
- Include the verification you ran (`cargo test`/`clippy` output, and any `ccync` command output for
  behavior changes).
- Update the relevant doc in `docs/` when you change a command or behavior.

## Where to start

Open issues / deferred work live in `.dev/bugs/ccync-carve-deferred.md` (notably **B-01**: making
projection source-optional so installed plugins actually reach every agent).
