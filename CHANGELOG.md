# Changelog

All notable user-facing changes to ccync are documented in this file. Each
section corresponds to a stable release tag and is curated from the commits
merged into that release's source range.

## [0.1.5]

Source range: `v0.1.4..v0.1.5`

Follow-up fix for the WinGet release verification step introduced in 0.1.4:
after the printed WinGet-package version assertion passes, the release script
now exits `0` instead of propagating an unrelated non-zero exit code from the
verification command, which had been failing the release job even on a
successful check.

## [0.1.4]

Source range: `v0.1.3..v0.1.4`

Follow-up fix for the WinGet publish flow: the release script now asserts
against the version string WinGetCreate actually prints to its own output,
rather than trusting the wrapping command's process exit code, closing a gap
where a failed WinGet submission could be reported as successful.

## [0.1.3]

Source range: `v0.1.2..v0.1.3`

Fixes the WinGet manifest submission step in the release pipeline: the
release script now invokes `WingetCreate` with its correct verb and options,
replacing an invocation that had been silently using the wrong arguments.

## [0.1.2]

Source range: `378c672b..v0.1.2`

Release-automation hardening release, built on top of the 0.1.1 resubmission
work. Adds the GitLab `publish-public-release` job and a matching GitHub tag
and crate-version preflight, static-CRT-linkage assertion for Windows MSVC
release targets, WinGet manifest packaging in the `winget-pkgs` submission
layout, package-manager manifest upload plus GitHub Release notes, a
stable-only `submit-winget` job, artifact/checksum/cosign smoke verification,
Cargo-install and platform installer smoke jobs, and a stable-only Homebrew
smoke job. Tightens job permissions to least privilege across `release.yml`
and fixes a dot-sourced-host `iex` install-guard bug. Also lands
`ccync-engine` materializing `.claude-plugin/plugin.json` into the render
output and documents the projection prune two-row ownership/authorization
model.

## [0.1.1]

Source range: `v0.1.0..378c672b`

First post-launch stability release. `ccync` with no arguments now prints
help to stdout and exits `0` instead of behaving as an error case. Adds a
static-CRT-linkage fix for the Windows WinGet-distributed executable and an
arg-discipline guard that rejects unknown flags across every CLI verb,
closing a `remove --dry-run` handling gap along the way. `backup` now
correctly includes the personal plugin catalog (`plugins.json`) and
`restore` covers it symmetrically. Internal `refresh` and `restore-source`
developer commands are added, along with `update`/`upgrade` self-update
verbs (provenance detection, verified-release fetch, atomic binary swap) and
the `show`, `cleanup`, `pin`, and `list --upgrade-available` plugin-lifecycle
verbs. Fixes a marketplace canonical-root resolution bug, a runtime-selection
index panic, and a `cmd_add` first-run gate ordering bug. Continues the
GAL-vocabulary removal from user-facing docs and code carried over from
0.1.0, and migrates machine state onto the `~/.ccync/` three-class
(`config`/`cache`/`build`) layout.

## [0.1.0]

Initial public release of ccync: a standalone cross-agent plugin, MCP, and
skills manager. Projects a plugin's skills, commands, agents, and MCP server
definitions onto each supported coding agent's native configuration surface
— Claude Code, Codex, Copilot, Gemini CLI, OpenCode, and the Antigravity
faces — from a single curated catalog and lockfile, with `ccync doctor`
health checks and atomic, non-destructive writes to each agent's live
config.
