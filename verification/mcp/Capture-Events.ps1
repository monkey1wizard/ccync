function Get-PassiveInterventionCounts {
    param([string[]] $Interventions = @())
    $counts = [ordered]@{ use = 0; connect = 0; launch = 0; write = 0; reload = 0; notify = 0; restart = 0; shutdown = 0; desktop = 0 }
    foreach ($intervention in $Interventions) {
        if (-not $counts.Contains($intervention)) { throw 'Unknown fixture intervention.' }
        $counts[$intervention]++
    }
    return $counts
}

function Invoke-PassiveCounterSelfTest {
    $names = @('use', 'connect', 'launch', 'write', 'reload', 'notify', 'restart', 'shutdown', 'desktop')
    $checks = @()
    foreach ($injected in $names) {
        $counts = Get-PassiveInterventionCounts @($injected)
        $failed = @($names | Where-Object { $counts[$_] -ne $(if ($_ -eq $injected) { 1 } else { 0 }) })
        $checks += @{ id = 'counter-' + $injected; result = $(if ($failed.Count) { 'Failed' } else { 'Passed' }); reason = 'Synthetic intervention affects only its own counter.'; evidencePaths = @('passive-report.json') }
    }
    return $checks
}

function Invoke-PassiveFixtureCapture {
    param([string] $EvidenceRoot, [string] $FixturePath, [string] $ObservationHost,
        [string] $ObservationSession, [double] $ObservationSeconds)
    $start = [DateTimeOffset]::UtcNow
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $selected = @(); $gaps = @('Events before observation began are not visible.', 'A supplied fixture source is sampled once; later events may be unseen.', 'Fixture observation does not establish Native capture or clock calibration.')
    if ($FixturePath) {
        $item = Get-Item -LiteralPath $FixturePath -Force -ErrorAction Stop
        if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $item.Length -gt 262144) { throw 'Fixture source must be a regular file within the size bound.' }
        try { $inputFile = Get-Content -LiteralPath $FixturePath -Raw -ErrorAction Stop | ConvertFrom-Json }
        catch { throw 'Fixture source could not be read as JSON.' }
        ConvertTo-NormalizedEvents $inputFile | Out-Null
        $sampleTime = [DateTimeOffset]::UtcNow
        foreach ($event in @($inputFile.events)) {
            $time = [DateTimeOffset]::Parse($event.time)
            if ($event.host -ceq $ObservationHost -and $event.session -ceq $ObservationSession -and $time -ge $start -and $time -le $sampleTime -and $timer.Elapsed.TotalSeconds -lt $ObservationSeconds) { $selected += $event }
        }
    } else { $gaps += 'No fixture event source was supplied.' }
    while ($timer.Elapsed.TotalSeconds -lt $ObservationSeconds) {
        $remaining = $ObservationSeconds - $timer.Elapsed.TotalSeconds
        if ($remaining -gt 0) { Start-Sleep -Milliseconds ([Math]::Max(1, [Math]::Min(100, [Math]::Ceiling($remaining * 1000)))) }
    }
    $end = [DateTimeOffset]::UtcNow
    $timer.Stop()
    $boundedStop = $timer.Elapsed.TotalSeconds -ge $ObservationSeconds
    $rows = @(@{ seq = 1; time = $start.ToString('o'); host = $ObservationHost; hostVersion = '1.0'; session = $ObservationSession; server = 'fixture-observer'; operation = 'fixture-observation'; attempt = '0'; parent = $null; kind = 'observe-start'; source = 'unknown'; actor = 'unknown'; result = 'ok'; digest = $null })
    foreach ($event in $selected) {
        $rows += @{ seq = ([long]$event.seq + 2); time = $event.time; host = $event.host; hostVersion = $event.hostVersion; session = $event.session; server = $event.server; operation = $event.operation; attempt = $event.attempt; parent = $(if ($null -eq $event.parent) { $null } else { [long]$event.parent + 2 }); kind = $event.kind; source = $event.source; actor = $event.actor; result = $event.result; digest = $null }
    }
    $last = [long]$rows[-1].seq
    $rows += @{ seq = ($last + 1); time = $end.ToString('o'); host = $ObservationHost; hostVersion = '1.0'; session = $ObservationSession; server = 'fixture-observer'; operation = 'fixture-observation'; attempt = '0'; parent = $null; kind = 'observe-end'; source = 'unknown'; actor = 'unknown'; result = 'ok'; digest = $null }
    Write-EvidenceJson (Join-Path $EvidenceRoot 'events.json') (ConvertTo-NormalizedEvents @{ version = 1; events = $rows })
    Write-EvidenceJson (Join-Path $EvidenceRoot 'event-provenance.json') @{ mode = 'Fixture'; source = $(if ($FixturePath) { 'synthetic fixture snapshot' } else { 'no fixture source' }); host = $ObservationHost; session = 'aliased in events.json'; start = $start.ToString('o'); end = $end.ToString('o'); durationSeconds = $ObservationSeconds; nativeObservation = $false; gaps = $gaps }
    $counts = Get-PassiveInterventionCounts
    Write-EvidenceJson (Join-Path $EvidenceRoot 'passive-report.json') @{ mode = 'Fixture'; interventionCounts = $counts; boundedStop = $boundedStop; elapsedSeconds = $timer.Elapsed.TotalSeconds; requestedSeconds = $ObservationSeconds }
    $names = @('use', 'connect', 'launch', 'write', 'reload', 'notify', 'restart', 'shutdown', 'desktop')
    $zero = $counts.Count -eq $names.Count -and @($names | Where-Object { -not $counts.Contains($_) -or $counts[$_] -isnot [int] -or $counts[$_] -ne 0 }).Count -eq 0
    $checks = @(@{ id = 'passive-fixture'; result = $(if ($boundedStop -and $zero) { 'Passed' } else { 'Failed' }); reason = 'Bounded fixture observation and intervention counters checked.'; evidencePaths = @('events.json', 'event-provenance.json', 'passive-report.json') })
    $checks += @(Invoke-PassiveCounterSelfTest)
    return @{ checks = $checks; gaps = $gaps }
}

function Invoke-EventCapture {
    param([string] $Mode, [string] $EvidenceRoot, [string] $FixturePath, $Table)
    if ($Mode -eq 'Native') {
        $hosts = @(
            foreach ($name in @('claude', 'codex', 'copilot', 'opencode', 'agy')) {
                $versionRecord = $Table.hosts.$name.hostVersion
                $version = $null
                $patterns = @{ claude = '^([0-9]+(?:\.[0-9]+)+) \(Claude Code\)'; codex = '^codex-cli ([0-9]+(?:\.[0-9]+)+)'; copilot = '^GitHub Copilot CLI ([0-9]+(?:\.[0-9]+)+)\.'; opencode = '^opencode v([0-9]+(?:\.[0-9]+)+)'; agy = '^([0-9]+(?:\.[0-9]+)+)' }
                if ($versionRecord -and $versionRecord.output -cmatch $patterns[$name]) { $version = $Matches[1] }
                @{ host = $name; version = $version; versionSource = 'recorded capability table'; probeResult = 'Blocked';
                   gaps = @('A verified logical event feed and native capture prerequisites are unavailable.') }
            }
        )
        Write-EvidenceJson (Join-Path $EvidenceRoot 'event-provenance.json') @{ mode = 'Native'; hosts = $hosts; source = 'not observed' }
        return @{ result = 'Blocked'; checks = @($hosts | ForEach-Object { @{ id = $_.host; result = 'Blocked'; reason = "Recorded version: $($_.version). $($_.gaps[0])"; evidencePaths = @('event-provenance.json') } }); gaps = @($hosts | ForEach-Object { "$($_.host): logical event feed unavailable" }) }
    }
    if ($FixturePath) {
        $item = Get-Item -LiteralPath $FixturePath -ErrorAction Stop
        if ($item.Length -gt 262144 -or $item.PSIsContainer) { throw 'Fixture input exceeds the size bound or is not a file.' }
        try { $inputFile = Get-Content -LiteralPath $FixturePath -Raw | ConvertFrom-Json }
        catch { throw 'Fixture input is not valid JSON.' }
    } else {
        $inputFile = '{"version":1,"run":"fixture","events":[{"seq":1,"time":"2026-01-01T00:00:01Z","host":"codex","hostVersion":"1.0","session":"s","server":"fault","operation":"op","attempt":"0","parent":null,"kind":"observe-start","source":"native-config","actor":"unknown","result":"ok","digest":null},{"seq":2,"time":"2026-01-01T00:00:02Z","host":"codex","hostVersion":"1.0","session":"s","server":"fault","operation":"op","attempt":"1","parent":null,"kind":"use","source":"native-config","actor":"agent","result":"ok","digest":null},{"seq":3,"time":"2026-01-01T00:00:03Z","host":"codex","hostVersion":"1.0","session":"s","server":"fault","operation":"op","attempt":"1","parent":2,"kind":"failure","source":"native-config","actor":"agent","result":"error","digest":null},{"seq":4,"time":"2026-01-01T00:00:04Z","host":"codex","hostVersion":"1.0","session":"s","server":"fault","operation":"op","attempt":"0","parent":null,"kind":"observe-end","source":"native-config","actor":"unknown","result":"ok","digest":null}]}' | ConvertFrom-Json
    }
    $output = ConvertTo-NormalizedEvents $inputFile
    Write-EvidenceJson (Join-Path $EvidenceRoot 'events.json') $output
    $controls = @(
        @{ name = 'native-source'; source = 'native-config'; kinds = @('use', 'dispatch', 'failure') },
        @{ name = 'http-no-local-pid'; source = 'native-config'; kinds = @('use', 'dispatch', 'failure') },
        @{ name = 'pre-handshake-failure'; source = 'native-config'; kinds = @('use', 'dispatch', 'failure') },
        @{ name = 'dropped-connection'; source = 'native-config'; kinds = @('dispatch', 'failure') }
    )
    $checks = @(@{ id = 'fixture-events'; result = 'Passed'; reason = 'Fixture events normalized.'; evidencePaths = @('events.json','event-provenance.json') })
    foreach ($control in $controls) {
        $rows = @(); $sequence = 0
        foreach ($kind in (@('observe-start') + $control.kinds + @('observe-end'))) {
            $sequence++
            $rows += @{ seq = $sequence; time = "2026-01-01T00:00:0${sequence}Z"; host = 'codex'; hostVersion = '1.0'; session = 'fixture-session'; server = $control.name; operation = 'fixture-operation'; attempt = 'fixture-attempt'; parent = $(if ($sequence -le 2 -or $kind -eq 'observe-end') { $null } else { $sequence - 1 }); kind = $kind; source = $control.source; actor = 'agent'; result = $(if ($kind -eq 'failure') { 'error' } else { 'ok' }); digest = $null }
        }
        $file = 'control-' + $control.name + '.json'
        $normalized = ConvertTo-NormalizedEvents @{ version = 1; events = $rows }
        Write-EvidenceJson (Join-Path $EvidenceRoot $file) $normalized
        $checks += @{ id = $control.name; result = 'Passed'; reason = 'Synthetic logical events normalized as v1.'; evidencePaths = @($file, 'event-provenance.json') }
    }
    $gaps = @('Native event source and clock calibration not observed.', 'v1 does not encode transport, process ID, handshake phase, or connection-drop cause; fixture control names supply scenario context only.')
    Write-EvidenceJson (Join-Path $EvidenceRoot 'event-provenance.json') @{ mode = 'Fixture'; source = 'synthetic fixture'; eventFile = 'events.json'; nativeObservation = $false; controls = @($controls | ForEach-Object { @{ name = $_.name; eventFile = 'control-' + $_.name + '.json'; source = 'synthetic fixture'; nativeObservation = $false } }); gaps = $gaps }
    return @{ result = 'Passed'; checks = $checks; gaps = $gaps }
}

function ConvertTo-NormalizedEvents {
    param($inputFile)
    if ($inputFile.version -ne 1 -or @($inputFile.events).Count -gt 1000) { throw 'Fixture version or event count is invalid.' }
    $aliases = @{}; $nextAlias = 0; $events = @(); $lastSeq = -1
    foreach ($event in @($inputFile.events)) {
        if ($event.seq -isnot [long] -and $event.seq -isnot [int]) { throw 'Invalid sequence.' }
        if ($event.seq -le $lastSeq -or $event.seq -lt 0) { throw 'Invalid sequence.' }; $lastSeq = $event.seq
        if ($event.host -cnotin @('claude','codex','copilot','opencode','agy') -or $event.hostVersion -cnotmatch '^[0-9]+(?:\.[0-9]+){1,3}$' -or $event.hostVersion.Length -gt 32) { throw 'Invalid host identity.' }
        if ($event.kind -cnotin @('use','dispatch','launch-request','failure','write','notify','reload','schedule','terminate','health-call','observe-start','observe-end','bound-exceeded','manual-stop') -or
            $event.source -cnotin @('native-config','plugin','personal-list','unknown') -or $event.actor -cnotin @('agent','ccync','third-party','unknown') -or $event.result -cnotin @('ok','error','unchanged','none')) { throw 'Invalid event classification.' }
        $parsedTime = [DateTimeOffset]::MinValue
        if ($event.time -cnotmatch '^\d{4}-\d\d-\d\d[Tt]\d\d:\d\d:\d\d(?:\.\d{1,9})?(?:[Zz]|[+-]\d\d:\d\d)$' -or -not [DateTimeOffset]::TryParse($event.time, [ref]$parsedTime)) { throw 'Invalid timestamp.' }
        if ($null -ne $event.parent -and (($event.parent -isnot [long] -and $event.parent -isnot [int]) -or $event.parent -lt 0)) { throw 'Invalid parent.' }
        $mapped = @{}
        foreach ($field in @('session','server','operation','attempt')) {
            $value = [string]$event.$field
            if (-not $value -or $value.Length -gt 1024) { throw 'Invalid fixture identity.' }
            $key = $field + ':' + $value
            if (-not $aliases.ContainsKey($key)) { $nextAlias++; $aliases[$key] = 'id' + $nextAlias }
            $mapped[$field] = $aliases[$key]
        }
        $events += [ordered]@{ seq = $event.seq; time = $event.time; host = $event.host; hostVersion = $event.hostVersion; session = $mapped.session; server = $mapped.server; operation = $mapped.operation; attempt = $mapped.attempt; parent = $event.parent; kind = $event.kind; source = $event.source; actor = $event.actor; result = $event.result; digest = $null }
    }
    $output = [ordered]@{ version = 1; run = 'fixture'; events = $events }
    $serialized = ConvertTo-Json -InputObject $output -Depth 20
    if ([Text.Encoding]::UTF8.GetByteCount($serialized) -gt 262144) { throw 'Normalized evidence exceeds the size bound.' }
    return $output
}
