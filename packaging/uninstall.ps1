# ccync — cross-agent plugin / MCP / skills manager uninstaller (Windows)
# Usage: irm https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/uninstall.ps1 | iex
#
# Removes the ccync binary and runs `ccync uninstall` to clear derived state and
# de-project ccync-owned entries from live agent configs. Preserves the user's
# input files (~/.ccync/config.json and ~/.ccync/plugins.json), the shared
# ~/.local/bin directory, and the user PATH — none of those are modified.

$ErrorActionPreference = 'Stop'
$InstallDir = Join-Path $env:USERPROFILE '.local\bin'
$CcyncBin = Join-Path $InstallDir 'ccync.exe'

function Write-Info { param([string]$Msg) Write-Host "ccync-uninstall: $Msg" }
function Write-Warn { param([string]$Msg) Write-Error "ccync-uninstall: warning: $Msg" -ErrorAction Continue }

# 1. Run the CLI uninstall first (clears derived ~/.ccync state + live
#    de-projection). Best-effort: a partial/failed run still proceeds to binary
#    removal so a wedged install can always be cleaned up.
$ccyncCmd = Get-Command ccync -ErrorAction SilentlyContinue
if ($ccyncCmd) {
    try { & ccync uninstall } catch { Write-Warn 'ccync uninstall reported a partial/failed run (continuing to binary removal).' }
}
elseif (Test-Path $CcyncBin) {
    try { & $CcyncBin uninstall } catch { Write-Warn 'ccync uninstall reported a partial/failed run (continuing to binary removal).' }
}
else {
    Write-Warn "ccync not found on PATH or in $InstallDir; skipping CLI uninstall."
}

# 2. Remove ONLY the ccync binary. The shared $InstallDir directory is left in
#    place (other tools may live there) and the user PATH is not edited.
if (Test-Path $CcyncBin) {
    Remove-Item -Path $CcyncBin -Force
    Write-Info "Removed $CcyncBin"
}
else {
    Write-Info "No ccync binary at $CcyncBin (already removed)."
}

Write-Info 'Done. Preserved your input files: ~/.ccync/config.json and ~/.ccync/plugins.json.'
Write-Info "Left in place: $InstallDir (directory) and your user PATH (not modified)."
