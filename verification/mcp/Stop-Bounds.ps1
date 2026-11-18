function Get-StopAssessment {
    param($Observation)
    $failures = [Collections.Generic.List[string]]::new()
    if ($Observation.scope -cne 'per-server' -or -not $Observation.versionBound -or
        $Observation.maxAttempts -le 0 -or $Observation.deadlineSeconds -le 0 -or $Observation.graceSeconds -lt 0 -or
        -not $Observation.settingsLoaded -or -not $Observation.entryLoaded) { $failures.Add('Version-bound per-server limits and loaded settings are required before the fault.') }
    if (-not $Observation.faultAfterLoad) { $failures.Add('Fault preceded load confirmation.') }
    if ($Observation.stopKind -cnotin @('native-termination', 'native-queue-cleared')) { $failures.Add('No native termination or queue clearing was observed.') }
    if ($Observation.attempts -gt $Observation.maxAttempts -or $Observation.attemptsAfterDeadline -ne 0 -or -not $Observation.observedThroughGrace) { $failures.Add('Attempt cap or deadline plus grace was not established.') }
    if (-not $Observation.healthyCallSucceeded -or -not $Observation.sameSession) { $failures.Add('Healthy MCP call in the same session was not established.') }
    if ($Observation.effectivePaths -ne 1) { $failures.Add('The physical configuration does not have exactly one effective path.') }
    if ($Observation.maxTargetPids -gt 1 -or $Observation.maxTargetPids -lt 0) { $failures.Add('A launch has an invalid or multiple target process count.') }
    if ($Observation.duplicateUnresolved -and (-not $Observation.foreignPreserved -or -not $Observation.additionBlocked)) { $failures.Add('The unresolved foreign registration was not preserved and the addition blocked.') }
    foreach ($operation in @('preview', 'doctor', 'sync1', 'sync2')) {
        foreach ($counter in @('use', 'launch', 'reload', 'notify')) {
            if ($Observation.increments[$operation][$counter] -ne 0) { $failures.Add("$operation increased $counter.") }
        }
    }
    $message = if ($Observation.stillRetrying) { 'still retrying, ccync cannot stop it; consult the exact host version documentation for manual remediation.' } else { '' }
    if ($Observation.stillRetrying) { $failures.Add('The loaded entry is still retrying.') }
    return @{ result = $(if ($failures.Count) { 'Failed' } else { 'Passed' }); stopState = $Observation.stopKind; failures = @($failures.ToArray()); remediation = $message }
}

function New-StopFixture {
    $increments = @{}
    foreach ($operation in @('preview', 'doctor', 'sync1', 'sync2')) { $increments[$operation] = @{ use = 0; launch = 0; reload = 0; notify = 0 } }
    return @{ scope = 'per-server'; versionBound = $true; maxAttempts = 3; deadlineSeconds = 10; graceSeconds = 2
        settingsLoaded = $true; entryLoaded = $true; faultAfterLoad = $true; stopKind = 'native-termination'
        attempts = 3; attemptsAfterDeadline = 0; observedThroughGrace = $true; healthyCallSucceeded = $true; sameSession = $true
        effectivePaths = 1; maxTargetPids = 1; duplicateUnresolved = $false; foreignPreserved = $true; additionBlocked = $true
        increments = $increments; stillRetrying = $false }
}

function Invoke-StopVerification {
    param([string] $Mode, [string] $EvidenceRoot, [string] $FixturePath)
    $path = 'stop-bounds.json'
    if ($Mode -eq 'Native') {
        if ($FixturePath) { throw 'Event fixtures are accepted only in Fixture mode.' }
        $missing = @('version: exact installed host version is not confirmed for this run', 'entry_version: loaded entry is unavailable',
            'command_class: loaded entry is unavailable', 'cwd_class: loaded entry is unavailable',
            'manifest lookup key (host, serversKey, server): rendered entry and actual load are unavailable',
            'digest: recomputation from the copy actually loaded is unavailable',
            'Version-specific per-server attempt cap, deadline, grace, settings and load confirmation are unverified',
            'Isolated baseline, finite budget, logical attempt source, fault server, rendered-entry merge and healthy MCP control are unverified')
        $cells = @(foreach ($hostName in @('claude', 'codex', 'copilot', 'opencode', 'agy')) {
            @{ host = $hostName; result = 'Blocked'; executionStarted = $false; entryLoaded = $false; stopState = 'NotRun'; gaps = $missing; ownership = 'The isolated render entry is not ccync-managed.' }
        })
        Write-EvidenceJson (Join-Path $EvidenceRoot $path) @{ mode = 'Native'; cells = $cells; completed = $false }
        return @{ result = 'Blocked'; reason = 'Native stop prerequisites are unverified.'; executionStarted = $false; checks = @($cells | ForEach-Object { @{ id = $_.host; result = 'Blocked'; reason = 'Native prerequisites are missing.'; evidencePaths = @($path) } }); gaps = $missing; missingPrerequisites = $missing; evidencePaths = @($path) }
    }
    $scenarios = @(
        @{ name = 'native-stop'; changes = @{}; expected = 'Passed' },
        @{ name = 'native-queue-cleared'; changes = @{ stopKind = 'native-queue-cleared' }; expected = 'Passed' },
        @{ name = 'global'; changes = @{ scope = 'global' }; expected = 'Failed' },
        @{ name = 'unloaded'; changes = @{ settingsLoaded = $false; entryLoaded = $false }; expected = 'Failed' },
        @{ name = 'timeout'; changes = @{ stopKind = 'harness-timeout' }; expected = 'Failed' },
        @{ name = 'manual-stop'; changes = @{ stopKind = 'manual-disable' }; expected = 'Failed' },
        @{ name = 'cleanup'; changes = @{ stopKind = 'cleanup' }; expected = 'Failed' },
        @{ name = 'dual-path'; changes = @{ effectivePaths = 2 }; expected = 'Failed' },
        @{ name = 'multi-pid'; changes = @{ maxTargetPids = 2 }; expected = 'Failed' },
        @{ name = 'late-attempt'; changes = @{ attemptsAfterDeadline = 1; stillRetrying = $true }; expected = 'Failed' },
        @{ name = 'attempt-cap'; changes = @{ attempts = 4 }; expected = 'Failed' },
        @{ name = 'grace-unobserved'; changes = @{ observedThroughGrace = $false }; expected = 'Failed' },
        @{ name = 'fault-before-load'; changes = @{ faultAfterLoad = $false }; expected = 'Failed' },
        @{ name = 'healthy-failed'; changes = @{ healthyCallSucceeded = $false }; expected = 'Failed' },
        @{ name = 'healthy-other-session'; changes = @{ sameSession = $false }; expected = 'Failed' },
        @{ name = 'duplicate-blocked'; changes = @{ duplicateUnresolved = $true }; expected = 'Passed' },
        @{ name = 'unresolved-duplicate'; changes = @{ duplicateUnresolved = $true; foreignPreserved = $false; additionBlocked = $false }; expected = 'Failed' }
    )
    $controls = @(); $checks = @()
    foreach ($scenario in $scenarios) {
        $observation = New-StopFixture
        foreach ($key in $scenario.changes.Keys) { $observation[$key] = $scenario.changes[$key] }
        $assessment = Get-StopAssessment $observation
        $controls += @{ name = $scenario.name; observation = $observation; assessment = $assessment; expected = $scenario.expected }
        $checks += @{ id = $scenario.name; result = $(if ($assessment.result -eq $scenario.expected) { 'Passed' } else { 'Failed' }); reason = 'Synthetic stop control.'; evidencePaths = @($path) }
    }
    foreach ($operation in @('preview', 'doctor', 'sync1', 'sync2')) {
        foreach ($counter in @('use', 'launch', 'reload', 'notify')) {
            $observation = New-StopFixture; $observation.increments[$operation][$counter] = 1
            $assessment = Get-StopAssessment $observation; $name = "$operation-$counter"
            $controls += @{ name = $name; observation = $observation; assessment = $assessment; expected = 'Failed' }
            $checks += @{ id = $name; result = $(if ($assessment.result -eq 'Failed') { 'Passed' } else { 'Failed' }); reason = 'Synthetic nonzero increment control.'; evidencePaths = @($path) }
        }
    }
    if ($FixturePath) { throw 'The stop fixture uses its recorded built-in controls and does not accept an event fixture.' }
    $gaps = @('All stop observations and counters are synthetic. Native isolation, settings, load, attempts and healthy control remain unproven.')
    Write-EvidenceJson (Join-Path $EvidenceRoot $path) @{ mode = 'Fixture'; synthetic = $true; controls = $controls; completed = $true; gaps = $gaps }
    $failed = @($checks | Where-Object { $_.result -ne 'Passed' }).Count
    return @{ result = $(if ($failed) { 'Failed' } else { 'Passed' }); reason = 'Synthetic stop controls completed.'; executionStarted = $true; checks = $checks; gaps = $gaps; missingPrerequisites = @(); evidencePaths = @($path) }
}
