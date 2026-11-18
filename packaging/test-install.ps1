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
    [ValidateSet('PathHint', 'PathHintQuoting', 'CallerPreferences', 'OutputSemantics', 'VersionGuard', 'InstallReceipt', 'DotSourceGuard', 'All')]
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

# --- InstallReceipt ---------------------------------------------------------
# The receipt is versioned JSON and must hash the installed target bytes.
function Test-InstallReceipt {
    $script:CaseFailed = $false
    $tmp = Join-Path $env:TEMP "ccync-receipttest-$(New-Guid)"
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    try {
        $target = Join-Path $tmp 'ccync.exe'
        $bytes = [System.Text.Encoding]::UTF8.GetBytes('receipt-test-binary')
        [System.IO.File]::WriteAllBytes($target, $bytes)
        $receiptDir = Join-Path $tmp '.ccync\install-receipts'
        Write-CcyncInstallReceipt -ReceiptDir $receiptDir -Target $target -Version 'v1.2.3'

        $receiptPath = Join-Path $receiptDir 'ccync.json'
        $receipt = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json
        Assert-Cond ($receipt.schemaVersion -eq 1) 'receipt uses schema version 1'
        Assert-Cond ($receipt.target -eq [System.IO.Path]::GetFullPath($target)) 'receipt records the canonical installed target'
        Assert-Cond ($receipt.version -eq 'v1.2.3') 'receipt records the release version'
        Assert-Cond ($receipt.releaseSource -eq 'github-release') 'receipt records the first-party release source'
        Assert-Cond ($receipt.sha256 -eq (Get-FileSha256 -Path $target)) 'receipt hash matches installed target bytes'
        Assert-Cond (@(Get-ChildItem -LiteralPath $receiptDir -Filter '*.tmp').Count -eq 0) 'temporary receipt was renamed away'
        # Forced failure before copy leaves no receipt
        $failedHome = Join-Path $tmp 'failed-home'
        $failedReceipt = Join-Path $failedHome '.ccync\install-receipts\ccync.json'
        $oldProfile = $env:USERPROFILE
        $oldVer = $env:CCYNC_VERSION
        try {
            $env:USERPROFILE = $failedHome
            $env:CCYNC_VERSION = 'v1.2.3'
            $script:threwBeforeCopy = $false
            & {
                function Invoke-WebRequest { throw 'simulated download failure before copy' }
                try {
                    Invoke-CcyncInstall
                } catch {
                    $script:threwBeforeCopy = $true
                }
            }
            Assert-Cond $script:threwBeforeCopy 'installer threw on simulated download failure before copy'
            Assert-Cond (-not (Test-Path -LiteralPath $failedReceipt)) 'forced failure before copy leaves no receipt'
        } finally {
            $env:USERPROFILE = $oldProfile
            if ($null -ne $oldVer) { $env:CCYNC_VERSION = $oldVer } else { Remove-Item Env:\CCYNC_VERSION -ErrorAction SilentlyContinue }
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
    Complete-Case 'InstallReceipt'
}

# --- DotSourceGuard ----------------------------------------------------------
# The bottom guard decides whether the shipped install actually runs. Guarding on
# $MyInvocation.InvocationName alone was not enough: under `iex` the enclosing
# scope's invocation values are inherited, so a dot-sourced host script made the
# installer look dot-sourced and the whole install was skipped — silently, exit 0,
# nothing installed. That is exactly the shape of a pwsh CI step, and it is what
# turned the Windows installer smoke red.
#
# Each shape runs in a child process (the guard fires at load time, so it cannot
# be re-observed in this one). Network is stubbed in the child and CCYNC_VERSION
# is pinned, so no shape reaches the network — the assertion is only on whether
# the installer's own opening output appears.
function Test-DotSourceGuard {
    $script:CaseFailed = $false

    $pwshPath = [Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
    $tmp      = Join-Path $env:TEMP "ccync-guardtest-$(New-Guid)"
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null

    # Stubs + pinned version, prepended in the child so the real installer body
    # can start but can never reach the network.
    $preamble = @"
`$env:CCYNC_VERSION = 'v0.0.0-guardtest'
function Invoke-WebRequest { throw 'network blocked by test' }
function Invoke-RestMethod { throw 'network blocked by test' }
"@

    function Invoke-Shape {
        param([string]$Body)
        $hostPs1 = Join-Path $tmp "host-$(New-Guid).ps1"
        Set-Content -Path $hostPs1 -Value "$preamble`ntry {`n$Body`n} catch { }" -Encoding UTF8
        # `. '<host>'` is how a pwsh CI step runs its generated step script. The
        # path is embedded in a single-quoted literal, so double any apostrophe —
        # $env:TEMP sits under the account name, and an account like O'Brien would
        # otherwise terminate the literal and make the child silently not run,
        # which this case would misread as "the install did not run".
        $hostLiteral = $hostPs1 -replace "'", "''"
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            (& $pwshPath -NoProfile -command ". '$hostLiteral'" 2>&1) -join "`n"
        } finally {
            $ErrorActionPreference = $prevEap
        }
    }

    $iexInsideDotSourcedHost = Invoke-Shape "`$s = Get-Content '$InstallPs1' -Raw; Invoke-Expression `$s"
    $dotSourcedInstaller     = Invoke-Shape ". '$InstallPs1'"
    $plainIex                = Invoke-Shape "`$s = Get-Content '$InstallPs1' -Raw; & { Invoke-Expression `$s }"

    Assert-Cond ($iexInsideDotSourcedHost -match 'ccync-install:') `
        'iex inside a dot-sourced host still runs the install (the CI step shape)'
    Assert-Cond (-not ($dotSourcedInstaller -match 'ccync-install:')) `
        'dot-sourcing the installer itself still skips the install (the self-test shape)'
    Assert-Cond ($plainIex -match 'ccync-install:') `
        'the shipped irm | iex path still runs the install'

    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue

    Complete-Case 'DotSourceGuard'
}


function Test-DownloadBase {
    $script:CaseFailed = $false
    $oldVersion = $env:CCYNC_VERSION
    $oldBase = $env:CCYNC_DOWNLOAD_BASE
    $oldArch = $env:PROCESSOR_ARCHITECTURE
    $script:requests = @()
    function Invoke-WebRequest {
        param($Uri, $OutFile, [switch]$UseBasicParsing)
        $script:requests += $Uri
        throw 'simulated download failure'
    }
    try {
        $env:PROCESSOR_ARCHITECTURE = 'AMD64'
        foreach ($base in @('https://example.com/assets', 'http://127.0.0.1:8765', 'http://localhost:8765')) {
            $env:CCYNC_VERSION = 'v1.2.3'
            $env:CCYNC_DOWNLOAD_BASE = $base
            $script:requests = @()
            try { Invoke-CcyncInstall } catch {}
            Assert-Cond ($script:requests.Count -eq 1 -and $script:requests[0] -eq "$base/ccync-v1.2.3-windows-x64.zip") "override directs archive request to $base"
        }
        foreach ($base in @('http://example.com', 'http://localhost.example.com', 'ftp://localhost')) {
            $env:CCYNC_DOWNLOAD_BASE = $base
            $script:requests = @()
            try { Invoke-CcyncInstall } catch {}
            Assert-Cond ($script:requests.Count -eq 0) "invalid base rejected: $base"
        }
        $env:CCYNC_VERSION = ''
        $env:CCYNC_DOWNLOAD_BASE = 'https://example.com'
        $script:requests = @()
        try { Invoke-CcyncInstall } catch {}
        Assert-Cond ($script:requests.Count -eq 0) 'base requires an explicit version'
    } finally {
        $env:CCYNC_VERSION = $oldVersion
        $env:CCYNC_DOWNLOAD_BASE = $oldBase
        $env:PROCESSOR_ARCHITECTURE = $oldArch
    }
    Complete-Case 'DownloadBase'
}

function Test-IsolatedDownload {
    $script:CaseFailed = $false
    $tmp = Join-Path $env:TEMP "ccync-downloadtest-$(New-Guid)"
    $oldHome, $oldProfile, $oldVersion, $oldBase, $oldArch, $oldInstall = $env:HOME, $env:USERPROFILE, $env:CCYNC_VERSION, $env:CCYNC_DOWNLOAD_BASE, $env:PROCESSOR_ARCHITECTURE, $InstallDir
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    try {
        $env:HOME = $tmp
        $env:USERPROFILE = $tmp
        $env:PROCESSOR_ARCHITECTURE = 'AMD64'
        $script:InstallDir = Join-Path $tmp '.local/bin'
        $script:requests = @()
        $script:badChecksum = $false
        $script:archive = Join-Path $tmp 'fixture.zip'
        $fixture = Join-Path $tmp 'fixture'
        New-Item -ItemType Directory -Path $fixture | Out-Null
        Copy-Item (Join-Path $env:SystemRoot 'System32/curl.exe') (Join-Path $fixture 'ccync.exe')
        Compress-Archive -Path (Join-Path $fixture 'ccync.exe') -DestinationPath $script:archive
        $script:hash = Get-FileSha256 $script:archive
        function Get-LatestVersion { return 'v1.2.3' }
        function Invoke-WebRequest {
            param($Uri, $OutFile, [switch]$UseBasicParsing)
            $script:requests += $Uri
            if ($Uri -notlike 'http://127.0.0.1:*') { throw 'network blocked' }
            if ($Uri -like '*.zip') { Copy-Item $script:archive $OutFile }
            elseif ($Uri -like '*/checksums.txt') {
                $hash = if ($script:badChecksum) { '0' * 64 } else { $script:hash }
                Set-Content -Path $OutFile -Value "$hash  ccync-v1.2.3-windows-x64.zip"
            } else { throw 'signature unavailable' }
        }
        $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
        $listener.Start()
        $port = $listener.LocalEndpoint.Port
        $listener.Stop()
        $env:CCYNC_VERSION = 'v1.2.3'
        $env:CCYNC_DOWNLOAD_BASE = "http://127.0.0.1:$port"
        $success = $true
        try { Invoke-CcyncInstall } catch { $success = $false; Write-Host $_ }
        Assert-Cond ($success -and (Test-Path (Join-Path $tmp '.local/bin/ccync.exe')) -and (Test-Path (Join-Path $tmp '.ccync/install-receipts/ccync.json'))) 'loopback install succeeds in isolated home'
        Remove-Item (Join-Path $tmp '.local'), (Join-Path $tmp '.ccync') -Recurse -Force -ErrorAction SilentlyContinue
        $script:badChecksum = $true
        $failed = $false
        try { Invoke-CcyncInstall } catch { $failed = $_.Exception.Message -match 'SHA-256 mismatch' }
        Assert-Cond ($failed -and -not (Test-Path (Join-Path $tmp '.local')) -and -not (Test-Path (Join-Path $tmp '.ccync'))) 'checksum mismatch prevents installation'
        $env:CCYNC_VERSION = ''
        $script:requests = @()
        try { Invoke-CcyncInstall } catch {}
        Assert-Cond ($script:requests.Count -eq 0) 'override alone makes no request'
        $env:CCYNC_VERSION = 'v1.2.3'
        $env:CCYNC_DOWNLOAD_BASE = 'http://example.com'
        $script:requests = @()
        try { Invoke-CcyncInstall } catch {}
        Assert-Cond ($script:requests.Count -eq 0) 'non-loopback HTTP makes no request'
        $env:CCYNC_VERSION = ''
        $env:CCYNC_DOWNLOAD_BASE = ''
        $script:requests = @()
        try { Invoke-CcyncInstall } catch {}
        Assert-Cond ($script:requests.Count -eq 1 -and $script:requests[0].StartsWith('https://github.com/monkey1wizard/ccync/releases/download/v1.2.3/')) 'default request uses public release URL'
    } finally {
        $env:HOME, $env:USERPROFILE, $env:CCYNC_VERSION, $env:CCYNC_DOWNLOAD_BASE, $env:PROCESSOR_ARCHITECTURE, $script:InstallDir = $oldHome, $oldProfile, $oldVersion, $oldBase, $oldArch, $oldInstall
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
    Complete-Case 'IsolatedDownload'
}

# --- dispatch ----------------------------------------------------------------
switch ($Case) {
    'PathHint'          { Test-PathHint }
    'PathHintQuoting'   { Test-PathHintQuoting }
    'CallerPreferences' { Test-CallerPreferences }
    'OutputSemantics'   { Test-OutputSemantics }
    'VersionGuard'      { Test-VersionGuard }
    'InstallReceipt'    { Test-InstallReceipt }
    'DotSourceGuard'    { Test-DotSourceGuard }
    'All'               { Test-PathHint; Test-PathHintQuoting; Test-CallerPreferences; Test-OutputSemantics; Test-VersionGuard; Test-InstallReceipt; Test-DotSourceGuard; Test-DownloadBase; Test-IsolatedDownload }
    default             { Write-Host "Case '$Case' not yet implemented"; $script:AnyFailed = $true }
}

if ($script:AnyFailed) { exit 1 } else { exit 0 }
