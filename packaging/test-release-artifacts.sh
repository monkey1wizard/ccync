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
# Probe 1: no GAL / golem / -p cli residue in release files or CONTRIBUTING
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
  "${REPO_ROOT}/CONTRIBUTING.md"
  "${SCRIPT_DIR}/gen-release-artifacts.sh"
  "${SCRIPT_DIR}/install.sh"
  "${SCRIPT_DIR}/install.ps1"
  "${SCRIPT_DIR}/publish-public.sh"
  "${REPO_ROOT}/.github/workflows/release.yml"
  "${REPO_ROOT}/.github/workflows/release-build.yml"
  "${REPO_ROOT}/.github/workflows/release-rehearsal.yml"
)

RESIDUE_COUNT=0
for f in "${PROBE1_FILES[@]}"; do
  if [[ ! -f "$f" && "$f" == */release-rehearsal.yml && ! -f "${REPO_ROOT}/.github/workflows/publish-public.yml" ]]; then continue; fi
  [[ -f "$f" ]] || { fail "expected file not found: $f"; continue; }
  cnt=$(grep -icE "$RESIDUE_PATTERN" "$f" 2>/dev/null || true)
  if [[ "$cnt" -gt 0 ]]; then
    fail "GAL/golem/-p cli residue in $(basename "$f"): $cnt match(es)"
    RESIDUE_COUNT=$(( RESIDUE_COUNT + cnt ))
  fi
done

# Recursive scan of crates/ and docs/ for residue. Root CONTRIBUTING.md,
# packaging files, and release.yml are covered by PROBE1_FILES above.
is_current_gal_reference() {
  local rel="$1"
  local line="$2"
  case "$rel:$line" in
    *'crates/ccync-engine/src/install.rs:'*'Antigravity plugin manifest shape (`gal`'"'"'s own'*) return 0 ;;
    *'crates/ccync-engine/src/install.rs:'*'`~/.gal/plugins/gal/plugin.json`'*) return 0 ;;
  esac
  return 1
}

for dir in "${REPO_ROOT}/crates" "${REPO_ROOT}/docs"; do
  [[ -d "$dir" ]] || continue
  while IFS= read -r rf; do
    normalized_root="${REPO_ROOT//\\//}"
    normalized_file="${rf//\\//}"
    rel="${normalized_file#"${normalized_root}/"}"
    while IFS=: read -r lineno line; do
      if is_current_gal_reference "$rel" "$line"; then
        continue
      fi
      fail "GAL/golem/-p cli residue in ${rel}:${lineno}"
      RESIDUE_COUNT=$(( RESIDUE_COUNT + 1 ))
    done < <(grep -niE "$RESIDUE_PATTERN" "$rf" 2>/dev/null || true)
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
TMP="${CCYNC_TEST_TMP:-$(mktemp -d)}"
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
if ! bash "${SCRIPT_DIR}/gen-release-artifacts.sh" \
  --version "$VER" \
  --assets-dir "$ASSETS_DIR" \
  --output-dir "$OUTPUT_DIR" \
  >"${TMP}/generator.log" 2>&1; then
  fail "Probe 2 — artifact generator failed"
  sed -n '1,20p' "${TMP}/generator.log" >&2
fi

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

# Probe 3: the authorized bare-RC tag stays intact in asset and manifest URLs.
RC_VER="v0.2.0-rc"
RC_ASSETS_DIR="${TMP}/rc-assets"
RC_OUTPUT_DIR="${TMP}/rc-out"
mkdir -p "$RC_ASSETS_DIR"
for suffix in linux-x64.tar.gz linux-arm64.tar.gz darwin-x64.tar.gz darwin-arm64.tar.gz windows-x64.zip windows-arm64.zip; do
  printf 'synthetic-RC-content-for-%s\n' "$suffix" > "${RC_ASSETS_DIR}/ccync-${RC_VER}-${suffix}"
done

if ! bash "${SCRIPT_DIR}/gen-release-artifacts.sh" \
  --version "$RC_VER" \
  --assets-dir "$RC_ASSETS_DIR" \
  --output-dir "$RC_OUTPUT_DIR" \
  >"${TMP}/rc-generator.log" 2>&1; then
  fail "Probe 3 — v0.2.0-rc artifact generator failed"
  sed -n '1,20p' "${TMP}/rc-generator.log" >&2
else
  if grep -qF "ccync-${RC_VER}-linux-x64.tar.gz" "${RC_OUTPUT_DIR}/checksums.txt" \
      && grep -qF "\"version\": \"${RC_VER}\"" "${RC_OUTPUT_DIR}/artifact-manifest.json" \
      && grep -qF "ccync-${RC_VER}-windows-x64.zip" "${RC_OUTPUT_DIR}/artifact-manifest.json"; then
    ok "Probe 3a — v0.2.0-rc asset names and manifest version retain the full tag"
  else
    fail "Probe 3a — v0.2.0-rc full tag missing from checksums or artifact manifest"
  fi

  RC_WINGET="${RC_OUTPUT_DIR}/manifests/m/Monkey1Wizard/ccync/0.2.0-rc/Monkey1Wizard.ccync.installer.yaml"
  if grep -qF "https://github.com/monkey1wizard/ccync/releases/download/${RC_VER}/ccync-${RC_VER}-windows-x64.zip" "$RC_WINGET"; then
    ok "Probe 3b — v0.2.0-rc WinGet installer URL retains the full tag"
  else
    fail "Probe 3b — v0.2.0-rc WinGet installer URL is missing the full tag"
  fi

  if grep -qF "https://github.com/monkey1wizard/ccync/releases/download/${RC_VER}/ccync-${RC_VER}-linux-x64.tar.gz" "${RC_OUTPUT_DIR}/ccync.rb"; then
    ok "Probe 3c — v0.2.0-rc Homebrew asset URL retains the full tag"
  else
    fail "Probe 3c — v0.2.0-rc Homebrew asset URL is missing the full tag"
  fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""

baseline_files=(checksums.txt artifact-manifest.json ccync.rb
  manifests/m/Monkey1Wizard/ccync/0.0.0/Monkey1Wizard.ccync.installer.yaml
  manifests/m/Monkey1Wizard/ccync/0.0.0/Monkey1Wizard.ccync.locale.en-US.yaml
  manifests/m/Monkey1Wizard/ccync/0.0.0/Monkey1Wizard.ccync.yaml)
baseline_hashes=(ad6d7b4c96f8183896b563ff713b72eac8ec9f3c50fbfc5b799a629b4df98f61
  70277dc243bd343d051b4415cfb6b54fcf099a2a1be6402970045c2515ebc1e0
  f0119da41fbf56a02a3e808e327d7e18a6edeb0b0e41e56f2471926cc6f6f555
  557eb64a7c4cf27b4f8e17f6cb484ee2de8f4b709b795a8b80a4a0fbc268b270
  5bfd75878ad6ede42829a1e9d02e14ca1686c9b2be61321168562fc9786dfcdb
  5fbc6d1db245a2006f9ab4339e273da4dc0b4de6207a22c0cccd49949134adcc)
for i in "${!baseline_files[@]}"; do
  if [[ "$(sha256_file "$OUTPUT_DIR/${baseline_files[$i]}")" == "${baseline_hashes[$i]}" ]]; then
    ok "baseline output ${baseline_files[$i]}"
  else
    fail "baseline output changed: ${baseline_files[$i]}"
  fi
done
ALT_OUTPUT="${TMP}/alternate"
mkdir -p "$ALT_OUTPUT"
if bash "${REPO_ROOT}/packaging/gen-release-artifacts.sh" --version "$RC_VER" --assets-dir "$RC_ASSETS_DIR" --output-dir "$ALT_OUTPUT" --download-base http://127.0.0.1:8765 > /dev/null 2>&1 \
  && cmp -s "$RC_OUTPUT_DIR/checksums.txt" "$ALT_OUTPUT/checksums.txt" \
  && cmp -s "$RC_OUTPUT_DIR/artifact-manifest.json" "$ALT_OUTPUT/artifact-manifest.json" \
  && grep -qF "http://127.0.0.1:8765/ccync-${RC_VER}-linux-x64.tar.gz" "$ALT_OUTPUT/ccync.rb" \
  && grep -qF "http://127.0.0.1:8765/ccync-${RC_VER}-windows-x64.zip" "$ALT_OUTPUT/manifests/m/Monkey1Wizard/ccync/0.2.0-rc/Monkey1Wizard.ccync.installer.yaml"; then
  ok "alternate download base preserves hashes and asset names"
else
  fail "alternate download base did not preserve release artifacts"
fi
if [[ "$(uname -s)" == Linux ]]; then
mkdir -p "$TMP/server" "$TMP/isolated-home" "$TMP/mock-bin"
printf '#!/bin/sh\necho ccync 0.2.0\n' > "$TMP/server/ccync"
chmod +x "$TMP/server/ccync"
tar -czf "$TMP/server/ccync-v0.2.0-linux-$(uname -m | sed -e 's/x86_64/x64/' -e 's/aarch64/arm64/').tar.gz" -C "$TMP/server" ccync
archive=$(basename "$TMP"/server/*.tar.gz)
printf '%s  %s\n' "$(sha256_file "$TMP/server/$archive")" "$archive" > "$TMP/server/checksums.txt"
python3 - "$TMP/server" "$TMP/port" <<'PY' &
import functools, http.server, sys
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(http.server.SimpleHTTPRequestHandler, directory=sys.argv[1]))
with open(sys.argv[2], 'w') as f:
    f.write(str(server.server_port))
server.serve_forever()
PY
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true; rm -rf "$TMP"' EXIT
for attempt in {1..100}; do [[ -s "$TMP/port" ]] && break; sleep 0.1; done
base="http://127.0.0.1:$(cat "$TMP/port")"
real_curl=$(command -v curl)
cat > "$TMP/mock-bin/curl" <<'MOCK'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CURL_LOG"
for arg in "$@"; do
  case "$arg" in
    http://127.0.0.1:*) exec "$REAL_CURL" "$@" ;;
    https://api.github.com/*) printf '{"tag_name":"v0.2.0"}\n'; exit 0 ;;
  esac
done
exit 1
MOCK
chmod +x "$TMP/mock-bin/curl"
run_install() {
  env -u CCYNC_VERSION -u CCYNC_DOWNLOAD_BASE HOME="$TMP/isolated-home" USERPROFILE="$TMP/isolated-home" PATH="$TMP/mock-bin:$PATH" CURL_LOG="$TMP/curl.log" REAL_CURL="$real_curl" "$@" bash "${REPO_ROOT}/packaging/install.sh" > "$TMP/install.log" 2>&1
}
: > "$TMP/curl.log"
if run_install CCYNC_VERSION=v0.2.0 CCYNC_DOWNLOAD_BASE="$base" && [[ -x "$TMP/isolated-home/.local/bin/ccync" && -f "$TMP/isolated-home/.ccync/install-receipts/ccync.json" ]] && grep -qF 'warning:' "$TMP/install.log"; then
  ok 'isolated loopback install succeeds'
else
  fail 'isolated loopback install failed'
fi
rm -rf "$TMP/isolated-home/.local" "$TMP/isolated-home/.ccync"
printf '%064d  %s\n' 0 "$archive" > "$TMP/server/checksums.txt"
if ! run_install CCYNC_VERSION=v0.2.0 CCYNC_DOWNLOAD_BASE="$base" && grep -qF 'SHA-256 mismatch' "$TMP/install.log" && [[ ! -e "$TMP/isolated-home/.local/bin/ccync" && ! -e "$TMP/isolated-home/.ccync" ]]; then
  ok 'checksum mismatch prevents installation'
else
  fail 'checksum mismatch did not prevent installation'
fi
for base in http://example.com http://localhost.example.com ftp://localhost; do
  : > "$TMP/curl.log"
  if run_install CCYNC_VERSION=v0.2.0 CCYNC_DOWNLOAD_BASE="$base" || [[ -s "$TMP/curl.log" ]]; then
    fail "installer accepted invalid download base $base"
  else
    ok "installer rejects invalid download base $base"
  fi
done
: > "$TMP/curl.log"
if run_install CCYNC_DOWNLOAD_BASE=https://example.com || [[ -s "$TMP/curl.log" ]]; then
  fail "installer accepted download base without version"
else
  ok "installer requires version with download base"
fi


: > "$TMP/curl.log"
run_install || true
if grep -qF 'https://github.com/monkey1wizard/ccync/releases/download/v0.2.0/' "$TMP/curl.log"; then
  ok 'default install requests the public release URL'
else
  fail 'default install did not request the public release URL'
fi
else
  echo '[SKIP] isolated Unix installer cases require Linux'
fi

echo "test-release-artifacts: $PASS_COUNT probe(s) passed, $FAILURES failed."
[[ $FAILURES -eq 0 ]]
