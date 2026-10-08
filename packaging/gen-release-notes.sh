#!/usr/bin/env bash
#
# gen-release-notes.sh — deterministic sentinel-marked release-notes draft
# generator.
#
# Given a commit range, walks non-merge commits, drops the fixed noise
# allowlist, and groups every retained commit into Added -> Changed ->
# Fixed -> Other. Every retained item is prefixed with
# `[[CCYNC-RELEASE-DRAFT:<short-hash>]]` so a human curator can find and
# strip each sentinel while rewriting the draft into CHANGELOG.md.
#
# Grouping (Conventional Commit `type(scope): subject`):
#   - Breaking-change markers (`!` before the colon, or a `BREAKING CHANGE:`
#     / `BREAKING-CHANGE:` footer line) take priority over type and land the
#     commit in Changed regardless of its type.
#   - `feat` -> Added.
#   - `fix` -> Fixed.
#   - Everything else -> Other, with `[REVIEW]` prepended to flag it for
#     human triage.
#
# Noise allowlist: a commit is dropped (not grouped) only when its
# (type, scope, subject-prefix) matches one of exactly three fixed triples:
#   docs  | plan    | subject starts with "converge T-"
#   chore | plans   | subject starts with "absorb the "
#   chore | release | subject starts with "bump ccync-cli to "
# Any other commit is retained, including every `docs(state)` commit — a
# near miss on type, scope, or subject-prefix is never dropped.
#
# The footer separately reports the exact count of structural merge
# exclusions (git's own merge commits) and noise-allowlist exclusions.
#
# Range resolution never writes anything until every check below has passed:
# a malformed range, an unresolved endpoint, or a git subprocess/framing
# failure aborts before any stdout is produced.
#
# Usage:
#   gen-release-notes.sh [<from>..<to>]
#
# With no argument, <from> defaults to the numerically highest HEAD-reachable
# exact stable `vX.Y.Z` tag (not the most recently created or dated one) and
# <to> defaults to HEAD. An explicit argument must be exactly one non-empty
# two-dot range whose endpoints both resolve to commits; three-dot ranges,
# empty endpoints, extra arguments, and option-shaped arguments are rejected.
#
set -euo pipefail

PROG="gen-release-notes"

die_usage() {
  echo "${PROG}: error: $*" >&2
  exit 1
}

usage() {
  sed -n '2,44p' "$0" | sed 's/^# \{0,1\}//'
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ $# -gt 1 ]]; then
  die_usage "unexpected extra arguments (expected at most one <from>..<to> range)"
fi

RANGE_RAW="${1:-}"

if [[ -n "$RANGE_RAW" && "$RANGE_RAW" == -* ]]; then
  die_usage "invalid range '${RANGE_RAW}' (option-shaped argument is not a valid range)"
fi

git rev-parse --show-toplevel >/dev/null 2>&1 \
  || die_usage "not inside a git repository"

if [[ -n "$RANGE_RAW" ]]; then
  # --- Explicit range: validate shape, then both endpoints -------------------
  if [[ "$RANGE_RAW" == *"..."* ]]; then
    die_usage "invalid range '${RANGE_RAW}' (three-dot ranges are not supported, use <from>..<to>)"
  fi
  if [[ "$RANGE_RAW" != *".."* ]]; then
    die_usage "invalid range '${RANGE_RAW}' (expected <from>..<to>)"
  fi

  FROM_REF="${RANGE_RAW%%..*}"
  TO_REF="${RANGE_RAW#*..}"

  if [[ -z "$FROM_REF" || -z "$TO_REF" ]]; then
    die_usage "invalid range '${RANGE_RAW}' (both endpoints of <from>..<to> must be non-empty)"
  fi
  if [[ "$TO_REF" == *".."* ]]; then
    die_usage "invalid range '${RANGE_RAW}' (expected exactly one '..' separator)"
  fi

  git rev-parse --verify -q "${FROM_REF}^{commit}" >/dev/null 2>&1 \
    || die_usage "invalid range '${RANGE_RAW}' (from-endpoint '${FROM_REF}' does not resolve to a commit)"
  git rev-parse --verify -q "${TO_REF}^{commit}" >/dev/null 2>&1 \
    || die_usage "invalid range '${RANGE_RAW}' (to-endpoint '${TO_REF}' does not resolve to a commit)"
else
  # --- Default range: numerically highest HEAD-reachable stable tag..HEAD ----
  mapfile -t ALL_TAGS < <(git tag --merged HEAD 2>/dev/null)
  STABLE_TAGS=()
  for t in "${ALL_TAGS[@]}"; do
    if [[ "$t" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      STABLE_TAGS+=("$t")
    fi
  done

  [[ ${#STABLE_TAGS[@]} -gt 0 ]] \
    || die_usage "no stable vX.Y.Z tag reachable from HEAD; pass an explicit <from>..<to> range"

  FROM_REF="$(printf '%s\n' "${STABLE_TAGS[@]}" | sort -V | tail -n1)"
  TO_REF="HEAD"
fi

RANGE="${FROM_REF}..${TO_REF}"

# --- Structural merge count, over the same range ----------------------------
MERGE_COUNT="$(git rev-list --count --merges "$RANGE" 2>/dev/null)" \
  || die_usage "unable to count merges for range '${RANGE}'"
[[ "$MERGE_COUNT" =~ ^[0-9]+$ ]] \
  || die_usage "unexpected merge count output for range '${RANGE}'"

# --- Collect non-merge commits, NUL-framed ----------------------------------
LOG_TMP="$(mktemp)"
LOG_ERR="$(mktemp)"
cleanup() { rm -f "$LOG_TMP" "$LOG_ERR"; }
trap cleanup EXIT

if ! git log --no-merges -z --format='%H%x00%s%x00%b' "$RANGE" > "$LOG_TMP" 2>"$LOG_ERR"; then
  die_usage "git log failed for range '${RANGE}': $(cat "$LOG_ERR")"
fi

mapfile -d '' -t FIELDS < "$LOG_TMP"

if (( ${#FIELDS[@]} % 3 != 0 )); then
  die_usage "malformed git log output for range '${RANGE}' (NUL-framed record count is not a multiple of 3)"
fi

# --- Noise allowlist: exactly three fixed (type, scope, subject-prefix) ----
# triples. A commit is dropped only on a full match of all three fields;
# any near miss (type, scope, or subject-prefix) is retained.
is_allowlisted() {
  local type="$1" scope="$2" desc="$3"
  case "${type}|${scope}" in
    "docs|plan")
      [[ "$desc" == "converge T-"* ]] && return 0
      ;;
    "chore|plans")
      [[ "$desc" == "absorb the "* ]] && return 0
      ;;
    "chore|release")
      [[ "$desc" == "bump ccync-cli to "* ]] && return 0
      ;;
  esac
  return 1
}

SUBJECT_RE='^([A-Za-z]+)(\(([^)]*)\))?(!)?:[[:space:]]*(.*)$'

ADDED_ITEMS=()
CHANGED_ITEMS=()
FIXED_ITEMS=()
OTHER_ITEMS=()
ALLOWLIST_COUNT=0

i=0
while (( i < ${#FIELDS[@]} )); do
  hash="${FIELDS[i]}"
  subject="${FIELDS[i+1]}"
  body="${FIELDS[i+2]}"
  [[ "$hash" =~ ^[0-9a-f]{7,40}$ ]] \
    || die_usage "malformed git log output for range '${RANGE}' (unexpected commit hash framing)"

  type=""
  scope=""
  bang=""
  desc="$subject"
  if [[ "$subject" =~ $SUBJECT_RE ]]; then
    type="${BASH_REMATCH[1],,}"
    scope="${BASH_REMATCH[3]}"
    bang="${BASH_REMATCH[4]}"
    desc="${BASH_REMATCH[5]}"
  fi

  if is_allowlisted "$type" "$scope" "$desc"; then
    ALLOWLIST_COUNT=$(( ALLOWLIST_COUNT + 1 ))
    i=$((i + 3))
    continue
  fi

  item="[[CCYNC-RELEASE-DRAFT:${hash:0:7}]] ${subject}"

  breaking=0
  [[ -n "$bang" ]] && breaking=1
  if [[ "$body" == *"BREAKING CHANGE:"* || "$body" == *"BREAKING-CHANGE:"* ]]; then
    breaking=1
  fi

  if (( breaking )); then
    CHANGED_ITEMS+=("$item")
  elif [[ "$type" == "feat" ]]; then
    ADDED_ITEMS+=("$item")
  elif [[ "$type" == "fix" ]]; then
    FIXED_ITEMS+=("$item")
  else
    OTHER_ITEMS+=("[[CCYNC-RELEASE-DRAFT:${hash:0:7}]] [REVIEW] ${subject}")
  fi

  i=$((i + 3))
done

# --- Write output: only after every check above has passed -----------------
print_group() {
  local title="$1"; shift
  local items=("$@")
  (( ${#items[@]} == 0 )) && return
  printf '### %s\n' "$title"
  for line in "${items[@]}"; do
    printf '%s\n' "$line"
  done
  printf '\n'
}

print_group "Added" "${ADDED_ITEMS[@]+"${ADDED_ITEMS[@]}"}"
print_group "Changed" "${CHANGED_ITEMS[@]+"${CHANGED_ITEMS[@]}"}"
print_group "Fixed" "${FIXED_ITEMS[@]+"${FIXED_ITEMS[@]}"}"
print_group "Other" "${OTHER_ITEMS[@]+"${OTHER_ITEMS[@]}"}"

printf 'Merges excluded: %s\n' "$MERGE_COUNT"
printf 'Allowlist excluded: %s\n' "$ALLOWLIST_COUNT"
