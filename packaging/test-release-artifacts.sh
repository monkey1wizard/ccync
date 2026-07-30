#!/usr/bin/env bash
#
# test-release-artifacts.sh — regression guard for the ccync release pipeline.
#
# Probe 1 — GAL-residue sweep:
#   grep gal|golem|GAL|-p cli across packaging/ + .github/workflows/release.yml must = 0.
#
# Probe 2 — gen-release-artifacts.sh dry-run:
#   Create synthetic ccync-v0.0.0-<os>-<arch>.{tar.gz,zip} assets, run the
#   generator, assert checksums.txt + winget manifests + ccync.rb are emitted and
#   each asset SHA in checksums.txt matches the actual file.
#
# Usage: bash packaging/test-release-artifacts.sh
# Exit: 0 = all probes pass; 1 = one or more probes fail.
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_DIR="${REPO_ROOT}/packaging"
FAILURES=0
PASS_COUNT=0

ok()   { echo "[PASS] $*"; PASS_COUNT=$(( PASS_COUNT + 1 )); }
fail() { echo "[FAIL] $*" >&2; FAILURES=$(( FAILURES + 1 )); }

# ---------------------------------------------------------------------------
# Probe 1: no GAL / golem / -p cli residue in packaging files or release.yml
# ---------------------------------------------------------------------------
#
# A bare `\b` is not enough here: `_` is a word character, so `\bgal\b` would
# miss `gal_core`, and POSIX ERE has no lookaround to fix that. Instead each
# token is anchored on a non-letter (or start/end) on both sides, matched
# case-insensitively — `gal-core`, `gal_core`, `.gal/...`, `GAL`, and
# `Golem-Agents-Legion` all still match, while `legal`, `Illegal`, and
# `legally` do not (the "gal"/"golem" they contain is flanked by letters).
RESIDUE_PATTERN='(^|[^A-Za-z])(gal|golem|-p cli)($|[^A-Za-z])'

PROBE1_FILES=(
  "${SCRIPT_DIR}/gen-release-artifacts.sh"
  "${SCRIPT_DIR}/install.sh"
  "${SCRIPT_DIR}/install.ps1"
  "${SCRIPT_DIR}/publish-public.sh"
  "${REPO_ROOT}/.github/workflows/release.yml"
)

# docs/devguide.md carries one deliberate mention of the rename (the `gal` in
# "the `gal`→`ccync` rename"). Allowlisted at clause (phrase) granularity, not
# by line number and not by skipping the whole line: the exact phrase is
# stripped wherever it appears in the file before each hit line is re-checked,
# so rewording the line still fails the probe, and the allowlist survives
# line drift from unrelated edits above or below it (a numbered-line allowlist
# would silently stop matching — or worse, start matching the wrong line —
# the moment the file's line count above it changes).
DEVGUIDE_ALLOWED_PHRASE='`gal`→`ccync` rename'

RESIDUE_COUNT=0
for f in "${PROBE1_FILES[@]}"; do
  [[ -f "$f" ]] || { fail "expected file not found: $f"; continue; }
  cnt=$(grep -icE "$RESIDUE_PATTERN" "$f" 2>/dev/null || true)
  if [[ "$cnt" -gt 0 ]]; then
    fail "GAL/golem/-p cli residue in $(basename "$f"): $cnt match(es)"
    RESIDUE_COUNT=$(( RESIDUE_COUNT + cnt ))
  fi
done

# Recursive scan of crates/ and docs/ for residue (packaging/ and .github/ are
# outside this scan root and are already covered by PROBE1_FILES above).
for dir in "${REPO_ROOT}/crates" "${REPO_ROOT}/docs"; do
  [[ -d "$dir" ]] || continue
  while IFS= read -r rf; do
    rel="${rf#"${REPO_ROOT}/"}"
    if [[ "$rel" == "docs/devguide.md" ]]; then
      while IFS=: read -r lineno linetext; do
        stripped="${linetext/"$DEVGUIDE_ALLOWED_PHRASE"/}"
        if ! echo "$stripped" | grep -iqE "$RESIDUE_PATTERN"; then
          continue
        fi
        fail "GAL/golem/-p cli residue in ${rel}:${lineno}"
        RESIDUE_COUNT=$(( RESIDUE_COUNT + 1 ))
      done < <(grep -niE "$RESIDUE_PATTERN" "$rf" 2>/dev/null || true)
    else
      cnt=$(grep -icE "$RESIDUE_PATTERN" "$rf" 2>/dev/null || true)
      fail "GAL/golem/-p cli residue in ${rel}: $cnt match(es)"
      RESIDUE_COUNT=$(( RESIDUE_COUNT + cnt ))
    fi
  done < <(grep -rliE "$RESIDUE_PATTERN" "$dir" 2>/dev/null || true)
done

[[ $RESIDUE_COUNT -eq 0 ]] && ok "Probe 1 — no GAL/golem/-p cli residue (${#PROBE1_FILES[@]} explicit files + crates/ + docs/ scanned)"

# ---------------------------------------------------------------------------
# Probe 1b: plan/decision-ID residue in tracked source outside .dev/**
# ---------------------------------------------------------------------------
#
# Observed leak families: single-digit R-prefixed refs (dashed and bare),
# letter-suffixed two-digit task refs, single-digit decision refs, lettered
# open-ended refs, single-digit RC-prefixed refs, two-digit F-prefixed refs,
# the Pn-Fn shape, and a letter+digit hybrid (an R-prefixed ref whose tail is
# a letter then a digit — a shape earlier per-file rewrite passes left
# surviving because it matches none of the other families above).
# Every fragment below is built by runtime string concatenation from bare
# letters and character classes — no fragment, once concatenated, spells a
# literal instance of any of those shapes, so this file's own source is
# clean against the very pattern it assembles.
DASH='-'
DIG='[0-9]'
ALPHA='[A-Za-z]'
PWB_L='(^|[^A-Za-z0-9])'
PWB_R='($|[^A-Za-z0-9])'

PLANID_FAM_R="${PWB_L}R${DASH}${DIG}${PWB_R}"
PLANID_FAM_RBARE="${PWB_L}R${DIG}${PWB_R}"
PLANID_FAM_T="${PWB_L}T${DASH}${DIG}${DIG}${ALPHA}${PWB_R}"
PLANID_FAM_D="${PWB_L}D${DASH}${DIG}${PWB_R}"
PLANID_FAM_OE="${PWB_L}OE${DASH}${ALPHA}${PWB_R}"
PLANID_FAM_RC="${PWB_L}RC${DASH}${DIG}${PWB_R}"
PLANID_FAM_F="${PWB_L}F${DASH}${DIG}${DIG}${PWB_R}"
PLANID_FAM_P="${PWB_L}P${DIG}${DASH}F${DIG}${PWB_R}"
PLANID_FAM_RHYBRID="${PWB_L}R${DASH}${ALPHA}${DIG}${PWB_R}"

PLANID_PATTERN="${PLANID_FAM_R}|${PLANID_FAM_RBARE}|${PLANID_FAM_T}|${PLANID_FAM_D}|${PLANID_FAM_OE}|${PLANID_FAM_RC}|${PLANID_FAM_F}|${PLANID_FAM_P}|${PLANID_FAM_RHYBRID}"

PLANID_MISS=0
while IFS= read -r rf; do
  [[ "$rf" == .dev/* ]] && continue
  full="${REPO_ROOT}/${rf}"
  [[ -f "$full" ]] || continue
  cnt=$(grep -a -cE "$PLANID_PATTERN" "$full" 2>/dev/null || true)
  if [[ "$cnt" -gt 0 ]]; then
    fail "plan-ID residue in ${rf}: $cnt match(es)"
    PLANID_MISS=$(( PLANID_MISS + cnt ))
  fi
done < <(git -C "$REPO_ROOT" ls-files)

[[ $PLANID_MISS -eq 0 ]] && ok "Probe 1b — no plan-ID residue in tracked source outside .dev/**"

# ---------------------------------------------------------------------------
# Probe 2: gen-release-artifacts.sh produces correct outputs for v0.0.0
# ---------------------------------------------------------------------------

VER="v0.0.0"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

ASSETS_DIR="${TMP}/assets"
OUTPUT_DIR="${TMP}/out"
mkdir -p "$ASSETS_DIR"

# Synthetic assets — unique content so each SHA differs
ASSET_NAMES=(
  "ccync-${VER}-linux-x64.tar.gz"
  "ccync-${VER}-linux-arm64.tar.gz"
  "ccync-${VER}-darwin-x64.tar.gz"
  "ccync-${VER}-darwin-arm64.tar.gz"
  "ccync-${VER}-windows-x64.zip"
  "ccync-${VER}-windows-arm64.zip"
)
for name in "${ASSET_NAMES[@]}"; do
  printf 'synthetic-content-for-%s\n' "$name" > "${ASSETS_DIR}/${name}"
done

# Run the generator
bash "${SCRIPT_DIR}/gen-release-artifacts.sh" \
  --version "$VER" \
  --assets-dir "$ASSETS_DIR" \
  --output-dir "$OUTPUT_DIR" \
  >/dev/null 2>&1

# 2a. checksums.txt exists and has one line per asset
CHECKSUMS="${OUTPUT_DIR}/checksums.txt"
if [[ ! -f "$CHECKSUMS" ]]; then
  fail "Probe 2a — checksums.txt not generated"
else
  LINE_COUNT=$(wc -l < "$CHECKSUMS" | tr -d ' ')
  EXPECTED=${#ASSET_NAMES[@]}
  if [[ "$LINE_COUNT" -eq "$EXPECTED" ]]; then
    ok "Probe 2a — checksums.txt has $LINE_COUNT lines (= $EXPECTED assets)"
  else
    fail "Probe 2a — checksums.txt: expected $EXPECTED lines, got $LINE_COUNT"
  fi

  # 2b. each SHA in checksums.txt matches the actual file
  SHA_MISMATCH=0
  sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
      shasum -a 256 "$1" | awk '{print $1}'
    else
      echo "NO_SHA_TOOL"; return 1
    fi
  }

  while IFS= read -r line; do
    recorded_sha="${line%% *}"
    fname="${line##*  }"
    actual_sha="$(sha256_file "${ASSETS_DIR}/${fname}")"
    if [[ "$recorded_sha" != "$actual_sha" ]]; then
      fail "Probe 2b — SHA mismatch for $fname: recorded=$recorded_sha actual=$actual_sha"
      SHA_MISMATCH=$(( SHA_MISMATCH + 1 ))
    fi
  done < "$CHECKSUMS"
  [[ $SHA_MISMATCH -eq 0 ]] && ok "Probe 2b — all ${LINE_COUNT} asset SHAs in checksums.txt verified"
fi

# 2c. winget manifests generated in the nested winget-pkgs submission layout
WINGET_OUT="${OUTPUT_DIR}/manifests/m/Monkey1Wizard/ccync/${VER#v}"
WINGET_FILES=(
  "Monkey1Wizard.ccync.yaml"
  "Monkey1Wizard.ccync.locale.en-US.yaml"
  "Monkey1Wizard.ccync.installer.yaml"
)
WINGET_MISSING=0
for wf in "${WINGET_FILES[@]}"; do
  [[ -f "${WINGET_OUT}/${wf}" ]] || { fail "Probe 2c — winget manifest missing: $wf"; WINGET_MISSING=$(( WINGET_MISSING + 1 )); }
done
[[ $WINGET_MISSING -eq 0 ]] && ok "Probe 2c — all ${#WINGET_FILES[@]} winget manifests present under ${WINGET_OUT#"${TMP}/"}"

# 2d. ccync.rb generated
if [[ -f "${OUTPUT_DIR}/ccync.rb" ]]; then
  ok "Probe 2d — ccync.rb present"
else
  fail "Probe 2d — ccync.rb not generated"
fi

# 2e. installer manifest embeds correct SHAs for windows assets
if [[ -f "${WINGET_OUT}/Monkey1Wizard.ccync.installer.yaml" ]]; then
  WIN_X64_SHA="$(sha256_file "${ASSETS_DIR}/ccync-${VER}-windows-x64.zip")"
  WIN_ARM64_SHA="$(sha256_file "${ASSETS_DIR}/ccync-${VER}-windows-arm64.zip")"
  INSTALLER_CONTENT="$(cat "${WINGET_OUT}/Monkey1Wizard.ccync.installer.yaml")"
  SHA_EMBED_OK=1
  if ! echo "$INSTALLER_CONTENT" | grep -qF "$WIN_X64_SHA"; then
    fail "Probe 2e — installer.yaml missing windows-x64 SHA ($WIN_X64_SHA)"
    SHA_EMBED_OK=0
  fi
  if ! echo "$INSTALLER_CONTENT" | grep -qF "$WIN_ARM64_SHA"; then
    fail "Probe 2e — installer.yaml missing windows-arm64 SHA ($WIN_ARM64_SHA)"
    SHA_EMBED_OK=0
  fi
  [[ $SHA_EMBED_OK -eq 1 ]] && ok "Probe 2e — installer.yaml embeds correct windows SHAs"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "test-release-artifacts: $PASS_COUNT probe(s) passed, $FAILURES failed."
[[ $FAILURES -eq 0 ]]
