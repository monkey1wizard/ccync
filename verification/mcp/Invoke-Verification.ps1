[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Case,
    [ValidateSet('Fixture', 'Native')] [string] $Mode = 'Fixture',
    [Parameter(Mandatory)] [string] $EvidenceRoot,
    [string] $CargoCommand,
    [string] $HostProfileRoot,
    [string] $ClassificationTablePath,
    [string] $FixtureCommand = '',
    [string] $CapabilityFixturePath,
    [string] $EventFixturePath,
    [string] $ObservationHost,
    [string] $ObservationSession,
    [double] $ObservationSeconds = 0,
    [string] $OmitFixtureVariable,
    [switch] $SelfTest,
    [int] $TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'Isolation.ps1')
$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
if (-not (Test-EvidenceRoot -Path $EvidenceRoot -RepositoryRoot $repositoryRoot)) {
    [Console]::Error.WriteLine('EvidenceRoot must be inside the named task directory without reparse points.')
    exit 2
}
$EvidenceRoot = [IO.Path]::GetFullPath($EvidenceRoot)
New-Item -ItemType Directory -Force -Path $EvidenceRoot | Out-Null
$reportPath = Join-Path $EvidenceRoot 'report.json'
if ((Test-Path -LiteralPath $reportPath) -and ((Get-Item -LiteralPath $reportPath -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
    [Console]::Error.WriteLine('The report path must not be a reparse point.')
    exit 2
}
$report = [ordered]@{
    version = 1; case = $Case; mode = $Mode; result = 'Blocked'; reason = ''; executionStarted = $false
    missingPrerequisites = @(); evidencePaths = @('report.json'); publication = 'NotRun'; checks = @(); gaps = @()
    allowedVolatile = @(); childEnvironment = @{}; processes = @(); cleanup = @()
}
$exitCode = 2; $childRoot = $null; $temporaryParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/')
try {
    if ($Case -notin @('isolation', 'cargo', 'capabilities', 'events', 'passive', 'attribution', 'stop', 'windows', 'qualification', 'publication-status', 'replay')) { throw 'Unsupported case.' }
    if ($Case -eq 'replay') {
        . (Join-Path $PSScriptRoot 'Replay-Recovery.ps1')
        $capture = Invoke-RecoveryReplay -Mode $Mode -EvidenceRoot $EvidenceRoot -RepositoryRoot $repositoryRoot -TimeoutSeconds $TimeoutSeconds
        $report.result = $capture.result; $report.reason = $capture.reason
        $report.executionStarted = $capture.executionStarted; $report.checks = $capture.checks
        $report.gaps = $capture.gaps; $report.missingPrerequisites = $capture.missingPrerequisites
        $report.evidencePaths += $capture.evidencePaths
        $exitCode = if ($capture.result -eq 'Passed') { 0 } elseif ($capture.result -eq 'Failed') { 1 } else { 2 }
        exit $exitCode
    }
    if ($Case -eq 'publication-status') {
        $tablePath = Join-Path $repositoryRoot 'crates/mcp/src/windows_support.json'
        $tableText = (Get-Content -LiteralPath $tablePath -Raw).Trim()
        if ($tableText -cne '[]') { throw 'Publication status requires review: the support table is not empty.' }
        $reason = 'Native loaded-entry digest, exact identity, stop and window evidence, and production resolver facts are unavailable.'
        $report.executionStarted = $true
        $report.result = 'Blocked'; $report.publication = 'Blocked'
        $report.reason = 'No Native qualification passed. Windows stdio publication remains blocked.'
        $report.checks = @('claude', 'codex', 'copilot', 'opencode', 'agy' | ForEach-Object {
            @{ id = $_; result = 'Blocked'; reason = $reason; evidencePaths = @() }
        })
        $report.gaps = @($report.checks | ForEach-Object { "$($_.id): $($_.reason)" })
        $report.missingPrerequisites = @($report.gaps)
        $exitCode = 2
        exit $exitCode
    }
    if ($Case -eq 'qualification') {
        . (Join-Path $PSScriptRoot 'Qualification.ps1')
        $capture = Invoke-QualificationVerification -Mode $Mode -EvidenceRoot $EvidenceRoot -FixturePath $EventFixturePath
        $report.result = $capture.result; $report.reason = $capture.reason
        $report.executionStarted = $capture.executionStarted; $report.checks = $capture.checks
        $report.gaps = $capture.gaps; $report.missingPrerequisites = $capture.missingPrerequisites
        $report.evidencePaths += $capture.evidencePaths
        $exitCode = if ($capture.result -eq 'Passed') { 0 } elseif ($capture.result -eq 'Failed') { 1 } else { 2 }
        exit $exitCode
    }
    if ($Case -eq 'windows') {
        . (Join-Path $PSScriptRoot 'Windows-Calibration.ps1')
        $capture = Invoke-WindowsCalibration -Mode $Mode -EvidenceRoot $EvidenceRoot -FixturePath $EventFixturePath
        $report.result = $capture.result; $report.reason = $capture.reason
        $report.executionStarted = $capture.executionStarted; $report.checks = $capture.checks
        $report.gaps = $capture.gaps; $report.missingPrerequisites = $capture.missingPrerequisites
        $report.evidencePaths += $capture.evidencePaths
        if ($capture.ContainsKey('processes')) { $report.processes = @($capture.processes) }
        if ($capture.ContainsKey('cleanup')) { $report.cleanup = @($capture.cleanup) }
        $exitCode = if ($capture.result -eq 'Passed') { 0 } elseif ($capture.result -eq 'Failed') { 1 } else { 2 }
        exit $exitCode
    }
    if ($Case -eq 'stop') {
        . (Join-Path $PSScriptRoot 'Stop-Bounds.ps1')
        $capture = Invoke-StopVerification -Mode $Mode -EvidenceRoot $EvidenceRoot -FixturePath $EventFixturePath
        $report.result = $capture.result; $report.reason = $capture.reason
        $report.executionStarted = $capture.executionStarted; $report.checks = $capture.checks
        $report.gaps = $capture.gaps; $report.missingPrerequisites = $capture.missingPrerequisites
        $report.evidencePaths += $capture.evidencePaths
        $exitCode = if ($capture.result -eq 'Passed') { 0 } elseif ($capture.result -eq 'Failed') { 1 } else { 2 }
        exit $exitCode
    }
    if ($Case -eq 'attribution') {
        . (Join-Path $PSScriptRoot 'Capture-Events.ps1')
        . (Join-Path $PSScriptRoot 'Attribution.ps1')
        $capture = Invoke-AttributionVerification -Mode $Mode -EvidenceRoot $EvidenceRoot -FixturePath $EventFixturePath
        $report.result = $capture.result; $report.reason = $capture.reason
        $report.executionStarted = $capture.executionStarted; $report.checks = $capture.checks
        $report.gaps = $capture.gaps; $report.missingPrerequisites = $capture.missingPrerequisites
        $report.evidencePaths += $capture.evidencePaths
        $exitCode = if ($capture.result -eq 'Passed') { 0 } elseif ($capture.result -eq 'Failed') { 1 } else { 2 }
        exit $exitCode
    }
    if ($Case -eq 'passive') {
        if ($Mode -eq 'Fixture') {
            if (-not $PSBoundParameters.ContainsKey('ObservationHost')) { $ObservationHost = 'codex' }
            if (-not $PSBoundParameters.ContainsKey('ObservationSession')) { $ObservationSession = 'fixture-session' }
            if (-not $PSBoundParameters.ContainsKey('ObservationSeconds')) { $ObservationSeconds = 1 }
        }
        if ($ObservationHost -cnotin @('claude', 'codex', 'copilot', 'opencode', 'agy') -or
            [string]::IsNullOrWhiteSpace($ObservationSession) -or $ObservationSession.Length -gt 1024 -or
            [double]::IsNaN($ObservationSeconds) -or [double]::IsInfinity($ObservationSeconds) -or
            $ObservationSeconds -le 0 -or $ObservationSeconds -gt 3600) { throw 'Passive capture requires an exact host, session, and duration greater than zero and at most 3600 seconds.' }
        if ($Mode -eq 'Native') {
            $report.missingPrerequisites = @('A verified passive logical event source and separate exact P7 authorization')
            throw 'Native passive observation is unavailable.'
        }
        . (Join-Path $PSScriptRoot 'Capture-Events.ps1')
        $report.executionStarted = $true
        $capture = Invoke-PassiveFixtureCapture -EvidenceRoot $EvidenceRoot -FixturePath $EventFixturePath -ObservationHost $ObservationHost -ObservationSession $ObservationSession -ObservationSeconds $ObservationSeconds
        $failed = @($capture.checks | Where-Object { $_.result -ne 'Passed' })
        $report.result = if ($failed.Count) { 'Failed' } else { 'Passed' }; $report.reason = 'Bounded synthetic observation and counter checks completed.'
        $report.checks = $capture.checks; $report.gaps = $capture.gaps
        $report.evidencePaths += @('events.json', 'event-provenance.json', 'passive-report.json')
        $exitCode = if ($failed.Count) { 1 } else { 0 }
        exit $exitCode
    }
    if ($Case -eq 'events') {
        . (Join-Path $PSScriptRoot 'Capture-Events.ps1')
        $tablePath = if ($ClassificationTablePath) { $ClassificationTablePath } else { Join-Path $PSScriptRoot 'host-state-classification.json' }
        $table = Get-Content -LiteralPath $tablePath -Raw | ConvertFrom-Json
        if ($Mode -eq 'Native' -and $EventFixturePath) { throw 'Event fixtures are accepted only in Fixture mode.' }
        $capture = Invoke-EventCapture -Mode $Mode -EvidenceRoot $EvidenceRoot -FixturePath $EventFixturePath -Table $table
        $report.result = $capture.result; $report.checks = $capture.checks; $report.gaps = $capture.gaps
        $report.evidencePaths += 'event-provenance.json'
        if ($Mode -eq 'Fixture') {
            $report.executionStarted = $true
            $report.evidencePaths += @('events.json', 'control-native-source.json', 'control-http-no-local-pid.json', 'control-pre-handshake-failure.json', 'control-dropped-connection.json')
        }
        $report.reason = if ($Mode -eq 'Fixture') { 'Synthetic event capture completed. Native capture remains unproven.' } else { 'Native logical event feeds are unverified.' }
        $exitCode = if ($report.result -eq 'Passed') { 0 } else { 2 }
        exit $exitCode
    }
    if ($Case -eq 'capabilities') {
        . (Join-Path $PSScriptRoot 'Host-Probes.ps1')
        $tablePath = if ($ClassificationTablePath) { $ClassificationTablePath } else { Join-Path $PSScriptRoot 'host-state-classification.json' }
        $table = Get-Content -LiteralPath $tablePath -Raw | ConvertFrom-Json
        foreach ($name in @('claude', 'codex', 'copilot', 'opencode', 'agy')) {
            $hostTable = $table.hosts.PSObject.Properties[$name]
            if (-not $hostTable) { throw "Missing classification for $name." }
            $derivedPath = Join-Path $EvidenceRoot "classification-$name.json"
            Write-EvidenceJson $derivedPath @{ allowedVolatile = @($hostTable.Value.allowedVolatile); excludedEphemeral = @($hostTable.Value.excludedEphemeral) }
            Get-ClassificationTable $derivedPath | Out-Null
            $report.evidencePaths += "classification-$name.json"
        }
        $bootstrap = $null
        if ($Mode -eq 'Native') {
            if ($HostProfileRoot) { throw 'Capability bootstrap requires an empty synthetic profile; omit HostProfileRoot.' }
            $childRoot = Join-Path $temporaryParent ('ccync-verification-' + [guid]::NewGuid().ToString('N'))
            $report.executionStarted = $true
            $bootstrap = Invoke-HostInventoryBootstrap -Root $childRoot -EvidenceRoot $EvidenceRoot -TimeoutSeconds ([Math]::Min($TimeoutSeconds, 30))
            $report.evidencePaths += 'bootstrap.json'
        }
        $capabilities = Get-HostCapabilities -Mode $Mode -FixturePath $CapabilityFixturePath -Table $table -Bootstrap $bootstrap
        Write-EvidenceJson (Join-Path $EvidenceRoot 'capabilities.json') $capabilities
        $report.evidencePaths += 'capabilities.json'
        if ($Mode -eq 'Fixture') { $report.executionStarted = $true }
        $report.checks = @($capabilities.hosts | ForEach-Object {
            @{ id = $_.host; result = $_.result; reason = $_.reason; evidencePaths = @('capabilities.json') }
        })
        $report.gaps = @($capabilities.hosts | ForEach-Object { $hostName = $_.host; $_.gaps | ForEach-Object { "$hostName`: $_" } })
        $report.missingPrerequisites = @($report.gaps)
        if (@($capabilities.hosts | Where-Object { $_.result -ne 'Passed' }).Count) {
            $report.result = 'Blocked'; $report.reason = 'One or more host prerequisites are missing.'; $exitCode = 2
        } else {
            $report.result = 'Passed'; $report.reason = 'Synthetic capability checks passed; Native capability remains unproven.'; $exitCode = 0
        }
        exit $exitCode
    }
    if ($Case -eq 'cargo' -and $CargoCommand -cnotmatch '^cargo +[A-Za-z0-9_./\\:=+@,%-]+(?:[ \t]+[A-Za-z0-9_./\\:=+@,%-]+)*[ \t]*$') {
        throw 'CargoCommand must start with cargo and contain only plain arguments.'
    }
    if ($Mode -eq 'Native') {
        $report.missingPrerequisites = @('Native host capability, budget and approved exact classification table integration')
        throw 'Native execution is outside this fixture implementation boundary.'
    }
    if (-not (Get-Command Get-CimInstance -ErrorAction SilentlyContinue)) {
        $report.missingPrerequisites = @('Windows native process identity through CIM'); throw 'Native process identity is unavailable.'
    }
    if ($SelfTest) {
        if ($Case -ne 'isolation') { throw 'SelfTest requires the isolation case.' }
        $report.executionStarted = $true
        $report.checks = @(Invoke-IsolationSelfTest $EvidenceRoot)
        $failed = @($report.checks | Where-Object { $_.result -ne 'Passed' })
        $report.result = if ($failed.Count) { 'Failed' } else { 'Passed' }
        $report.reason = "Script self-tests completed with $($failed.Count) failures."
        $exitCode = if ($failed.Count) { 1 } else { 0 }
        exit $exitCode
    }
    $classification = Get-ClassificationTable $ClassificationTablePath
    $childRoot = Join-Path $temporaryParent ('ccync-verification-' + [guid]::NewGuid().ToString('N'))
    $environment = Get-IsolatedEnvironment -Root $childRoot -Cargo ($Case -eq 'cargo')
    foreach ($name in @('USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'CCYNC_HOME')) {
        New-Item -ItemType Directory -Force -Path $environment[$name] | Out-Null
    }
    if ($HostProfileRoot) {
        $profile = [IO.Path]::GetFullPath($HostProfileRoot)
        $realHome = [Environment]::GetFolderPath('UserProfile')
        if ($profile.TrimEnd('\', '/') -ieq $realHome.TrimEnd('\', '/')) { throw 'HostProfileRoot must be a dedicated profile, not the real home.' }
        Copy-HostProfile $profile $environment
    }
    $shell = (Get-Command powershell -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $prefix = '$ErrorActionPreference = ''Stop''; Start-Sleep -Milliseconds 400; '
    $environmentCode = '$observed = [ordered]@{}; foreach ($name in ' +
        "@('" + (($environment.Keys | Sort-Object) -join "','") + "')) { " +
        '$observed[$name] = [Environment]::GetEnvironmentVariable($name, ''Process'') }; ' +
        'Write-Output (''CCYNC_ENVIRONMENT:'' + (ConvertTo-Json -InputObject $observed -Compress)); '
    if ($Case -eq 'isolation') {
        $fixture = Join-Path $EvidenceRoot 'fixture'
        if (-not (Test-EvidenceRoot (Join-Path $fixture 'probe') $repositoryRoot)) { throw 'Fixture root must not contain reparse points.' }
        New-Item -ItemType Directory -Force -Path $fixture | Out-Null
        $before = Get-FixtureInventory $fixture $classification
        Write-EvidenceJson (Join-Path $EvidenceRoot 'before.json') $before
        $report.evidencePaths += 'before.json'
        $report.gaps = @($before.gaps)
        if ($before.gaps.Count) { throw 'Initial inventory has uncovered read gaps.' }
        if ($OmitFixtureVariable) {
            if (-not $environment.ContainsKey($OmitFixtureVariable)) { throw 'OmitFixtureVariable must name an isolated environment field.' }
            $environment[$OmitFixtureVariable] = $null
        }
        $validation = '$required = @(''USERPROFILE'',''HOME'',''APPDATA'',''LOCALAPPDATA'',''XDG_CONFIG_HOME'',''XDG_STATE_HOME'',''XDG_CACHE_HOME'',''CCYNC_HOME''); ' +
            'foreach ($name in $required) { if (-not [Environment]::GetEnvironmentVariable($name)) { throw (''Missing isolated variable: '' + $name) } }; ' +
            'if ($env:HOME -ne $env:USERPROFILE) { throw ''HOME does not match USERPROFILE'' }; ' +
            'foreach ($name in @(''OPENCODE_CONFIG_CONTENT'',''OPENCODE_CONFIG'',''CCYNC_SKILLS'',''CLAUDE_CONFIG_DIR'')) { if ([Environment]::GetEnvironmentVariable($name)) { throw (''Unexpected override: '' + $name) } }; ' +
            'if ($env:CODEX_HOME -ne (Join-Path $env:USERPROFILE ''.codex'') -or $env:COPILOT_HOME -ne (Join-Path $env:USERPROFILE ''.copilot'') -or $env:OPENCODE_CONFIG_DIR -ne (Join-Path $env:APPDATA ''opencode'')) { throw ''Host variables do not match isolated defaults'' }; '
        $script = $prefix + $environmentCode + $validation + $FixtureCommand
        $arguments = '-NoProfile -NonInteractive -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
        $report.executionStarted = $true
        try { $child = Invoke-IsolatedProcess $shell $arguments $environment $fixture $TimeoutSeconds }
        finally {
            $after = Get-FixtureInventory $fixture $classification
            $excludedPaths = @(@($before.excludedEphemeral) + @($after.excludedEphemeral) | ForEach-Object { $_.path })
            $changes = Compare-FixtureInventory $before.records $after.records $classification $excludedPaths
            $rawChanges = Compare-FixtureInventory $before.records $after.records
            $volatilePaths = @(@($before.allowedVolatile) + @($after.allowedVolatile) | Sort-Object -Unique)
            $report.allowedVolatile = $volatilePaths
            $unchanged = @($before.records.Keys | Where-Object { $after.records.ContainsKey($_) -and $_ -notin $rawChanges -and
                $_ -notin $volatilePaths -and $_ -notin $excludedPaths -and $before.records[$_].type -eq 'file' } | Sort-Object)
            $diff = @{ changes = @($changes); allowedVolatileChanges = @($rawChanges | Where-Object { $_ -in $volatilePaths });
                unchanged = $unchanged; excludedEphemeral = $excludedPaths; excludedClaudeFields = @($before.excludedClaudeFields + $after.excludedClaudeFields | Sort-Object -Unique) }
            Write-EvidenceJson (Join-Path $EvidenceRoot 'after.json') $after
            Write-EvidenceJson (Join-Path $EvidenceRoot 'diff.json') $diff
            $report.evidencePaths += @('after.json', 'diff.json')
            $report.gaps = @($before.gaps) + @($after.gaps)
        }
        $report.result = if ($child.exitCode -eq 0 -and -not $changes.Count -and -not $report.gaps.Count) { 'Passed' } else { 'Failed' }
        $report.reason = if ($report.result -eq 'Passed') { 'Fixture child and persistent-state comparison passed. Excluded paths are outside unchanged claims.' } else { 'Fixture child failed, persistent state changed, or inventory has uncovered gaps. No repair was attempted.' }
        $report.checks += @{ id = 'persistent-state'; result = $(if ($changes.Count -or $report.gaps.Count) { 'Failed' } else { 'Passed' });
            reason = 'Byte hash, size and mtime comparison with explicit exclusions and Claude MCP subtree comparison.'; evidencePaths = @('before.json', 'after.json', 'diff.json') }
    } else {
        $cargo = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if (-not $cargo) { $report.missingPrerequisites += 'cargo'; throw 'Cargo is unavailable.' }
        $environment.CARGO_TARGET_DIR = Join-Path $repositoryRoot 'target'
        foreach ($pair in @(@('CARGO_HOME', '.cargo'), @('RUSTUP_HOME', '.rustup'))) {
            $value = [Environment]::GetEnvironmentVariable($pair[0], 'Process')
            if (-not $value) { $value = Join-Path ([Environment]::GetFolderPath('UserProfile')) $pair[1] }
            if (-not (Test-Path -LiteralPath $value -PathType Container)) { $report.missingPrerequisites += $pair[0]; throw "$($pair[0]) is unavailable." }
            $environment[$pair[0]] = $value
        }
        $pythonPath = [Environment]::GetEnvironmentVariable('CCYNC_RECORDED_PYTHON', 'Process')
        if (-not $pythonPath) {
            $python = Get-Command python -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($python) { $pythonPath = $python.Source }
        }
        if (-not $pythonPath -or -not (Test-Path -LiteralPath $pythonPath -PathType Leaf)) { $report.missingPrerequisites += 'python'; throw 'An existing Python interpreter is required for cargo verification.' }
        $environment.CCYNC_RECORDED_PYTHON = $pythonPath
        $environmentCode = '$observed = [ordered]@{}; foreach ($name in @(''' + (($environment.Keys | Sort-Object) -join "','") +
            ''')) { $observed[$name] = [Environment]::GetEnvironmentVariable($name, ''Process'') }; Write-Output (''CCYNC_ENVIRONMENT:'' + (ConvertTo-Json -InputObject $observed -Compress)); '
        $cargoPath = $cargo.Source.Replace("'", "''")
        $cargoArguments = @($CargoCommand.Substring(6).Trim() -split '[ \t]+' | ForEach-Object { "'" + $_ + "'" }) -join ','
        $script = $prefix + $environmentCode + "`$ErrorActionPreference = 'Continue'; & '$cargoPath' @($cargoArguments) 2>&1 | ForEach-Object { `"`$_`" }; exit `$LASTEXITCODE"
        $arguments = '-NoProfile -NonInteractive -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
        $report.executionStarted = $true
        $child = Invoke-IsolatedProcess $shell $arguments $environment $repositoryRoot $TimeoutSeconds
        $report.result = if ($child.exitCode -eq 0) { 'Passed' } else { 'Failed' }
        $report.reason = "Cargo exited with code $($child.exitCode)."
        $target = $null
        foreach ($line in ($child.stdout -split '\r?\n')) {
            if ($line -cmatch '^\s*(?:Running|Doc-tests)\s+(.+)$') {
                $target = @{ id = $Matches[1]; result = 'NotRun'; reason = 'No target summary observed.'; evidencePaths = @('child.log') }
                $report.checks += $target
            } elseif ($line -match '^test result: (ok|FAILED)\..*') {
                if (-not $target) {
                    $target = @{ id = 'cargo-test-target'; result = 'NotRun'; reason = ''; evidencePaths = @('child.log') }
                    $report.checks += $target
                }
                $target.result = if ($Matches[1] -eq 'ok') { 'Passed' } else { 'Failed' }
                if ($line -match '\b0 passed; 0 failed;') { $target.result = 'NotRun' }
                $target.reason = $line; $target = $null
            }
        }
        if ($CargoCommand -match '^cargo +test\b' -and -not @($report.checks | Where-Object { $_.result -in @('Passed', 'Failed') }).Count) {
            $report.result = 'Failed'; $report.reason = 'Cargo executed no completed nonempty test target.'
        }
    }
    $report.processes = $child.processes; $report.cleanup = $child.cleanup
    if ($child.failure) { $report.gaps += $child.failure; $report.result = 'Failed'; $report.reason = $child.failure }
    $environmentLine = @($child.stdout -split '\r?\n' | Where-Object { $_.StartsWith('CCYNC_ENVIRONMENT:') })
    if ($environmentLine.Count -ne 1) { throw 'Exactly one child environment record is required.' }
    $report.childEnvironment = $environmentLine[0].Substring('CCYNC_ENVIRONMENT:'.Length) | ConvertFrom-Json
    $log = ($child.stdout -split '\r?\n' | Where-Object { -not $_.StartsWith('CCYNC_ENVIRONMENT:') }) -join "`n"
    Write-EvidenceJson (Join-Path $EvidenceRoot 'child.log') @{ stdout = $log; stderr = $child.stderr; exitCode = $child.exitCode }
    $report.evidencePaths += 'child.log'
    $report.checks += @{ id = $Case + '-child'; result = $(if ($child.exitCode -eq 0) { 'Passed' } else { 'Failed' });
        reason = "Isolated child exited with code $($child.exitCode)."; evidencePaths = @('child.log', 'report.json') }
    $exitCode = if ($report.result -eq 'Passed') { 0 } else { 1 }
} catch {
    $report.reason = $_.Exception.Message
    if ($report.executionStarted) { $report.result = 'Failed'; $exitCode = 1 }
} finally {
    if ($childRoot -and (Test-Path -LiteralPath $childRoot)) {
        try {
            $resolvedChild = [IO.Path]::GetFullPath($childRoot)
            if ([IO.Path]::GetDirectoryName($resolvedChild) -ine $temporaryParent -or
                [IO.Path]::GetFileName($resolvedChild) -notmatch '^ccync-verification-[0-9a-f]{32}$' -or
                ((Get-Item -LiteralPath $resolvedChild -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Temporary copy cleanup boundary failed.' }
            $pending = [Collections.Generic.Queue[string]]::new(); $pending.Enqueue($resolvedChild)
            while ($pending.Count) {
                $directory = $pending.Dequeue()
                foreach ($item in Get-ChildItem -LiteralPath $directory -Force -ErrorAction Stop) {
                    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Temporary copy contains a reparse point. Cleanup refused.' }
                    if ($item.PSIsContainer) { $pending.Enqueue($item.FullName) }
                }
            }
            Remove-Item -LiteralPath $childRoot -Recurse -Force -ErrorAction Stop
        } catch {
            $report.gaps += "Temporary profile cleanup failed: $childRoot"
            $report.result = 'Failed'; $report.reason = 'Temporary profile cleanup failed.'; $exitCode = 1
        }
    }
    Write-EvidenceJson $reportPath $report
}
exit $exitCode
