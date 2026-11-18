function Get-QualificationFixtureCandidate {
    param($Metadata, $Stop, $Window, $Manifest)
    $gaps = [Collections.Generic.List[string]]::new()
    foreach ($field in @('host', 'version', 'entry_version', 'command_class', 'cwd_class', 'host_digest', 'binary_digest')) {
        foreach ($record in @($Metadata, $Stop, $Window)) {
            if (-not $record.ContainsKey($field) -or [string]::IsNullOrWhiteSpace([string]$record[$field])) { $gaps.Add("missing identity: $field"); break }
            if ($record[$field] -cne $Metadata[$field]) { $gaps.Add("identity drift: $field"); break }
        }
    }
    foreach ($record in @($Stop, $Window)) {
        if ($record.result -cne 'Passed' -or -not $record.completed -or -not $record.entryLoaded) { $gaps.Add('evidence is not a complete loaded Pass') }
        if (-not $record.ContainsKey('lookupKey') -or -not $record.lookupKey) { $gaps.Add('missing manifest lookup key'); continue }
        $key = $record.lookupKey
        foreach ($field in @('host', 'serversKey', 'server')) {
            if (-not $key.ContainsKey($field) -or [string]::IsNullOrWhiteSpace([string]$key[$field])) { $gaps.Add("missing lookup identity: $field") }
        }
        $items = @($Manifest | Where-Object { $_.host -ceq $key.host -and $_.serversKey -ceq $key.serversKey -and $_.server -ceq $key.server })
        if ($items.Count -ne 1 -or $key.host -cne $record.host) { $gaps.Add('manifest item missing or ambiguous'); continue }
        if (-not $record.ContainsKey('loadedEntryDigest') -or $record.loadedEntryDigest -cne $items[0].sha256 -or
            $record.loadedEntryDigest -cnotmatch '^sha256:[0-9a-f]{64}$') { $gaps.Add('loaded entry digest mismatch') }
    }
    if (-not $Stop.settingsLoaded) { $gaps.Add('settings load missing') }
    if (-not $Stop.healthyAfterStop) { $gaps.Add('healthy control missing') }
    if (-not $Window.windowless) { $gaps.Add('windowless observation missing') }
    if (-not $Stop.evidenceId -or -not $Window.evidenceId) { $gaps.Add('evidence ID missing') }
    if ($Stop.maxAttempts -le 0 -or $Stop.terminationMs -lt 0 -or $Stop.graceMs -lt 0) { $gaps.Add('stop bounds missing') }
    if ($gaps.Count) { return @{ row = $null; gaps = @($gaps.ToArray()) } }
    return @{ row = [ordered]@{ host = $Metadata.host; host_version = $Metadata.version; host_digest = $Metadata.host_digest
        entry_version = $Metadata.entry_version; command_class = $Metadata.command_class; cwd_class = $Metadata.cwd_class
        evidence_id = "$($Stop.evidenceId)+$($Window.evidenceId)"; windowless = $true; max_attempts = $Stop.maxAttempts
        termination_ms = $Stop.terminationMs; observation_grace_ms = $Stop.graceMs; healthy_after_stop = $true }; gaps = @() }
}

function Invoke-QualificationVerification {
    param([string] $Mode, [string] $EvidenceRoot, [string] $FixturePath)
    if ($FixturePath) { throw 'Custom qualification fixtures are not supported.' }
    $paths = @('qualification.json', 'candidate-table.json')
    if ($Mode -eq 'Native') {
        $gaps = @('Native loaded-entry digest, exact identity, stop evidence and window evidence are unavailable.',
            'The production resolver does not supply local facts to its matcher.')
        $cells = @('claude', 'codex', 'copilot', 'opencode', 'agy' | ForEach-Object { @{ host = $_; result = 'Blocked'; gaps = $gaps } })
        Write-EvidenceJson (Join-Path $EvidenceRoot $paths[0]) @{ mode = 'Native'; completed = $false; cells = $cells; gaps = $gaps }
        Write-EvidenceJson (Join-Path $EvidenceRoot $paths[1]) @()
        return @{ result = 'Blocked'; reason = 'Native qualification evidence is unavailable.'; executionStarted = $false; checks = @(); gaps = $gaps; missingPrerequisites = $gaps; evidencePaths = $paths }
    }
    $checks = @(); $controls = @()
    foreach ($scenario in @('complete', 'missing-settings', 'missing-healthy', 'identity-drift', 'stop-entry-drift', 'window-entry-drift', 'manifest-missing', 'different-group', 'missing-identity', 'corrupt-table', 'no-pass')) {
        $digest = 'sha256:' + ('a' * 64)
        $metadata = @{ host = 'codex'; version = 'fixture'; entry_version = '1'; command_class = 'fixture'; cwd_class = 'fixture'; host_digest = $digest; binary_digest = $digest }
        $stop = @{}; $window = @{}
        foreach ($key in $metadata.Keys) { $stop[$key] = $metadata[$key]; $window[$key] = $metadata[$key] }
        foreach ($record in @($stop, $window)) { $record.result = 'Passed'; $record.completed = $true; $record.entryLoaded = $true; $record.lookupKey = @{ host = 'codex'; serversKey = 'mcpServers'; server = 'fixture' }; $record.loadedEntryDigest = $digest; $record.evidenceId = 'synthetic' }
        $stop.settingsLoaded = $true; $stop.healthyAfterStop = $true; $stop.maxAttempts = 1; $stop.terminationMs = 1; $stop.graceMs = 1; $window.windowless = $true
        $manifest = @(@{ host = 'codex'; serversKey = 'mcpServers'; server = 'fixture'; sha256 = $digest })
        switch ($scenario) {
            'missing-settings' { $stop.settingsLoaded = $false }
            'missing-healthy' { $stop.healthyAfterStop = $false }
            'identity-drift' { $stop.host_digest = 'drift' }
            'stop-entry-drift' { $stop.loadedEntryDigest = 'drift' }
            'window-entry-drift' { $window.loadedEntryDigest = 'drift' }
            'manifest-missing' { $manifest = @() }
            'different-group' { $window.version = 'other' }
            'missing-identity' { $window.Remove('cwd_class') }
            'corrupt-table' { $manifest = @() }
            'no-pass' { $stop.result = 'Blocked' }
        }
        $assessment = Get-QualificationFixtureCandidate $metadata $stop $window $manifest
        $passed = ($null -ne $assessment.row) -eq ($scenario -eq 'complete')
        $checks += @{ id = $scenario; result = $(if ($passed) { 'Passed' } else { 'Failed' }); reason = 'Synthetic qualification control.'; evidencePaths = @($paths[0]) }
        $controls += @{ name = $scenario; candidate = $assessment.row; gaps = $assessment.gaps }
    }
    Write-EvidenceJson (Join-Path $EvidenceRoot $paths[0]) @{ mode = 'Fixture'; synthetic = $true; controls = $controls; completed = $true }
    Write-EvidenceJson (Join-Path $EvidenceRoot $paths[1]) @()
    $failed = @($checks | Where-Object { $_.result -ne 'Passed' }).Count
    return @{ result = $(if ($failed) { 'Failed' } else { 'Passed' }); reason = 'Synthetic controls completed; Native qualification remains unproven.'; executionStarted = $true; checks = $checks; gaps = @('Synthetic candidates are not qualification evidence.'); missingPrerequisites = @(); evidencePaths = $paths }
}
