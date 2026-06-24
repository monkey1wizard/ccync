#!/usr/bin/env bash
#
# gen-release-artifacts.sh — ccync release artifact generator.
#
# Self-contained replacement for a `release` CLI verb: ccync's public surface is
# frozen at 9 verbs (no `release`), so manifest generation lives here as a plain
# POSIX script with zero dependency on the ccync binary and zero shared tooling
# with any other product's release.
#
# Given the canonical release assets (ccync-<version>-<os>-<arch>.{tar.gz,zip,exe})
# it produces, into --output-dir:
#   - checksums.txt          SHA-256 over every archive + standalone binary
#   - artifact-manifest.json machine-readable {version, assets[]} index
#   - Monkey1Wizard.ccync.*.yaml         winget manifests (version/installer/locale)
#   - ccync.rb               homebrew formula (binary-only)
#
# Usage:
#   gen-release-artifacts.sh --version v0.1.0 --assets-dir . --output-dir release-out
#
set -euo pipefail

VERSION=""
ASSETS_DIR="."
OUTPUT_DIR="release-out"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() { echo "gen-release-artifacts: error: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)     VERSION="${2:?--version needs a value}"; shift ;;
    --assets-dir)  ASSETS_DIR="${2:?--assets-dir needs a value}"; shift ;;
    --output-dir)  OUTPUT_DIR="${2:?--output-dir needs a value}"; shift ;;
    -h|--help)     sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)             die "unknown argument '$1'" ;;
  esac
  shift
done

[[ -n "$VERSION" ]] || die "--version is required"
# Strip leading 'v' for fields that must not include it (winget PackageVersion,
# homebrew version). URLs keep the original VERSION (with v-tag).
VERSION_NOV="${VERSION#v}"
mkdir -p "$OUTPUT_DIR"

# --- SHA-256 helper ----------------------------------------------------------
sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  else
    die "no SHA-256 tool found (sha256sum or shasum required)"
  fi
}

# --- Collect canonical assets ------------------------------------------------
cd "$ASSETS_DIR"
shopt -s nullglob
ASSETS=( ccync-"${VERSION}"-* )
shopt -u nullglob
[[ ${#ASSETS[@]} -gt 0 ]] || die "no assets matching ccync-${VERSION}-* in ${ASSETS_DIR}"

# --- checksums.txt -----------------------------------------------------------
CHECKSUMS="${OUTPUT_DIR}/checksums.txt"
: > "$CHECKSUMS"
for f in "${ASSETS[@]}"; do
  printf '%s  %s\n' "$(sha256_file "$f")" "$f" >> "$CHECKSUMS"
done
echo "gen-release-artifacts: wrote $CHECKSUMS (${#ASSETS[@]} assets)"

# Look up a sha by exact asset filename from the freshly written checksums.txt.
sha_of() {
  local name="$1" sha
  sha="$(awk -v n="$name" '$2 == n {print $1}' "$CHECKSUMS")"
  [[ -n "$sha" ]] || die "no checksum entry for '$name' (asset missing from build?)"
  echo "$sha"
}

# --- artifact-manifest.json --------------------------------------------------
MANIFEST="${OUTPUT_DIR}/artifact-manifest.json"
{
  printf '{\n  "version": "%s",\n  "assets": [\n' "$VERSION"
  first=1
  for f in "${ASSETS[@]}"; do
    [[ $first -eq 1 ]] || printf ',\n'
    first=0
    printf '    { "name": "%s", "sha256": "%s" }' "$f" "$(sha256_file "$f")"
  done
  printf '\n  ]\n}\n'
} > "$MANIFEST"
echo "gen-release-artifacts: wrote $MANIFEST"

# --- Template fill helper ----------------------------------------------------
# render <template> <output> KEY=VAL KEY=VAL ...
render() {
  local tmpl="$1" out="$2"; shift 2
  local content; content="$(cat "$tmpl")"
  local pair key val
  for pair in "$@"; do
    key="${pair%%=*}"; val="${pair#*=}"
    content="${content//\{\{${key}\}\}/${val}}"
  done
  printf '%s\n' "$content" > "$out"
}

WINGET_DIR="${SCRIPT_DIR}/winget"
HOMEBREW_DIR="${SCRIPT_DIR}/homebrew"

# --- winget manifests --------------------------------------------------------
if [[ -d "$WINGET_DIR" ]]; then
  SHA_WIN_X64="$(sha_of "ccync-${VERSION}-windows-x64.zip")"
  SHA_WIN_ARM64="$(sha_of "ccync-${VERSION}-windows-arm64.zip")"
  render "${WINGET_DIR}/Monkey1Wizard.ccync.yaml.template" \
         "${OUTPUT_DIR}/Monkey1Wizard.ccync.yaml" \
         "VERSION=${VERSION}" \
         "PACKAGE_VERSION=${VERSION_NOV}"
  render "${WINGET_DIR}/Monkey1Wizard.ccync.locale.en-US.yaml.template" \
         "${OUTPUT_DIR}/Monkey1Wizard.ccync.locale.en-US.yaml" \
         "VERSION=${VERSION}" \
         "PACKAGE_VERSION=${VERSION_NOV}"
  render "${WINGET_DIR}/Monkey1Wizard.ccync.installer.yaml.template" \
         "${OUTPUT_DIR}/Monkey1Wizard.ccync.installer.yaml" \
         "VERSION=${VERSION}" \
         "PACKAGE_VERSION=${VERSION_NOV}" \
         "SHA256_WIN_X64=${SHA_WIN_X64}" \
         "SHA256_WIN_ARM64=${SHA_WIN_ARM64}"
  echo "gen-release-artifacts: wrote winget manifests"
fi

# --- homebrew formula --------------------------------------------------------
if [[ -f "${HOMEBREW_DIR}/ccync.rb.template" ]]; then
  render "${HOMEBREW_DIR}/ccync.rb.template" \
         "${OUTPUT_DIR}/ccync.rb" \
         "VERSION=${VERSION}" \
         "PACKAGE_VERSION=${VERSION_NOV}" \
         "SHA256_DARWIN_X64=$(sha_of "ccync-${VERSION}-darwin-x64.tar.gz")" \
         "SHA256_DARWIN_ARM64=$(sha_of "ccync-${VERSION}-darwin-arm64.tar.gz")" \
         "SHA256_LINUX_X64=$(sha_of "ccync-${VERSION}-linux-x64.tar.gz")" \
         "SHA256_LINUX_ARM64=$(sha_of "ccync-${VERSION}-linux-arm64.tar.gz")"
  echo "gen-release-artifacts: wrote ccync.rb"
fi

echo "gen-release-artifacts: done → ${OUTPUT_DIR}"
