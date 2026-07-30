# ccync — cross-agent plugin / MCP / skills manager installer (Windows)
# Usage: irm https://raw.githubusercontent.com/monkey1wizard/ccync/main/packaging/install.ps1 | iex
#
# [BETA] Windows installation has not yet been validated on a clean machine.
# Please report any issues at: https://github.com/monkey1wizard/ccync/issues
#
# Integrity model:
#   - SHA-256 check against checksums.txt is MANDATORY (aborts on mismatch)
#   - cosign verify-blob is BEST-EFFORT (warns if cosign absent, never blocks)
#
# Non-interactive: fully non-interactive (stdin may be a pipe). No prompts.
# Beta notice written as warning text via the host/information stream.
#
# Testability: the helper functions below are pure and dependency-free so that
# packaging/test-install.ps1 can dot-source this file (dot-sourcing skips the
# main install procedure — see the guard at the bottom) and exercise them
# directly, without any network or filesystem side effects.

#Requires -Version 5.1
[CmdletBinding()]
param()

# NOTE: $ErrorActionPreference (and any other preference) is set *inside* the
# Invoke-CcyncInstall function body below, never at script scope. Under
# `irm | iex` the script scope IS the caller's scope, so a script-scope
# preference assignment would leak into and mutate the caller's shell. The
# function's child scope keeps it local to the installer.

$Repo   = 'monkey1wizard/ccync'
$InstallDir = Join-Path $env:USERPROFILE '.local\bin'

# Output semantics:
#   Write-Info  — normal progress to the host.
#   Write-Warn  — plain warning text via Write-Host (host/information stream):
#                 NOT a PowerShell error record, so it never pollutes the
#                 caller's $Error or looks like a failure — and unlike
#                 [Console]::Error it rides the stream system, so `*>&1` and
#                 Start-Transcript capture it (a Tee'd install log must not
#                 silently omit the cosign warnings) and it still renders in
#                 console-less hosts.
#   Write-Fatal — plain error text to stderr, then `throw` (a catchable terminating
#                 error). It never calls `exit`, which under `irm | iex` would kill
#                 the caller's whole shell instead of just aborting the installer.
function Write-Info  { param([string]$Msg) Write-Host "ccync-install: $Msg" }
function Write-Warn  { param([string]$Msg) Write-Host "ccync-install: warning: $Msg" -ForegroundColor Yellow }
function Write-Fatal { param([string]$Msg) [Console]::Error.WriteLine("ccync-install: error: $Msg"); throw "ccync-install: $Msg" }

# --- PATH hint ---------------------------------------------------------------
# Build ONE copy-paste-runnable command that appends only $InstallDir to the
# registry-backed *User* PATH. It reads the User PATH at run time via
# GetEnvironmentVariable(...,'User') so it never persists the merged process
# $env:PATH (which would copy machine + session PATH into the User scope). The
# install dir is embedded as a single-quoted literal so paths containing spaces
# round-trip intact, and the command is a single interpolated string with no
# literal concatenation fragment.
#
# The literal is escaped for single-quoted PowerShell syntax: inside '...' an
# apostrophe is written as ''. Windows account names may contain apostrophes
# (C:\Users\O'Brien\.local\bin), which would otherwise terminate the literal and
# turn the emitted command into a parse error.
function Get-CcyncPathHint {
    param([Parameter(Mandatory)][string]$InstallDir)
    $Literal = $InstallDir -replace "'", "''"
    return "[System.Environment]::SetEnvironmentVariable('PATH', ([System.Environment]::GetEnvironmentVariable('PATH','User') + ';$Literal'), 'User')"
}

# --- runtime version gate ----------------------------------------------------
# Pure comparison so the guard is testable without an actual old PowerShell.
function Test-CcyncMinVersion {
    param(
        [Parameter(Mandatory)][version]$Detected,
        [version]$Minimum = [version]'5.1'
    )
    return $Detected -ge $Minimum
}

# --- detect architecture -----------------------------------------------------

function Get-CcyncArch {
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64'  { return 'x64'   }
        'ARM64'  { return 'arm64' }
        default  {
            Write-Fatal "Unsupported architecture: $($env:PROCESSOR_ARCHITECTURE). Supported: AMD64, ARM64.`nPlease download the binary manually from: https://github.com/$Repo/releases"
        }
    }
}

# --- fetch latest version tag ------------------------------------------------

function Get-LatestVersion {
    try {
        $response = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing
        if (-not $response.tag_name) { throw 'empty tag_name' }
        return $response.tag_name
    } catch {
        Write-Fatal "Failed to fetch latest release version from GitHub API: $_"
    }
}

# --- SHA-256 helper ----------------------------------------------------------

function Get-FileSha256 {
    param([string]$Path)
    $hash = Get-FileHash -Path $Path -Algorithm SHA256
    return $hash.Hash.ToLower()
}

# --- main --------------------------------------------------------------------
# The executable body lives in a function so its preference assignments stay in a
# child scope and never leak into an `irm | iex` caller's shell (under `irm | iex`
# the script scope IS the caller's scope).
#
# Both parameters are test seams, and they are *parameters* rather than env vars
# precisely so the shipped `irm | iex` path cannot reach them: the invocation at
# the bottom of this file passes neither, so an environment variable can no
# longer skip the install or spoof the runtime gate. packaging/test-install.ps1
# dot-sources this file (which skips the invocation below — see the guard at the
# bottom) and calls this function directly with the seam it needs.
function Invoke-CcyncInstall {
    param(
        # Runtime version the gate evaluates. Defaults to the real runtime.
        [version]$DetectedPsVersion = $PSVersionTable.PSVersion,
        # Return right after the preference assignment, before any network or
        # filesystem work, so preference isolation can be observed on its own.
        [switch]$StopAfterPreference
    )
    $ErrorActionPreference = 'Stop'

    if ($StopAfterPreference) { return }

    # Explicit runtime version gate. `#Requires -Version 5.1` (above) protects a
    # direct `-File` invocation, but it is silently ignored when this script is
    # piped to `iex`, so enforce the minimum here — before any network or
    # filesystem work.
    if (-not (Test-CcyncMinVersion -Detected $DetectedPsVersion)) {
        Write-Fatal "ccync requires PowerShell 5.1 or newer (detected $DetectedPsVersion). Upgrade PowerShell, then re-run the installer."
    }

    # Beta notice — plain warning text on the information stream, non-interactive
    Write-Warn '[BETA] Windows support has not yet been validated on a clean machine.'
    Write-Warn 'Proceeding with installation. Report issues at: https://github.com/monkey1wizard/ccync/issues'

    $Arch    = Get-CcyncArch
    $Platform = 'windows'
    Write-Info "Detected platform: $Platform-$Arch"

    $Version = if ($env:CCYNC_VERSION) { $env:CCYNC_VERSION } else { Get-LatestVersion }
    Write-Info "Installing ccync $Version"

    $ArchiveName = "ccync-$Version-$Platform-$Arch.zip"
    $BaseUrl     = "https://github.com/$Repo/releases/download/$Version"

    $TmpDir = Join-Path $env:TEMP "ccync-install-$(New-Guid)"
    New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null

    try {
        # Download archive
        Write-Info "Downloading $ArchiveName..."
        $ArchivePath = Join-Path $TmpDir $ArchiveName
        try {
            Invoke-WebRequest -Uri "$BaseUrl/$ArchiveName" -OutFile $ArchivePath -UseBasicParsing
        } catch {
            Write-Fatal "Download failed: $BaseUrl/$ArchiveName`n$_"
        }

        # Download checksums.txt
        Write-Info 'Downloading checksums.txt...'
        $ChecksumsPath = Join-Path $TmpDir 'checksums.txt'
        try {
            Invoke-WebRequest -Uri "$BaseUrl/checksums.txt" -OutFile $ChecksumsPath -UseBasicParsing
        } catch {
            Write-Fatal "Download failed: $BaseUrl/checksums.txt`n$_"
        }

        # --- Mandatory SHA-256 verification --------------------------------------
        Write-Info 'Verifying SHA-256 integrity...'
        $ChecksumLines = Get-Content $ChecksumsPath
        $ExpectedLine  = $ChecksumLines | Where-Object { $_ -match [regex]::Escape($ArchiveName) } | Select-Object -First 1
        if (-not $ExpectedLine) {
            Write-Fatal "Checksum entry not found for '$ArchiveName' in checksums.txt."
        }
        $Expected = ($ExpectedLine -split '\s+')[0].ToLower()
        $Actual   = Get-FileSha256 $ArchivePath

        if ($Expected -ne $Actual) {
            Write-Fatal "SHA-256 mismatch — download may be corrupted or tampered.`n  Expected: $Expected`n  Actual:   $Actual`nAborting installation."
        }
        Write-Info "SHA-256 OK ($($Actual.Substring(0,16))...)"

        # --- cosign best-effort verification -------------------------------------
        $CosignExe = Get-Command 'cosign' -ErrorAction SilentlyContinue
        if ($CosignExe) {
            Write-Info 'cosign found — downloading signature files...'
            $SigPath  = Join-Path $TmpDir 'checksums.txt.sig'
            $CertPath = Join-Path $TmpDir 'checksums.txt.pem'
            try {
                Invoke-WebRequest -Uri "$BaseUrl/checksums.txt.sig" -OutFile $SigPath  -UseBasicParsing
                Invoke-WebRequest -Uri "$BaseUrl/checksums.txt.pem" -OutFile $CertPath -UseBasicParsing
                & cosign verify-blob `
                    --signature $SigPath `
                    --certificate $CertPath `
                    --certificate-identity-regexp "https://github.com/$Repo/\.github/workflows/release\.yml@.*" `
                    --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' `
                    $ChecksumsPath 2>$null
                if ($LASTEXITCODE -eq 0) {
                    Write-Info 'cosign signature verified.'
                } else {
                    Write-Warn 'cosign verification failed — proceeding (SHA-256 passed).'
                }
            } catch {
                Write-Warn "Could not complete cosign verification — skipping (SHA-256 passed)."
            }
        } else {
            Write-Warn 'cosign not found in PATH — skipping cosign verification (SHA-256 passed).'
            Write-Warn 'Install cosign from https://docs.sigstore.dev/cosign/system_config/installation/ for full verification.'
        }

        # --- Extract and install -------------------------------------------------
        Write-Info 'Extracting archive...'
        $ExtractDir = Join-Path $TmpDir 'extract'
        Expand-Archive -Path $ArchivePath -DestinationPath $ExtractDir -Force

        $BinaryName = 'ccync.exe'
        $Binary = Get-ChildItem -Path $ExtractDir -Filter $BinaryName -Recurse | Select-Object -First 1
        if (-not $Binary) {
            Write-Fatal "Binary '$BinaryName' not found in archive."
        }

        if (-not (Test-Path $InstallDir)) {
            New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        }
        Copy-Item -Path $Binary.FullName -Destination (Join-Path $InstallDir $BinaryName) -Force

        # --- Verify install ------------------------------------------------------
        $CcyncBin = Join-Path $InstallDir $BinaryName
        $CcyncVersionOut = & $CcyncBin --version 2>&1

        Write-Host ''
        Write-Host 'ccync installed successfully.'
        Write-Host "  Location: $CcyncBin"
        Write-Host "  Version:  $CcyncVersionOut"
        Write-Host ''

        # PATH reminder — emit one copy-paste-runnable command that appends only
        # the install dir to the registry User PATH (never the merged process PATH).
        $CurrentPath = [System.Environment]::GetEnvironmentVariable('PATH', 'User')
        if ($CurrentPath -notlike "*$InstallDir*") {
            Write-Host "Add $InstallDir to your User PATH (run once, then reopen your shell):"
            Write-Host "  $(Get-CcyncPathHint -InstallDir $InstallDir)"
            Write-Host ''
        }

        Write-Host 'Next steps:'
        Write-Host '  ccync init        -- adopt a master agent (claude/codex) and project to all agents'
        Write-Host '  ccync --help      -- show all commands'
        Write-Host ''
        Write-Host "Documentation: https://github.com/$Repo"
        Write-Host ''
        Write-Host '[BETA] Please report any issues at: https://github.com/monkey1wizard/ccync/issues' -ForegroundColor Yellow

    } finally {
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    }
}

# Guarded so the self-test harness can dot-source the helpers above without
# triggering any network/filesystem work. The guard is dot-source detection, not
# an environment variable: an env var here would be live in the shipped
# `irm | iex` path and could silently no-op the install (exit 0, nothing
# installed).
#
# InvocationName alone is NOT enough, because it describes how the *enclosing*
# scope was entered, not how this content was loaded. Under `iex` the enclosing
# scope's values are inherited verbatim, so a host script that is itself
# dot-sourced makes this content look dot-sourced too — which is exactly the
# shape of a pwsh CI step (`pwsh -command ". '<step>.ps1'"` running
# `$s = irm …; iex $s`). Guarding on InvocationName alone silently skipped the
# whole install there: exit 0, nothing installed, no output — the same failure
# mode the env-var seam was rejected for.
#
# Observed values, all three shapes:
#   dot-sourcing this file      InvocationName='.'  MyCommand.Name='install.ps1'
#   iex inside dot-sourced host InvocationName='.'  MyCommand.Name='<host>.ps1'
#   irm | iex                   InvocationName=''   MyCommand.Name=''
#
# So the skip requires BOTH: dot-source syntax AND this file being the command
# that was dot-sourced. Every other shape runs the real install, and no test
# seam is passed here — the shipped path always installs against the real runtime.
$dotSourcedAsInstaller =
    $MyInvocation.InvocationName -eq '.' -and
    $MyInvocation.MyCommand.Name -eq 'install.ps1'

if (-not $dotSourcedAsInstaller) {
    Invoke-CcyncInstall
}
