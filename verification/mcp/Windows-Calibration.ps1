function Get-WindowFixtureAssessment {
    param($Observation)
    if (-not $Observation.observationComplete -or -not $Observation.parentChainComplete) { return 'Blocked' }
    foreach ($event in $Observation.events) {
        if ($event.pid -notin $Observation.chainPids) { return 'Blocked' }
        if ($event.visible -and $event.origin -ne 'application') { return 'Failed' }
    }
    if (-not $Observation.normalControlObserved -or -not $Observation.allocConsoleObserved) { return 'Blocked' }
    return 'Passed'
}

function Invoke-WindowsCalibration {
    param([string] $Mode, [string] $EvidenceRoot, [string] $FixturePath)
    $path = 'windows-calibration.json'
    if ($Mode -eq 'Native') {
        if ($FixturePath) { throw 'Event fixtures are accepted only in Fixture mode.' }
        return Invoke-NativeWindowCalibration -EvidenceRoot $EvidenceRoot
    }
    if ($FixturePath) { throw 'Custom window fixtures are not supported.' }
    $scenarios = @(
        @{ name = 'invisible'; events = @(@{ pid = 20; visible = $false; origin = 'launch' }); expected = 'Passed' },
        @{ name = 'visible-launch'; events = @(@{ pid = 20; visible = $true; origin = 'launch' }); expected = 'Failed' },
        @{ name = 'alloc-console'; events = @(@{ pid = 20; visible = $true; origin = 'application' }); expected = 'Passed' },
        @{ name = 'wrong-pid'; events = @(@{ pid = 99; visible = $true; origin = 'launch' }); expected = 'Blocked' },
        @{ name = 'missing-observation'; events = @(); expected = 'Blocked' },
        @{ name = 'missing-parent'; events = @(); expected = 'Blocked' },
        @{ name = 'missing-alloc-control'; events = @(); expected = 'Blocked' }
    )
    $controls = @(); $checks = @()
    foreach ($scenario in $scenarios) {
        $observation = @{ observationComplete = ($scenario.name -ne 'missing-observation'); parentChainComplete = ($scenario.name -ne 'missing-parent')
            chainPids = @(10, 20); parentChain = @(@{ pid = 20; parentPid = 10 }); normalControlObserved = $true
            allocConsoleObserved = ($scenario.name -ne 'missing-alloc-control'); events = $scenario.events }
        $actual = Get-WindowFixtureAssessment $observation
        $controls += @{ name = $scenario.name; synthetic = $true; observation = $observation; assessment = $actual; expected = $scenario.expected }
        $checks += @{ id = $scenario.name; result = $(if ($actual -eq $scenario.expected) { 'Passed' } else { 'Failed' }); reason = 'Synthetic window attribution control.'; evidencePaths = @($path) }
    }
    Write-EvidenceJson (Join-Path $EvidenceRoot $path) @{ mode = 'Fixture'; synthetic = $true; controls = $controls; nativeCapabilityEstablished = $false }
    $failed = @($checks | Where-Object { $_.result -ne 'Passed' })
    return @{ result = $(if ($failed.Count) { 'Failed' } else { 'Passed' }); reason = 'Synthetic controls completed; Native capability is not established by fixtures.'
        executionStarted = $true; checks = $checks; gaps = @('Fixture data does not establish desktop visibility or complete event capture.')
        missingPrerequisites = @(); evidencePaths = @($path) }
}

function Invoke-NativeWindowCalibration {
    param([string] $EvidenceRoot)
    $path = 'windows-calibration.json'
    $calibration = [ordered]@{
        result = 'Blocked'; failedStep = $null; reason = ''; executionStarted = $false
        desktop = $null; windowStation = $null; desktopClosed = $false
        controls = @(); gaps = @('Polling can miss windows and processes that exist between samples. Zero samples do not prove no flash.')
    }
    $observer = $null; $step = 'CompileObserver'; $runRelative = $null
    try {
        $runRelative = 'windows-run-' + [guid]::NewGuid().ToString('N')
        $runRoot = Join-Path $EvidenceRoot $runRelative
        [IO.Directory]::CreateDirectory($runRoot) | Out-Null
        $compilerRoot = Join-Path $runRoot 'temp'
        [IO.Directory]::CreateDirectory($compilerRoot) | Out-Null
        $compilerSettings = [CodeDom.Compiler.CompilerParameters]::new()
        $compilerSettings.TempFiles = [CodeDom.Compiler.TempFileCollection]::new($compilerRoot, $false)
        $launcher = Join-Path $runRoot 'calibration-control.exe'
        $compilerSettings.OutputAssembly = $launcher
        $compilerSettings.GenerateExecutable = $true; $compilerSettings.GenerateInMemory = $false
        $compilerSettings.CompilerOptions = '/target:winexe'
        $compilerSettings.MainClass = 'Ccync.Verification.CalibrationControlRunner'
        $compilerSettings.ReferencedAssemblies.Add('System.dll') | Out-Null
        Add-Type -Path (Join-Path $PSScriptRoot 'WindowsObserver.cs') -CompilerParameters $compilerSettings -ErrorAction Stop
        if (-not ('Ccync.Verification.WindowsObserver' -as [type])) { [Reflection.Assembly]::LoadFrom($launcher) | Out-Null }
        $step = 'CreateDesktop'
        $observer = [Ccync.Verification.WindowsObserver]::new()
        $calibration.desktop = $observer.DesktopName
        $calibration.windowStation = $observer.WindowStation
        $step = 'ControlEnvironment'
        $isolated = Get-IsolatedEnvironment -Root $runRoot -Cargo $false
        $environment = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($name in $isolated.Keys) {
            if ($null -ne $isolated[$name]) {
                $environment[$name] = $isolated[$name]
                [IO.Directory]::CreateDirectory($isolated[$name]) | Out-Null
            }
        }
        $systemRoot = [Environment]::GetEnvironmentVariable('SystemRoot', 'Process')
        if ([string]::IsNullOrWhiteSpace($systemRoot)) { throw 'SystemRoot is unavailable.' }
        $automationPath = [Management.Automation.PowerShell].Assembly.Location
        if (-not [IO.File]::Exists($automationPath)) { throw 'The installed Windows PowerShell assembly is unavailable.' }
        $environment['SystemRoot'] = $systemRoot; $environment['WINDIR'] = $systemRoot
        $environment['PATH'] = (Join-Path $systemRoot 'System32') + ';' + $PSHOME
        $environment['TEMP'] = Join-Path $runRoot 'temp'; $environment['TMP'] = $environment['TEMP']
        [IO.Directory]::CreateDirectory($environment['TEMP']) | Out-Null
        $environment['CCYNC_CALIBRATION_VALUE'] = 'isolated-calibration-value'
        $unicode = [string][char]0x6E2C + [char]0x8A66 + '-' + [char]0x03BB
        $argument = 'argument with spaces'
        $line = 'ccync-calibration-stdin'
        foreach ($control in @('normal', 'allocconsole')) {
            $step = "$control`:Launch"
            $resultPath = Join-Path $runRoot "$control-result.json"
            $inputPath = Join-Path $runRoot "$control-stdin.txt"
            $outputPath = Join-Path $runRoot "$control-stdout.txt"
            $errorPath = Join-Path $runRoot "$control-stderr.txt"
            [IO.File]::WriteAllText($inputPath, $line + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
            $arguments = @($automationPath, (Join-Path $PSScriptRoot 'TestServer.ps1'), $control, $resultPath, $argument, $unicode)
            $observation = $observer.RunControl($launcher, [string[]]$arguments, $runRoot, $environment,
                $inputPath, $outputPath, $errorPath, 15000)
            $calibration.executionStarted = $calibration.executionStarted -or $observation.executionStarted
            $entry = [ordered]@{ name = $control; synthetic = $false; result = 'Blocked'; observation = $observation
                control = $null; checks = @(); evidencePaths = @("$runRelative/$control-stdin.txt", "$runRelative/$control-stdout.txt", "$runRelative/$control-stderr.txt") }
            $calibration.controls += $entry
            if ($observation.failedStep) {
                $step = "$control`:$($observation.failedStep)"
                throw $observation.error
            }
            if (-not $observation.observationComplete -or -not $observation.parentChainComplete -or -not $observation.jobClosed -or
                $observation.cleanupErrors.Count -or @($observation.cleanup | Where-Object { -not $_.stopped }).Count) {
                $step = "$control`:ObservationAndCleanup"
                throw 'Window observation, parent identity, or owned job cleanup is incomplete.'
            }
            $step = "$control`:ControlResult"
            if (-not [IO.File]::Exists($resultPath)) { throw 'The control did not write its result.' }
            $record = [IO.File]::ReadAllText($resultPath, [Text.Encoding]::UTF8) | ConvertFrom-Json
            $entry.control = $record
            $entry.evidencePaths += "$runRelative/$control-result.json"
            if ($record.failedStep) {
                $step = "$control`:$($record.failedStep)"
                throw $record.error
            }
            $entry.checks = @(
                @{ id = 'args'; passed = ($record.arguments.Count -eq 2 -and $record.arguments[0] -ceq $argument) },
                @{ id = 'unicode'; passed = ($record.unicodeArgument -ceq $unicode -and $record.arguments[1] -ceq $unicode) },
                @{ id = 'cwd'; passed = ($record.cwd -ieq $runRoot) },
                @{ id = 'environment'; passed = ($record.environment -ceq $environment['CCYNC_CALIBRATION_VALUE']) },
                @{ id = 'identity'; passed = ($record.pid -eq $observation.parentChain[0].pid -and $record.control -ceq $control) },
                @{ id = 'exit'; passed = ($observation.rootExited -and $observation.exitCode -eq 0) }
            )
            if ($control -eq 'normal') {
                $entry.checks += @(
                    @{ id = 'stdin'; passed = ($record.stdin -ceq $line) },
                    @{ id = 'stdout'; passed = ([IO.File]::ReadAllText($outputPath, [Text.Encoding]::UTF8) -ceq ($line + [Environment]::NewLine)) },
                    @{ id = 'stderr'; passed = ([IO.File]::ReadAllText($errorPath, [Text.Encoding]::UTF8) -ceq ('ccync-calibration-stderr' + [Environment]::NewLine)) },
                    @{ id = 'close'; passed = ($record.stdinClosed -eq $true) }
                )
            }
            if (@($entry.checks | Where-Object { -not $_.passed }).Count) {
                $step = "$control`:ControlChecks"
                $entry.result = 'Failed'; $calibration.result = 'Failed'
                throw ('Control checks failed: ' + ((@($entry.checks | Where-Object { -not $_.passed }) | ForEach-Object { $_.id }) -join ', '))
            }
            $step = "$control`:WindowAttribution"
            if (@($observation.events | Where-Object { -not $_.attributed }).Count) {
                throw 'A window PID lacks a matching native creation identity in the owned parent chain.'
            }
            $visible = @($observation.events | Where-Object { $_.visible -and $_.attributed })
            if ($control -eq 'normal') {
                if ($visible.Count) {
                    $step = 'normal:UnexpectedStartupWindow'; $entry.result = 'Failed'; $calibration.result = 'Failed'
                    throw 'The normal control exposed a visible attributed startup window.'
                }
            } else {
                $console = @($visible | Where-Object {
                    $_.hwnd -eq $record.consoleWindow -and $_.pid -eq $record.consoleOwnerPid -and $_.windowClass -ceq 'ConsoleWindowClass'
                })
                if (-not $record.allocConsoleSucceeded -or $console.Count -eq 0) {
                    $step = 'allocconsole:VisibleAttributedConsole'
                    throw 'AllocConsole did not produce an observed visible attributed console window. No visibility capability is established.'
                }
            }
            $entry.result = 'Passed'
        }
        $calibration.result = 'Passed'
        $calibration.reason = 'Normal and deliberate AllocConsole controls passed on the owned desktop.'
    } catch {
        $calibration.failedStep = $step
        $calibration.reason = $_.Exception.Message
        $calibration.gaps += "$step`: $($calibration.reason)"
    } finally {
        if ($null -ne $observer) {
            $observer.Dispose()
            $calibration.desktopClosed = $observer.DesktopClosed
            if (-not $observer.DesktopClosed) {
                $calibration.result = 'Blocked'; $calibration.failedStep = 'CloseDesktop'
                $calibration.reason = $observer.CloseError
                $calibration.gaps += "CloseDesktop: $($observer.CloseError)"
            }
        }
    }
    $missing = @('host: entry loading was not confirmed', 'version: no host was launched',
        'entry_version: no loaded entry is available', 'command_class: no loaded entry is available',
        'cwd_class: no loaded entry is available', 'lookupKey.host: no loaded entry is available',
        'lookupKey.serversKey: no loaded entry is available', 'lookupKey.server: no loaded entry is available',
        'digest: no copy was loaded, so no digest can be recomputed',
        'Approved dedicated login is unavailable', 'Isolated host launch is outside this calibration task',
        'Rendered-entry merge and host load confirmation were not performed')
    $cells = @(foreach ($hostName in @('claude', 'codex', 'copilot', 'opencode', 'agy')) {
        @{ id = $hostName; result = 'Blocked'; executionStarted = $false; entryLoaded = $false
            ownership = 'An isolated render entry would not be ccync-managed.'; missingPrerequisites = $missing; gaps = @() }
    })
    $reason = if ($calibration.result -eq 'Passed') {
        'Desktop calibration passed. Host cells are Blocked because approved login and isolated host launch are unavailable.'
    } else { "Desktop calibration $($calibration.result) at $($calibration.failedStep): $($calibration.reason)" }
    foreach ($entry in $calibration.controls) {
        $entry.evidencePaths = @($entry.evidencePaths | Where-Object { [IO.File]::Exists((Join-Path $EvidenceRoot $_)) })
        $calibration.gaps += @($entry.observation.cleanupErrors | ForEach-Object { "$($entry.name): $_" })
        $calibration.gaps += @($entry.observation.cleanup | Where-Object { -not $_.stopped } | ForEach-Object { "$($entry.name): $($_.error)" })
    }
    $evidencePaths = @($path) + @($calibration.controls | ForEach-Object { $_.evidencePaths })
    if ($runRelative -and [IO.File]::Exists((Join-Path $EvidenceRoot "$runRelative/calibration-control.exe"))) {
        $evidencePaths += "$runRelative/calibration-control.exe"
    }
    Write-EvidenceJson (Join-Path $EvidenceRoot $path) @{ mode = 'Native'; synthetic = $false; calibration = $calibration
        cells = $cells; completed = $false; nativeCapabilityEstablished = $false
        desktopCalibrationEstablished = ($calibration.result -eq 'Passed') }
    return @{ result = 'Blocked'; reason = $reason; executionStarted = $false
        processes = @($calibration.controls | ForEach-Object { $_.observation.parentChain } | ForEach-Object {
            @{ pid = $_.pid; parentPid = $_.parentPid; created = $_.creationFileTime }
        })
        cleanup = @($calibration.controls | ForEach-Object { $_.observation.cleanup } | ForEach-Object {
            @{ identity = @{ pid = $_.identity.pid; parentPid = $_.identity.parentPid; created = $_.identity.creationFileTime }
                stopped = $_.stopped; error = $_.error }
        })
        checks = @(@{ id = 'desktop-calibration'; result = $calibration.result; reason = $calibration.reason; evidencePaths = @($path) }) +
            @($cells | ForEach-Object { @{ id = $_.id; result = 'Blocked'; reason = 'Approved login and isolated host launch are unavailable.'; evidencePaths = @($path) } })
        gaps = @($calibration.gaps); missingPrerequisites = $missing; evidencePaths = $evidencePaths }
}
