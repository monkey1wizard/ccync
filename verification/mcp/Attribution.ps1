function Get-FixtureAttribution {
    param($Events)
    $requests = @($Events | Where-Object { $_.kind -in @('dispatch', 'launch-request') })
    $gaps = @(); $trigger = 'Unknown'; $initiator = 'Unknown'; $chain = @(); $sources = @(); $readers = @()
    if ($requests.Count -ne 1) { $gaps += 'A unique request is required.' }
    else {
        $request = $requests[0]; $current = $request; $seen = @{}
        while ($null -ne $current) {
            if ($seen.ContainsKey([string]$current.seq)) { $gaps += 'Parent chain contains a cycle.'; break }
            $seen[[string]$current.seq] = $true; $chain += $current
            if ($null -eq $current.parent) { break }
            $parents = @($Events | Where-Object { $_.seq -eq $current.parent })
            if ($parents.Count -ne 1 -or $parents[0].seq -ge $current.seq -or $parents[0].operation -cne $request.operation -or $parents[0].server -cne $request.server -or $parents[0].session -cne $request.session -or $parents[0].host -cne $request.host) { $gaps += 'Parent chain is missing, ambiguous, or outside the operation.'; break }
            $current = $parents[0]
        }
        $sources = @($chain | Where-Object { $_.source -ne 'unknown' } | ForEach-Object { $_.source } | Select-Object -Unique)
        $readers = @($chain | Where-Object { $_.kind -in @('use', 'dispatch', 'launch-request') } | ForEach-Object { $_.actor } | Select-Object -Unique)
        if ($request.actor -in @('agent', 'ccync')) { $initiator = $request.actor }
        if ($sources.Count -gt 1) { $gaps += 'Conflicting sources in parent chain.' }
        if (@($chain | Where-Object { $_.source -eq 'unknown' }).Count) { $gaps += 'Source missing in parent chain.' }
        if (-not $gaps.Count -and $initiator -ne 'Unknown') {
            if (@($chain | Where-Object { $_.kind -eq 'reload' -and $_.actor -eq 'ccync' -and $_.result -eq 'ok' }).Count) { $trigger = 'ConfigReload' }
            elseif ($initiator -eq 'ccync') { $trigger = 'DirectRequest' }
            elseif (@($chain | Where-Object { $_.kind -eq 'use' -and $_.actor -eq 'agent' }).Count) { $trigger = 'UserOperation' }
        }
    }
    if ($trigger -eq 'Unknown') { $gaps += 'Trigger not established.' }
    return @{ initiator = $initiator; trigger = $trigger; sources = $sources; readers = $readers; parentChain = @($chain | ForEach-Object { $_.seq }); gaps = $gaps }
}

function New-AttributionEvent {
    param([int] $Seq, [string] $Kind, [string] $Actor, $Parent, [string] $Source = 'native-config', [string] $Result = 'ok')
    return @{ seq = $Seq; time = '2026-01-01T00:00:00Z'; host = 'codex'; hostVersion = '1.0'; session = 'fixture'; server = 'fault'; operation = 'same-operation'; attempt = 'attempt'; parent = $Parent; kind = $Kind; source = $Source; actor = $Actor; result = $Result; digest = $null }
}

function Invoke-AttributionVerification {
    param([string] $Mode, [string] $EvidenceRoot, [string] $FixturePath)
    $groups = @('A', 'B', 'C', 'C-withdrawal', 'D')
    $checks = @(); $paths = @(); $gaps = @()
    if ($Mode -eq 'Native') {
        if ($FixturePath) { throw 'Event fixtures are accepted only in Fixture mode.' }
        $missing = @('Verified isolated host capability, budget, logical event feed, fault and healthy controls, and render artifact loading are not established.')
        $cells = @(foreach ($hostName in @('claude', 'codex', 'copilot', 'opencode', 'agy')) { foreach ($group in $groups) {
            @{ host = $hostName; group = $group; result = 'Blocked'; gaps = $missing; ownership = 'The isolated render entry is not ccync-managed.' }
        } })
        Write-EvidenceJson (Join-Path $EvidenceRoot 'attribution.json') @{ mode = 'Native'; cells = $cells; completed = $false }
        $checks = @($cells | ForEach-Object { @{ id = "$($_.host)-$($_.group)"; result = $_.result; reason = $_.gaps[0]; evidencePaths = @('attribution.json') } })
        return @{ result = 'Blocked'; reason = 'Native attribution prerequisites are unverified.'; executionStarted = $false; checks = $checks; gaps = $missing; missingPrerequisites = $missing; evidencePaths = @('attribution.json') }
    }
    $baseline = '{"fault":"http://127.0.0.1:19001/fault","healthy":"http://127.0.0.1:19001/healthy","operation":"same-operation"}'
    $baselinePath = Join-Path $EvidenceRoot 'baseline.json'
    if ((Test-Path -LiteralPath $baselinePath) -and ((Get-Item -LiteralPath $baselinePath -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Baseline must not be a reparse point.' }
    [IO.File]::WriteAllText($baselinePath, $baseline, [Text.UTF8Encoding]::new($false))
    $baselineDigest = Get-FileDigest $baselinePath; $paths += 'baseline.json'; $results = @()
    foreach ($group in $groups) {
        $directory = Join-Path $EvidenceRoot $group
        if ((Test-Path -LiteralPath $directory) -and ((Get-Item -LiteralPath $directory -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Group directory must not be a reparse point.' }
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
        $snapshot = Join-Path $directory 'snapshot.json'
        if ((Test-Path -LiteralPath $snapshot) -and ((Get-Item -LiteralPath $snapshot -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Snapshot must not be a reparse point.' }
        [IO.File]::WriteAllText($snapshot, $baseline, [Text.UTF8Encoding]::new($false))
        $startDigest = Get-FileDigest $snapshot
        $events = @((New-AttributionEvent 1 'observe-start' 'unknown' $null))
        $writer = 'none'; $outcome = 'none'; $actor = 'agent'; $parent = 2
        if ($group -eq 'D') { $actor = 'ccync'; $parent = $null }
        elseif ($group -eq 'C') {
            $writer = 'ccync'; $outcome = 'changed'
            [IO.File]::WriteAllText($snapshot, $baseline.Replace('/fault', '/changed-fault'), [Text.UTF8Encoding]::new($false))
            $events += (New-AttributionEvent 2 'reload' 'ccync' $null)
        } else {
            if ($group -eq 'B') { $writer = 'ccync'; $outcome = 'unchanged' }
            $events += (New-AttributionEvent 2 'use' 'agent' $null)
        }
        $events += (New-AttributionEvent 3 'dispatch' $actor $parent)
        $events += (New-AttributionEvent 4 'failure' $actor 3 'native-config' 'error')
        $events += (New-AttributionEvent 5 'observe-end' 'unknown' $null)
        $analysis = Get-FixtureAttribution $events
        $expected = if ($group -eq 'C') { 'ConfigReload' } elseif ($group -eq 'D') { 'DirectRequest' } else { 'UserOperation' }
        $ok = $startDigest -eq $baselineDigest -and $analysis.trigger -eq $expected -and (($group -eq 'C') -eq ((Get-FileDigest $snapshot) -ne $baselineDigest))
        $eventsFile = "$group/events.json"; $metadataFile = "$group/attribution.json"; $usageFile = "$group/usage.json"
        Write-EvidenceJson (Join-Path $directory 'events.json') (ConvertTo-NormalizedEvents @{ version = 1; events = $events })
        Write-EvidenceJson (Join-Path $directory 'attribution.json') @{ mode = 'Fixture'; operation = 'same-operation'; fault = 'HTTP fault route'; healthyControl = 'HTTP healthy route'; startDigest = $startDigest; endDigest = (Get-FileDigest $snapshot); writer = $writer; writeOutcome = $outcome; notification = $(if ($group -eq 'C') { 'reload' } else { 'none' }); analysis = $analysis; completed = $true }
        Write-EvidenceJson (Join-Path $directory 'usage.json') @{ mode = 'Fixture'; attempts = 1; usage = 1; allowance = 4; attemptCap = 1; synthetic = $true }
        $paths += @("$group/snapshot.json", $eventsFile, $metadataFile, $usageFile)
        $results += @{ group = $group; result = $(if ($ok) { 'Passed' } else { 'Failed' }); startDigest = $startDigest; completed = $true }
        $checks += @{ id = $group; result = $(if ($ok) { 'Passed' } else { 'Failed' }); reason = 'Synthetic group and reset digest checked.'; evidencePaths = @($eventsFile, $metadataFile, $usageFile, "$group/snapshot.json") }
    }
    $controls = @(
        @{ name = 'with-parent'; events = @((New-AttributionEvent 1 'reload' 'ccync' $null), (New-AttributionEvent 2 'dispatch' 'agent' 1)); expected = 'ConfigReload' },
        @{ name = 'without-parent'; events = @((New-AttributionEvent 1 'reload' 'ccync' $null), (New-AttributionEvent 2 'dispatch' 'agent' $null)); expected = 'Unknown' },
        @{ name = 'same-time'; events = @((New-AttributionEvent 1 'reload' 'ccync' $null), (New-AttributionEvent 2 'dispatch' 'agent' 99)); expected = 'Unknown' },
        @{ name = 'source-conflict'; events = @((New-AttributionEvent 1 'reload' 'ccync' $null 'plugin'), (New-AttributionEvent 2 'dispatch' 'agent' 1)); expected = 'Unknown' }
    )
    foreach ($control in $controls) {
        $analysis = Get-FixtureAttribution $control.events; $file = 'control-' + $control.name + '.json'
        Write-EvidenceJson (Join-Path $EvidenceRoot $file) @{ events = (ConvertTo-NormalizedEvents @{ version = 1; events = $control.events }); analysis = $analysis; expected = $control.expected }
        $paths += $file; $checks += @{ id = $control.name; result = $(if ($analysis.trigger -eq $control.expected) { 'Passed' } else { 'Failed' }); reason = 'Independent synthetic causal control.'; evidencePaths = @($file) }
    }
    if ($FixturePath) {
        $item = Get-Item -LiteralPath $FixturePath -Force
        if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $item.Length -gt 262144) { throw 'Fixture must be a bounded regular file.' }
        $inputFile = Get-Content -LiteralPath $FixturePath -Raw | ConvertFrom-Json
        $normalized = ConvertTo-NormalizedEvents $inputFile
        Write-EvidenceJson (Join-Path $EvidenceRoot 'supplied-control.json') @{ events = $normalized; analysis = (Get-FixtureAttribution $normalized.events) }
        $paths += 'supplied-control.json'
    }
    $gaps = @('Fixture events, usage, and routes are synthetic. Native attribution and safety premises remain unproven.')
    Write-EvidenceJson (Join-Path $EvidenceRoot 'attribution.json') @{ mode = 'Fixture'; groups = $results; completed = $true; gaps = $gaps }
    $paths += 'attribution.json'
    $failed = @($checks | Where-Object { $_.result -ne 'Passed' }).Count
    return @{ result = $(if ($failed) { 'Failed' } else { 'Passed' }); reason = 'Synthetic attribution controls completed.'; executionStarted = $true; checks = $checks; gaps = $gaps; missingPrerequisites = @(); evidencePaths = $paths }
}
