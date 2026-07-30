#!/usr/bin/env bash
# ccync — cross-agent plugin / MCP / skills manager uninstaller
# Usage: curl -fsSL https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/uninstall.sh | bash
#
# Removes the ccync binary and runs `ccync uninstall` to clear derived state and
# de-project ccync-owned entries from live agent configs. Preserves the user's
# input files (~/.ccync/config.json and ~/.ccync/plugins.json), the shared
# ~/.local/bin directory, and the shell PATH — none of those are modified.

set -euo pipefail

INSTALL_DIR="${HOME}/.local/bin"
CCYNC_BIN="${INSTALL_DIR}/ccync"

warn() { echo "ccync-uninstall: warning: $*" >&2; }
info() { echo "ccync-uninstall: $*"; }

# 1. Run the CLI uninstall first (clears derived ~/.ccync state + live
#    de-projection). Best-effort: a partial/warning result still proceeds to
#    binary removal so a wedged install can always be cleaned up.
if command -v ccync >/dev/null 2>&1; then
    ccync uninstall || warn "ccync uninstall reported a partial/failed run (continuing to binary removal)."
elif [ -x "${CCYNC_BIN}" ]; then
    "${CCYNC_BIN}" uninstall || warn "ccync uninstall reported a partial/failed run (continuing to binary removal)."
else
    warn "ccync not found on PATH or in ${INSTALL_DIR}; skipping CLI uninstall."
fi

# 2. Remove ONLY the ccync binary. The shared ${INSTALL_DIR} directory is left
#    in place (other tools may live there) and the shell PATH is not edited.
if [ -f "${CCYNC_BIN}" ]; then
    rm -f "${CCYNC_BIN}"
    info "Removed ${CCYNC_BIN}"
else
    info "No ccync binary at ${CCYNC_BIN} (already removed)."
fi

info "Done. Preserved your input files: ~/.ccync/config.json and ~/.ccync/plugins.json."
info "Left in place: ${INSTALL_DIR} (directory) and your shell PATH (not modified)."
