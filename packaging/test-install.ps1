# packaging/test-install.ps1 — dependency-free self-test harness for install.ps1.
#
# Dot-sources install.ps1 (the installer's bottom guard detects dot-sourcing via
# $MyInvocation.InvocationName and skips the main install procedure), which
# defines the pure helper functions, then exercises those helpers with no network
# or filesystem side effects.
#
# Usage:
#   powershell.exe -NoProfile -File packaging/test-install.ps1 -Case PathHint
#   powershell.exe -NoProfile -File packaging/test-install.ps1 -Case All
#
# Each case prints exactly one `PASS <Case>` line on success (and `FAIL <Case>` on
# failure). The harness exits non-zero if any selected case failed.

[CmdletBinding()]
param(
    [ValidateSet('PathHint', 'PathHintQuoting', 'CallerPreferences', 'OutputSemantics', 'VersionGuard', 'All')]
    [string]$Case = 'All'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ScriptDir  = Split-Path -Parent $MyInvocation.MyCommand.Path
$InstallPs1 = Join-Path $ScriptDir 'install.ps1'

$script:AnyFailed = $false

# Dot-source at script scope: defines the installer's pure helpers (visible to
# the case functions below) but skips the main procedure — the installer's bottom
# guard detects dot-sourcing itself. No side effects, no env var required.
. $InstallPs1

function Assert-Cond {
    param([bool]$Cond, [string]$Because)
    if (-not $Cond) {
        Write-Host "  assert failed: $Because"
        $script:CaseFailed = $true
    }
}

function Complete-Case {
    param([string]$Name)
    if ($script:CaseFailed) {
        Write-Host "FAIL $Name"
        $script:AnyFailed = $true
    } else {
        Write-Host "PASS $Name"
    }
}

# --- PathHint ----------------------------------------------------------------
# Shared shape + round-trip assertions for one install directory. The round-trip
# actually *parses and evaluates* the emitted command's value expression, so any
# directory whose characters break out of the single-quoted literal is caught
# here rather than in the user's shell.
function Assert-PathHintRoundTrip {
    param([Parameter(Mandatory)][string]$Dir)

    $cmd = Get-CcyncPathHint -InstallDir $Dir

    Assert-Cond ($cmd -is [string])                              'hint is a string'
    Assert-Cond (-not ($cmd -match "`n"))                        'single command (no newline)'
    Assert-Cond ($cmd -match "GetEnvironmentVariable\('PATH','User'\)") 'reads the registry User PATH'
    Assert-Cond ($cmd -match "SetEnvironmentVariable\('PATH'")   'writes the User PATH'
    Assert-Cond ($cmd -match ", 'User'\)")                       'targets the User scope'
    Assert-Cond (-not ($cmd -match '\$env:PATH'))                'does not persist the merged process $env:PATH'
    Assert-Cond (-not ($cmd -match '\+\s*\$InstallDir\s*\+'))    'no literal concatenation fragment'

    # Evaluate the command's value expression against a simulated User PATH that
    # contains spaces, and confirm only the install dir was appended — verbatim.
    $m = [regex]::Match($cmd, "SetEnvironmentVariable\('PATH', \((.+)\), 'User'\)")
    Assert-Cond ($m.Success) 'command has the expected Set-User-PATH shape'
    if ($m.Success) {
        $valueExpr = $m.Groups[1].Value
        $simUser   = 'C:\Windows;C:\Program Files\Git\bin'
        $mocked    = $valueExpr -replace [regex]::Escape("[System.Environment]::GetEnvironmentVariable('PATH','User')"), "'$simUser'"
        $newPath   = $null
        try {
            $newPath = Invoke-Expression $mocked
        } catch {
            Assert-Cond $false "emitted command parses and runs for '$Dir' ($($_.Exception.Message))"
        }
        Assert-Cond ($newPath -eq ($simUser + ';' + $Dir)) "round-trips '$Dir', appending only the install dir"
    }

    return $cmd
}

function Test-PathHint {
    $script:CaseFailed = $false

    $dir = 'C:\Users\Test User\.local\bin'   # deliberately contains a space
    $cmd = Assert-PathHintRoundTrip -Dir $dir
    Assert-Cond ($cmd -match [regex]::Escape("';$dir'")) 'appends only the install dir as a quoted literal'

    Complete-Case 'PathHint'
}

# --- PathHintQuoting ---------------------------------------------------------
# Windows account names permit apostrophes (C:\Users\O'Brien\...). An unescaped
# apostrophe terminates the single-quoted literal the hint embeds the install dir
# in, so the copy-pasted command becomes a parse error. The literal must escape
# ' as '' — and the round-trip above proves the emitted command still evaluates.
function Test-PathHintQuoting {
    $script:CaseFailed = $false

    $dir = "C:\Users\O'Brien\.local\bin"
    $cmd = Assert-PathHintRoundTrip -Dir $dir

    Assert-Cond ($cmd -match [regex]::Escape("';C:\Users\O''Brien\.local\bin'")) `
        "apostrophe is doubled inside the single-quoted literal"
    Assert-Cond (-not ($cmd -match [regex]::Escape("';$dir'"))) `
        'the raw (unescaped) apostrophe form is not emitted'

    Complete-Case 'PathHintQuoting'
}

# --- CallerPreferences -------------------------------------------------------
# Verifies that running the installer the way `irm | iex` does (same-scope
# execution) leaves the caller's preference variables unchanged, because the
# installer sets its preferences inside Invoke-CcyncInstall's child scope.
# -StopAfterPreference returns right after the assignment, so no network or
# filesystem work runs.
function Test-CallerPreferences {
    $script:CaseFailed = $false

    # Distinctive caller value, different from the installer's own 'Stop'; a leak
    # would overwrite this with 'Stop'.
    $ErrorActionPreference = 'Continue'
    $before = $ErrorActionPreference

    # (1) Dot-sourcing install.ps1 executes it in THIS scope, exactly as
    #     `irm | iex` does. The file must carry no script-scope preference
    #     assignment, so the caller's value survives verbatim.
    . $InstallPs1
    Assert-Cond ($ErrorActionPreference -eq $before) `
        "dot-sourcing install.ps1 does not touch caller ErrorActionPreference (was '$before', now '$ErrorActionPreference')"

    # (2) The installer body does set $ErrorActionPreference = 'Stop'; that
    #     assignment must stay inside the function's child scope.
    Invoke-CcyncInstall -StopAfterPreference
    $after = $ErrorActionPreference

    Assert-Cond ($after -eq $before) "caller ErrorActionPreference unchanged (was '$before', now '$after')"

    # (3) Every install seam is a parameter (or dot-source detection), never an
    #     environment variable: any $env:CCYNC_INSTALL* read would be live in
    #     the shipped `irm | iex` path and could silently no-op the install
    #     (exit 0, nothing installed) or spoof a gate.
    $source = Get-Content $InstallPs1 -Raw
    Assert-Cond (-not ($source -match '\$env:CCYNC_INSTALL')) `
        'no env-var seam can short-circuit the shipped install path'

    Complete-Case 'CallerPreferences'
}

# --- OutputSemantics ---------------------------------------------------------
# Verifies Write-Warn emits no error record and Write-Fatal throws a catchable
# terminating error (so the caller shell survives), rather than emitting an error
# record or calling `exit`.
function Test-OutputSemantics {
    $script:CaseFailed = $false

    # Warning: plain text on the host/information stream, no PowerShell error record.
    $errBefore = $Error.Count
    Write-Warn 'sample warning'
    $errAfter = $Error.Count
    Assert-Cond ($errAfter -eq $errBefore) "Write-Warn adds no error record (before=$errBefore after=$errAfter)"

    # Loggable: the warning must ride PowerShell's stream system, so that
    # `*>&1 | Tee-Object install.log` and Start-Transcript capture it.
    # ([Console]::Error bypassed both — a Tee'd log of a clean-looking install
    # silently omitted the cosign warnings.)
    $captured = Write-Warn 'sample capturable warning' *>&1 | Out-String
    Assert-Cond ($captured -match 'ccync-install: warning: sample capturable warning') `
        "Write-Warn output is capturable via *>&1 (got '$($captured.Trim())')"

    # Fatal: catchable terminating error; execution continuing past the catch to
    # run these assertions is itself the proof the caller shell was not killed
    # by an `exit`.
    $threw = $false
    $caught = $null
    try {
        Write-Fatal 'sample fatal'
    } catch {
        $threw = $true
        $caught = $_
    }
    Assert-Cond $threw 'Write-Fatal throws a catchable terminating error (not exit — this line still ran)'
    Assert-Cond ($null -ne $caught -and $caught.Exception.Message -eq 'ccync-install: sample fatal') `
        'the caught error carries the fatal message (a throw, not an exit code path)'

    Complete-Case 'OutputSemantics'
}

# --- VersionGuard ------------------------------------------------------------
# Verifies the pure version classifier and that a mocked unsupported runtime
# aborts the installer before any download/filesystem work.
function Test-VersionGuard {
    $script:CaseFailed = $false

    # Pure classifier (defined by the top-level dot-source).
    Assert-Cond (-not (Test-CcyncMinVersion -Detected ([version]'5.0'))) 'PS 5.0 is rejected'
    Assert-Cond (Test-CcyncMinVersion -Detected ([version]'5.1'))        'PS 5.1 is accepted'
    Assert-Cond (Test-CcyncMinVersion -Detected ([version]'7.4'))        'PS 7.4 is accepted'

    # Body-level: a mocked unsupported version must throw before any network/fs.
    # Local shadows of the network/fs cmdlets act as call counters (the guard
    # runs strictly before them, so they must stay at zero).
    $script:dlCount = 0
    $script:mkCount = 0
    function Invoke-WebRequest { param([Parameter(ValueFromRemainingArguments = $true)]$Rest) $script:dlCount++ }
    function Invoke-RestMethod { param([Parameter(ValueFromRemainingArguments = $true)]$Rest) $script:dlCount++ }
    function New-Item          { param([Parameter(ValueFromRemainingArguments = $true)]$Rest) $script:mkCount++ }

    $threw = $false
    try {
        Invoke-CcyncInstall -DetectedPsVersion ([version]'5.0')
    } catch {
        $threw = $true
    }
    Assert-Cond $threw                   'unsupported runtime aborts the installer (throws)'
    Assert-Cond ($script:dlCount -eq 0)  "no download attempted (dlCount=$script:dlCount)"
    Assert-Cond ($script:mkCount -eq 0)  "no directory/file write attempted (mkCount=$script:mkCount)"

    # The detected-version seam is a parameter, never an environment variable:
    # an env var read here would let anyone spoof the runtime gate in the
    # shipped `irm | iex` path.
    $source = Get-Content $InstallPs1 -Raw
    Assert-Cond (-not ($source -match 'CCYNC_INSTALL_MOCK_PSVERSION')) `
        'no env-var seam can override the shipped runtime gate'

    Complete-Case 'VersionGuard'
}

# --- dispatch ----------------------------------------------------------------
switch ($Case) {
    'PathHint'          { Test-PathHint }
    'PathHintQuoting'   { Test-PathHintQuoting }
    'CallerPreferences' { Test-CallerPreferences }
    'OutputSemantics'   { Test-OutputSemantics }
    'VersionGuard'      { Test-VersionGuard }
    'All'               { Test-PathHint; Test-PathHintQuoting; Test-CallerPreferences; Test-OutputSemantics; Test-VersionGuard }
    default             { Write-Host "Case '$Case' not yet implemented"; $script:AnyFailed = $true }
}

if ($script:AnyFailed) { exit 1 } else { exit 0 }
