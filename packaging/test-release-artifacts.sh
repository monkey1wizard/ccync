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

PROBE1_FILES=(
  "${SCRIPT_DIR}/gen-release-artifacts.sh"
  "${SCRIPT_DIR}/install.sh"
  "${SCRIPT_DIR}/install.ps1"
  "${SCRIPT_DIR}/publish-public.sh"
  "${REPO_ROOT}/.github/workflows/release.yml"
)

RESIDUE_COUNT=0
for f in "${PROBE1_FILES[@]}"; do
  [[ -f "$f" ]] || { fail "expected file not found: $f"; continue; }
  cnt=$(grep -cE 'gal|golem|GAL|-p cli' "$f" 2>/dev/null || true)
  if [[ "$cnt" -gt 0 ]]; then
    fail "GAL/golem/-p cli residue in $(basename "$f"): $cnt match(es)"
    RESIDUE_COUNT=$(( RESIDUE_COUNT + cnt ))
  fi
done
[[ $RESIDUE_COUNT -eq 0 ]] && ok "Probe 1 — no GAL/golem/-p cli residue (${#PROBE1_FILES[@]} files checked)"

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

# 2c. winget manifests generated
WINGET_FILES=(
  "Monkey1Wizard.ccync.yaml"
  "Monkey1Wizard.ccync.locale.en-US.yaml"
  "Monkey1Wizard.ccync.installer.yaml"
)
WINGET_MISSING=0
for wf in "${WINGET_FILES[@]}"; do
  [[ -f "${OUTPUT_DIR}/${wf}" ]] || { fail "Probe 2c — winget manifest missing: $wf"; WINGET_MISSING=$(( WINGET_MISSING + 1 )); }
done
[[ $WINGET_MISSING -eq 0 ]] && ok "Probe 2c — all ${#WINGET_FILES[@]} winget manifests present"

# 2d. ccync.rb generated
if [[ -f "${OUTPUT_DIR}/ccync.rb" ]]; then
  ok "Probe 2d — ccync.rb present"
else
  fail "Probe 2d — ccync.rb not generated"
fi

# 2e. installer manifest embeds correct SHAs for windows assets
if [[ -f "${OUTPUT_DIR}/Monkey1Wizard.ccync.installer.yaml" ]]; then
  WIN_X64_SHA="$(sha256_file "${ASSETS_DIR}/ccync-${VER}-windows-x64.zip")"
  WIN_ARM64_SHA="$(sha256_file "${ASSETS_DIR}/ccync-${VER}-windows-arm64.zip")"
  INSTALLER_CONTENT="$(cat "${OUTPUT_DIR}/Monkey1Wizard.ccync.installer.yaml")"
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
