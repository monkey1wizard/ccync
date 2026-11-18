#!/usr/bin/env bash
#
# release-body.sh — shared stable-release changelog gate and section extractor.
#
# The changelog gate used by the private release controller and public GitHub
# `sign` job. Given a release version, it:
#   - accepts `X.Y.Z`, `X.Y.Z-rc`, or `X.Y.Z-rcN`, with an optional leading `v` alias;
#   - for a release candidate, prints a skip notice and exits 0 with zero
#     writes (RCs are rehearsal-only and exempt from the gate);
#   - for a stable version, extracts the matching `## [X.Y.Z]` section from
#     CHANGELOG.md and fails closed unless that section exists, is non-blank,
#     and carries no unresolved draft sentinel.
#
# All validation happens before any output is written: a failing run never
# creates or truncates --output.
#
# Usage:
#   release-body.sh <version> [--changelog <path>] [--output <path>] [--validate-only]
#
# --changelog <path>   Changelog file to read. Default: <repo-root>/CHANGELOG.md,
#                       where <repo-root> is resolved via `git rev-parse --show-toplevel`
#                       from the current working directory.
# --output <path>      Write the extracted section here (parent dir is created).
#                       Default: write to stdout.
# --validate-only       Run every check but write nothing, even to stdout.
#
set -euo pipefail

PROG="release-body"

die_usage() {
  echo "${PROG}: error: $*" >&2
  exit 1
}

usage() {
  sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
}

CHANGELOG_PATH=""
OUTPUT_PATH=""
VALIDATE_ONLY=0
POSITIONAL=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --changelog)
      [[ $# -ge 2 ]] || die_usage "--changelog needs a value"
      CHANGELOG_PATH="$2"
      shift 2
      ;;
    --output)
      [[ $# -ge 2 ]] || die_usage "--output needs a value"
      OUTPUT_PATH="$2"
      shift 2
      ;;
    --validate-only)
      VALIDATE_ONLY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      POSITIONAL+=("$1")
      shift
      ;;
  esac
done

# Any argument that is not a recognized flag and is not exactly one bare
# version token is an invalid-version failure — including option-shaped
# tokens like `--bogus`, which fall through the case above into POSITIONAL
# rather than being rejected as an "unknown flag". This keeps the version
# grammar the single source of truth for what counts as a valid version arg.
if [[ ${#POSITIONAL[@]} -ne 1 ]]; then
  die_usage "invalid version '${POSITIONAL[*]:-}' (expected exactly one version argument: X.Y.Z, X.Y.Z-rc, or X.Y.Z-rcN, optional leading v)"
fi

VERSION_RAW="${POSITIONAL[0]}"

# Strip a single leading 'v' into a fresh local variable — VERSION_RAW itself
# is never rewritten, so diagnostics can always report exactly what the
# caller passed.
VERSION_NOV="${VERSION_RAW#v}"

VERSION_RE='^([0-9]+\.[0-9]+\.[0-9]+)(-rc[0-9]*)?$'
if [[ ! "$VERSION_NOV" =~ $VERSION_RE ]]; then
  die_usage "invalid version '${VERSION_RAW}' (expected X.Y.Z, X.Y.Z-rc, or X.Y.Z-rcN, optional leading v)"
fi

VERSION_CORE="${BASH_REMATCH[1]}"
VERSION_RC="${BASH_REMATCH[2]}"

if [[ -n "$VERSION_RC" ]]; then
  echo "${PROG}: '${VERSION_RAW}' is a release candidate — rehearsal-only, skipping the changelog gate"
  exit 0
fi

# --- Stable path: resolve the changelog file --------------------------------
if [[ -z "$CHANGELOG_PATH" ]]; then
  REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" \
    || die_usage "unable to resolve repo root via 'git rev-parse --show-toplevel' (not inside a git repository, and no --changelog was given)"
  CHANGELOG_PATH="${REPO_ROOT}/CHANGELOG.md"
fi

[[ -f "$CHANGELOG_PATH" ]] || die_usage "changelog not found at '${CHANGELOG_PATH}'"

# --- Extract the exact column-0 '## [X.Y.Z]' section -------------------------
#
# Fenced code blocks (``` or ~~~ at column 0) are tracked so that a heading-
# shaped line inside an example never counts as a real section boundary.
# Capture starts right after the matching heading line and runs through the
# byte before the next column-0 '## [' heading (or EOF).
HEADING_RE='^## \[([0-9]+\.[0-9]+\.[0-9]+)\]( - [0-9]{4}-[0-9]{2}-[0-9]{2})?[[:space:]]*$'
FENCE_RE='^(```|~~~)'

# An odd count of column-0 fence markers means one fence is unterminated
# somewhere in the file. Left unchecked, the extraction loop below would
# leave in_fence stuck at 1 past that point, which suppresses heading-
# boundary detection and lets a later section's body silently merge into
# the target section (or mask an actually-blank target section). Fail
# closed before any boundary decision is made instead of guessing.
FENCE_COUNT=0
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ "$line" =~ $FENCE_RE ]] && FENCE_COUNT=$((FENCE_COUNT + 1))
done < "$CHANGELOG_PATH"

if (( FENCE_COUNT % 2 != 0 )); then
  die_usage "malformed changelog: unbalanced code fence in '${CHANGELOG_PATH}'"
fi

in_fence=0
state="searching"
found_heading=0
body=""

while IFS= read -r line || [[ -n "$line" ]]; do
  if [[ "$line" =~ $FENCE_RE ]]; then
    if [[ $in_fence -eq 0 ]]; then in_fence=1; else in_fence=0; fi
    if [[ "$state" == "capturing" ]]; then
      body+="${line}"$'\n'
    fi
    continue
  fi

  if [[ $in_fence -eq 0 && "$line" =~ $HEADING_RE ]]; then
    if [[ "$state" == "capturing" ]]; then
      # Next real section heading — capture stops here, this line excluded.
      state="done"
      break
    fi
    if [[ "${BASH_REMATCH[1]}" == "$VERSION_CORE" ]]; then
      state="capturing"
      found_heading=1
    fi
    continue
  fi

  if [[ "$state" == "capturing" ]]; then
    body+="${line}"$'\n'
  fi
done < "$CHANGELOG_PATH"

[[ $found_heading -eq 1 ]] || die_usage "no changelog section found for version '${VERSION_CORE}' in '${CHANGELOG_PATH}'"

if [[ ! "$body" =~ [^[:space:]] ]]; then
  die_usage "changelog section for version '${VERSION_CORE}' is blank"
fi

if [[ "$body" == *"[[CCYNC-RELEASE-DRAFT:"* ]]; then
  die_usage "changelog section for version '${VERSION_CORE}' still contains an unresolved draft sentinel ([[CCYNC-RELEASE-DRAFT:)"
fi

# --- Write bytes last: only after every check above has passed ---------------
[[ $VALIDATE_ONLY -eq 1 ]] && exit 0

if [[ -n "$OUTPUT_PATH" ]]; then
  mkdir -p "$(dirname "$OUTPUT_PATH")"
  printf '%s' "$body" > "$OUTPUT_PATH"
else
  printf '%s' "$body"
fi
