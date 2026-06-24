# ccync — Agent Instructions

> ⚠ **Interim stub (2026-06-23).** These repo adapters were stale GAL-generated
> snapshots that could not be regenerated in-repo (no `gal` binary / `gal-core`
> sources here after the carve). This pointer replaces the stale GAL content
> pending a proper re-init once the installer regenerates adapters from
> `.dev/project.md`. **Do not treat this file as authoritative** — it is a
> derived carrier.

## What This Is

**ccync** is a standalone cross-agent plugin / MCP / skills manager (binary
`ccync`, machine home `~/.ccync`; it never touches `~/.gal`). It resolves a
plugin catalog into a lockfile, fetches/caches git plugins, renders a canonical
root, and projects skills / commands / agents / MCP onto every selected coding
agent (Claude Code, Codex, Copilot, Gemini CLI, OpenCode, Antigravity faces).

## Authoritative Context (read these, not this stub)

- `.dev/project.md` — architecture (6-crate tree), constraints, key decisions, verified facts. **Read first.**
- `.dev/state.md` — active plans + session continuity.
- `docs/architecture.md` — crate tree + disk layout.
- `docs/manual.md` — the 9-verb command surface (init / sync / add / remove / list / doctor / backup / restore / uninstall).
- `docs/devguide.md` · `docs/naming.md` · `docs/contributing.md`. Onboarding lives in the repo-root `README.md`.

## Conventions

- Rust workspace; `cargo test --workspace` + `cargo clippy --workspace` must stay green.
- Writes to a user's live agent config (`~/.claude.json`, `~/.codex/config.toml`, …) must be non-destructive and atomic.
- `crates/projection/` is a Protected Path (prune safety); touching it needs a recorded architect review.
