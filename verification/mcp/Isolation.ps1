Set-StrictMode -Version Latest

function Get-IsolatedEnvironment {
    param([string] $Root, [bool] $Cargo)
    $homePath = Join-Path $Root 'home'
    $appData = Join-Path $Root 'appdata'
    $values = @{
        USERPROFILE = $homePath; HOME = $homePath; APPDATA = $appData
        LOCALAPPDATA = (Join-Path $Root 'localappdata')
        XDG_CONFIG_HOME = (Join-Path $Root 'xdg-config'); XDG_STATE_HOME = (Join-Path $Root 'xdg-state')
        XDG_CACHE_HOME = (Join-Path $Root 'xdg-cache'); CCYNC_HOME = (Join-Path $Root 'ccync')
        OPENCODE_CONFIG_CONTENT = $null; OPENCODE_CONFIG = $null; CCYNC_SKILLS = $null
        CLAUDE_CONFIG_DIR = $null; CODEX_HOME = $null; COPILOT_HOME = $null; OPENCODE_CONFIG_DIR = $null
    }
    if (-not $Cargo) {
        $values.CODEX_HOME = Join-Path $homePath '.codex'
        $values.COPILOT_HOME = Join-Path $homePath '.copilot'
        $values.OPENCODE_CONFIG_DIR = Join-Path $appData 'opencode'
    }
    return $values
}

function Test-EvidenceRoot {
    param([string] $Path, [string] $RepositoryRoot)
    $candidate = [IO.Path]::GetFullPath($Path)
    $planRoot = [IO.Path]::GetFullPath((Join-Path $RepositoryRoot '.dev/pipeline/fix-mcp-control-plane-completion'))
    $prefix = $planRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $candidate.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { return $false }
    if ($candidate.Substring($prefix.Length) -notmatch '^T-\d{2}[\\/].+') { return $false }
    $current = $candidate
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            if (((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $false }
        }
        $current = [IO.Path]::GetDirectoryName($current)
    }
    return $true
}

function Write-EvidenceJson {
    param([string] $Path, $Value)
    if (Test-Path -LiteralPath $Path) {
        if (((Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Evidence files must not be reparse points.' }
    }
    [IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $Value -Depth 100) + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
}

function Get-FileDigest {
    param([string] $Path, [switch] $ClaudeMcp)
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            if ($ClaudeMcp) {
                $reader = [IO.StreamReader]::new($stream)
                try {
                    try { $json = $reader.ReadToEnd() | ConvertFrom-Json -ErrorAction Stop }
                    catch { throw 'Claude JSON could not be parsed for MCP subtree comparison.' }
                } finally { $reader.Dispose() }
                $subtrees = [ordered]@{}
                $mcp = $json.PSObject.Properties['mcpServers']
                if ($mcp) { $subtrees['/mcpServers'] = $mcp.Value }
                $projects = $json.PSObject.Properties['projects']
                if ($projects -and $projects.Value) {
                    foreach ($project in $projects.Value.PSObject.Properties) {
                        $projectMcp = $project.Value.PSObject.Properties['mcpServers']
                        if ($projectMcp) { $subtrees["/projects/$($project.Name)/mcpServers"] = $projectMcp.Value }
                    }
                }
                function Normalize-JsonValue($Value) {
                    if ($null -eq $Value) { return $null }
                    if ($Value -is [Collections.IDictionary]) {
                        $normalized = [ordered]@{}
                        foreach ($key in @($Value.Keys | Sort-Object -CaseSensitive)) { $normalized[$key] = Normalize-JsonValue $Value[$key] }
                        return $normalized
                    }
                    if ($Value -is [pscustomobject]) {
                        $normalized = [ordered]@{}
                        foreach ($property in @($Value.PSObject.Properties | Sort-Object Name -CaseSensitive)) { $normalized[$property.Name] = Normalize-JsonValue $property.Value }
                        return $normalized
                    }
                    if ($Value -is [Array]) { return ,@(foreach ($entry in $Value) { Normalize-JsonValue $entry }) }
                    return $Value
                }
                $bytes = [Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject (Normalize-JsonValue $subtrees) -Depth 100 -Compress))
                $digest = $sha.ComputeHash($bytes)
            } else { $digest = $sha.ComputeHash($stream) }
            return ([BitConverter]::ToString($digest)).Replace('-', '').ToLowerInvariant()
        } finally { $sha.Dispose() }
    } finally { $stream.Dispose() }
}

function ConvertTo-ClassificationRegex {
    param([string] $Pattern)
    if (-not $Pattern -or $Pattern -match '[\\:?\[\]]' -or $Pattern.StartsWith('/') -or $Pattern.EndsWith('/')) { throw "Invalid relative classification pattern: $Pattern" }
    $segments = $Pattern.Split('/'); $fixed = $false; $expression = '^'
    for ($index = 0; $index -lt $segments.Count; $index++) {
        $segment = $segments[$index]
        if ($segment -in @('', '.', '..')) { throw "Invalid classification segment: $Pattern" }
        if ($segment -eq '**') {
            if (-not $fixed) { throw 'A recursive wildcard requires a preceding fixed segment.' }
            if ($index -eq $segments.Count - 1) { $expression = $expression.TrimEnd('/') + '(?:/.*)?' } else { $expression += '(?:[^/]+/)*' }
        } else {
            if ($segment.Contains('**')) { throw 'Recursive wildcards must occupy a whole segment.' }
            if (-not $segment.Contains('*')) { $fixed = $true }
            $expression += [regex]::Escape($segment).Replace('\*', '[^/]*')
            if ($index -lt $segments.Count - 1) { $expression += '/' }
        }
    }
    return [regex]::new($expression + '$', [Text.RegularExpressions.RegexOptions]::IgnoreCase)
}

function Get-ClassificationTable {
    param([string] $Path)
    $table = @{ allowedVolatile = @(); excludedEphemeral = @() }
    if ($Path) {
        $inputTable = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
        foreach ($property in $inputTable.PSObject.Properties) {
            if ($property.Name -notin @('allowedVolatile', 'excludedEphemeral')) { throw "Unknown classification: $($property.Name)" }
            $table[$property.Name] = @($property.Value)
        }
    }
    $homeTargets = @('.claude.json', '.codex/config.toml', '.copilot/mcp-config.json', '.gemini/config/mcp_config.json')
    $configTargets = @('opencode/opencode.json')
    $configRoots = @('', '.config', 'AppData/Roaming', 'Library/Application Support')
    $protected = @($homeTargets) + @('.claude/.credentials.json', '.codex/auth.json', '.copilot/credentials.json',
        '.local/share/opencode/auth.json', '.gemini/oauth_creds.json')
    foreach ($root in $configRoots) {
        foreach ($target in @($configTargets) + @('opencode/auth.json')) {
            $protected += if ($root) { "$root/$target" } else { $target }
        }
    }
    foreach ($kind in @('allowedVolatile', 'excludedEphemeral')) {
        $compiled = @()
        foreach ($pattern in $table[$kind]) {
            if ($pattern -isnot [string]) { throw 'Classification patterns must be strings.' }
            $matcher = ConvertTo-ClassificationRegex $pattern
            foreach ($path in $protected) { if ($matcher.IsMatch($path)) { throw "Classification matches a managed target or credentials: $pattern" } }
            foreach ($privateRoot in @('.ccync', 'AppData/Local/ccync', 'AppData/Roaming/ccync', '.config/ccync', '.local/state/ccync', '.cache/ccync')) {
                if (Test-ClassificationPrefix $pattern $privateRoot) { throw "Classification may cover ccync state: $pattern" }
            }
            # Conservative rejection covers private state and unknown credential filenames without reading secrets.
            if ($pattern -match '(^|/)(\.ccync|ccync)(/|$)' -or $pattern -match '(?i)(auth|credential|secret|token)' -or
                $pattern.Split('/')[0].Contains('*')) { throw "Classification may cover protected state or credentials: $pattern" }
            if ($kind -eq 'excludedEphemeral' -and $pattern.Contains('*')) { throw 'ExcludedEphemeral requires exact paths.' }
            $compiled += @{ pattern = $pattern; regex = $matcher }
        }
        $table[$kind] = @($compiled)
    }
    return $table
}

function Test-ClassificationPrefix {
    param([string] $Pattern, [string] $Prefix)
    $patternSegments = $Pattern.Split('/'); $prefixSegments = $Prefix.Split('/')
    function Match-Prefix([int] $PatternIndex, [int] $PrefixIndex) {
        if ($PrefixIndex -eq $prefixSegments.Count) { return $true }
        if ($PatternIndex -eq $patternSegments.Count) { return $false }
        if ($patternSegments[$PatternIndex] -eq '**') {
            return ((Match-Prefix ($PatternIndex + 1) $PrefixIndex) -or (Match-Prefix $PatternIndex ($PrefixIndex + 1)))
        }
        $segmentRegex = '^' + [regex]::Escape($patternSegments[$PatternIndex]).Replace('\*', '.*') + '$'
        return ($prefixSegments[$PrefixIndex] -imatch $segmentRegex -and (Match-Prefix ($PatternIndex + 1) ($PrefixIndex + 1)))
    }
    return Match-Prefix 0 0
}

function Get-PathClassification {
    param([string] $Relative, $Classification)
    foreach ($kind in @('allowedVolatile', 'excludedEphemeral')) {
        foreach ($entry in $Classification[$kind]) { if ($entry.regex.IsMatch($Relative)) { return $kind } }
    }
    return 'persistent'
}

function Get-FixtureInventory {
    param([string] $Root, $Classification = @{ allowedVolatile = @(); excludedEphemeral = @() })
    $records = @{}; $gaps = [Collections.Generic.List[string]]::new()
    $excluded = [Collections.Generic.List[object]]::new(); $volatile = [Collections.Generic.List[string]]::new()
    $claudeOtherFields = [Collections.Generic.List[string]]::new()
    function Visit-Item([string] $Path, [string] $Relative) {
        $kind = Get-PathClassification $Relative $Classification
        $metadata = [ordered]@{ path = $Relative; classification = $kind; type = 'unknown' }
        try {
            $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
            $metadata.type = if ($item.PSIsContainer) { 'directory' } else { 'file' }
            $metadata['mtime'] = $item.LastWriteTimeUtc.ToString('o')
            if (-not $item.PSIsContainer) { $metadata['size'] = $item.Length }
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                $metadata.type = 'reparse'; $metadata['target'] = @($item.Target) -join ';'; $records[$Relative] = $metadata
                if ($kind -eq 'allowedVolatile') { $volatile.Add($Relative) }
                return
            }
            if ($item.PSIsContainer) {
                if ($Relative) { $records[$Relative] = [ordered]@{ type = 'directory'; classification = $kind } }
                $enumerationErrors = @()
                $children = @(Get-ChildItem -LiteralPath $Path -Force -ErrorAction SilentlyContinue -ErrorVariable enumerationErrors)
                foreach ($errorRecord in $enumerationErrors) { $gaps.Add("${Relative}: $($errorRecord.Exception.Message)") }
                foreach ($child in $children) {
                    $name = if ($Relative) { "$Relative/$($child.Name)" } else { $child.Name }
                    Visit-Item $child.FullName $name
                }
            } else {
                $isClaude = $Relative -ieq '.claude.json'
                $metadata['hash'] = Get-FileDigest $Path -ClaudeMcp:$isClaude
                if ($isClaude) {
                    $metadata.type = 'claude-mcp'; $metadata.Remove('size'); $metadata.Remove('mtime')
                    $claudeOtherFields.Add('.claude.json fields outside /mcpServers and /projects/*/mcpServers')
                }
                $records[$Relative] = $metadata
                if ($kind -eq 'allowedVolatile') { $volatile.Add($Relative) }
            }
        } catch {
            $metadata['readError'] = $_.Exception.Message
            if ($kind -eq 'excludedEphemeral' -and $metadata.type -ne 'directory') { $excluded.Add($metadata) }
            else { $gaps.Add("${Relative}: $($_.Exception.Message)") }
        }
    }
    Visit-Item $Root ''
    return @{ records = $records; gaps = @($gaps.ToArray()); excludedEphemeral = @($excluded.ToArray());
        allowedVolatile = @($volatile.ToArray()); excludedClaudeFields = @($claudeOtherFields.ToArray()) }
}

function Compare-FixtureInventory {
    param($Before, $After, $Classification = @{ allowedVolatile = @(); excludedEphemeral = @() }, [string[]] $ExcludedPaths = @())
    $changes = [Collections.Generic.List[string]]::new()
    foreach ($key in @(@($Before.Keys) + @($After.Keys) | Sort-Object -Unique)) {
        if ($key -in $ExcludedPaths -or (Get-PathClassification $key $Classification) -eq 'allowedVolatile') { continue }
        if (-not $Before.ContainsKey($key) -or -not $After.ContainsKey($key) -or
            ((ConvertTo-Json -InputObject $Before[$key] -Depth 100 -Compress) -cne (ConvertTo-Json -InputObject $After[$key] -Depth 100 -Compress))) { $changes.Add($key) }
    }
    return ,@($changes.ToArray())
}

function Get-NativeProcessIdentity {
    param([int] $ProcessId)
    $native = Get-CimInstance Win32_Process -Filter "ProcessId = $ProcessId" -ErrorAction Stop
    if (-not $native) { return $null }
    return @{ pid = [int]$native.ProcessId; parentPid = [int]$native.ParentProcessId; created = $native.CreationDate.ToUniversalTime().Ticks }
}

function Stop-OwnedProcess {
    param($Identity, [Collections.Generic.List[object]] $Owned)
    if (-not @($Owned | Where-Object { $_.pid -eq $Identity.pid -and $_.created -eq $Identity.created }).Count) { return $false }
    $current = Get-NativeProcessIdentity $Identity.pid
    if (-not $current -or $current.created -ne $Identity.created -or $current.parentPid -ne $Identity.parentPid) { return $false }
    $process = [Diagnostics.Process]::GetProcessById($Identity.pid)
    try {
        $null = $process.Handle
        $handleCreated = $process.StartTime.ToUniversalTime().Ticks
        # CIM truncates native creation timestamps to microseconds. Compare the retained handle within that precision.
        if ([Math]::Abs([long]$handleCreated - [long]$Identity.created) -gt 9) { throw 'Owned process handle creation time did not match the recorded identity.' }
        $process.Kill()
        if (-not $process.WaitForExit(5000)) { throw 'Owned process did not exit after termination.' }
        return $true
    } finally { $process.Dispose() }
}

function Invoke-IsolatedProcess {
    param([string] $Executable, [string] $Arguments, [hashtable] $Environment, [string] $WorkingDirectory, [int] $TimeoutSeconds = 60)
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $Executable; $start.Arguments = $Arguments; $start.WorkingDirectory = $WorkingDirectory
    $start.UseShellExecute = $false; $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true; $start.RedirectStandardError = $true
    foreach ($name in $Environment.Keys) {
        if ($null -eq $Environment[$name]) { $start.EnvironmentVariables.Remove($name) }
        else { $start.EnvironmentVariables[$name] = $Environment[$name] }
    }
    $owned = [Collections.Generic.List[object]]::new(); $cleanup = [Collections.Generic.List[object]]::new()
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $start
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $failure = ''; $code = -1; $stdout = $null; $stderr = $null
    try {
        if (-not $process.Start()) { throw 'Child launch failed.' }
        $identity = Get-NativeProcessIdentity $process.Id
        if (-not $identity) { throw 'Child exited before its native identity was established.' }
        $owned.Add($identity)
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
        do {
            $nativeProcesses = @(Get-CimInstance Win32_Process -ErrorAction Stop)
            $nativeByPid = @{}; $ownedByPid = @{}
            foreach ($native in $nativeProcesses) { $nativeByPid[[int]$native.ProcessId] = $native }
            foreach ($entry in $owned) { $ownedByPid[$entry.pid] = $entry }
            do {
                $added = $false
                foreach ($native in $nativeProcesses) {
                    $created = $native.CreationDate.ToUniversalTime().Ticks
                    $nativeId = [int]$native.ProcessId; $parentId = [int]$native.ParentProcessId
                    if ($ownedByPid.ContainsKey($nativeId) -and $ownedByPid[$nativeId].created -eq $created) { continue }
                    if ($nativeByPid.ContainsKey($parentId) -and $ownedByPid.ContainsKey($parentId) -and
                        $ownedByPid[$parentId].created -eq $nativeByPid[$parentId].CreationDate.ToUniversalTime().Ticks -and
                        $ownedByPid[$parentId].created -le $created) {
                        $descendant = @{ pid = $nativeId; parentPid = $parentId; created = $created }
                        $owned.Add($descendant); $ownedByPid[$nativeId] = $descendant; $added = $true
                    }
                }
            } while ($added)
            if ($process.HasExited) { break }
            if ([DateTime]::UtcNow -ge $deadline) { throw 'Child execution timed out.' }
            Start-Sleep -Milliseconds 50
        } while ($true)
        $code = $process.ExitCode
    } catch { $failure = $_.Exception.Message } finally {
        foreach ($entry in @($owned.ToArray()) | Sort-Object { $_.created } -Descending) {
            try {
                $stopped = Stop-OwnedProcess $entry $owned
                $cleanup.Add(@{ identity = $entry; stopped = $stopped; error = '' })
            } catch {
                $cleanup.Add(@{ identity = $entry; stopped = $false; error = $_.Exception.Message })
                $failure += ' Owned-process cleanup failed.'
            }
        }
    }
    try {
        $output = ''; $errors = ''
        if ($stdout -and $stderr) {
            if ($stdout.Wait(5000) -and $stderr.Wait(5000)) { $output = $stdout.Result; $errors = $stderr.Result }
            else { $failure += ' Child output streams did not close after owned-process cleanup.' }
        }
        return @{ exitCode = $code; stdout = $output; stderr = $errors; failure = $failure;
            processes = @($owned.ToArray()); cleanup = @($cleanup.ToArray()) }
    } finally { $process.Dispose() }
}

function Copy-HostProfile {
    param([string] $Source, [hashtable] $Environment)
    $profileItem = Get-Item -LiteralPath $Source -Force
    if (($profileItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Dedicated profile must not be a reparse point.' }
    foreach ($name in @('home', 'appdata')) {
        $sourcePath = Join-Path $Source $name
        if (-not (Test-Path -LiteralPath $sourcePath -PathType Container)) { throw "Dedicated profile directory is missing: $name" }
        $pending = [Collections.Generic.Queue[string]]::new(); $pending.Enqueue($sourcePath)
        while ($pending.Count) {
            $path = $pending.Dequeue(); $item = Get-Item -LiteralPath $path -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Dedicated profiles must not contain reparse points.' }
            if ($item.PSIsContainer) { foreach ($child in Get-ChildItem -LiteralPath $path -Force) { $pending.Enqueue($child.FullName) } }
        }
        $destination = if ($name -eq 'home') { $Environment.USERPROFILE } else { $Environment.APPDATA }
        foreach ($child in Get-ChildItem -LiteralPath $sourcePath -Force) { Copy-Item -LiteralPath $child.FullName -Destination $destination -Recurse -Force }
    }
}

function Invoke-IsolationSelfTest {
    param([string] $EvidenceRoot)
    $entry = Join-Path $PSScriptRoot 'Invoke-Verification.ps1'
    $results = [Collections.Generic.List[object]]::new()
    function Assert-Condition([bool] $Condition, [string] $Message) { if (-not $Condition) { throw $Message } }
    function Run-Check([string] $Name, [scriptblock] $Action) {
        try {
            & $Action | Out-Null
            $results.Add(@{ id = $Name; result = 'Passed'; reason = 'Assertions passed.'; evidencePaths = @('report.json') })
        } catch {
            $results.Add(@{ id = $Name; result = 'Failed'; reason = $_.Exception.Message; evidencePaths = @('report.json') })
        }
    }
    function Run-Entry([string] $Name, [hashtable] $Parameters = @{}, [int] $ExpectedExit = 0) {
        $path = Join-Path $EvidenceRoot $Name
        & powershell -NoProfile -NonInteractive -File $entry -Case isolation -Mode Fixture -EvidenceRoot $path @Parameters | Out-Null
        Assert-Condition ($LASTEXITCODE -eq $ExpectedExit) "$Name exit $LASTEXITCODE, expected $ExpectedExit"
        return (Get-Content -LiteralPath (Join-Path $path 'report.json') -Raw | ConvertFrom-Json)
    }
    Run-Check 'reruns-and-environment' {
        $saved = @{}
        foreach ($name in @('OPENCODE_CONFIG_CONTENT', 'OPENCODE_CONFIG', 'CCYNC_SKILLS', 'CLAUDE_CONFIG_DIR')) {
            $saved[$name] = [Environment]::GetEnvironmentVariable($name); [Environment]::SetEnvironmentVariable($name, 'hostile-override')
        }
        try {
            foreach ($iteration in 1..2) {
                $report = Run-Entry 'rerun'
                Assert-Condition ($report.result -eq 'Passed' -and $report.executionStarted) 'A rerun did not execute successfully.'
                Assert-Condition ($report.childEnvironment.HOME -eq $report.childEnvironment.USERPROFILE) 'HOME mismatch.'
                foreach ($name in $saved.Keys) { Assert-Condition ($null -eq $report.childEnvironment.$name) "Inherited override: $name" }
                Assert-Condition (-not (Test-Path -LiteralPath $report.childEnvironment.HOME)) 'Temporary profile remains.'
                Assert-Condition ($report.processes.Count -ge 1) 'Native child identity is missing.'
            }
        } finally { foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) } }
    }
    Run-Check 'missing-variable' {
        $report = Run-Entry 'missing' @{ OmitFixtureVariable = 'CCYNC_HOME' } 1
        Assert-Condition ($report.result -eq 'Failed' -and $report.executionStarted) 'Missing variable was not an executed failure.'
        $report = Run-Entry 'missing-host-variable' @{ OmitFixtureVariable = 'CODEX_HOME' } 1
        Assert-Condition ($report.result -eq 'Failed') 'Missing host variable passed.'
    }
    Run-Check 'persistent-change-without-repair' {
        $report = Run-Entry 'persistent' @{ FixtureCommand = '[IO.File]::WriteAllText((Join-Path (Get-Location) ''persistent.json''), ''changed'')' } 1
        Assert-Condition ($report.result -eq 'Failed') 'Unclassified change passed.'
        Assert-Condition ((Get-Content (Join-Path $EvidenceRoot 'persistent/fixture/persistent.json') -Raw) -eq 'changed') 'Fixture was repaired.'
    }
    $tablePath = Join-Path $EvidenceRoot 'classification.json'
    Write-EvidenceJson $tablePath @{ allowedVolatile = @('logs/**', 'sessions/*/log.txt'); excludedEphemeral = @('coordination.lock') }
    Run-Check 'volatile-and-wildcards' {
        $matcher = ConvertTo-ClassificationRegex 'logs/**/trace*.txt'
        Assert-Condition ($matcher.IsMatch('LOGS/trace1.txt') -and $matcher.IsMatch('logs/deep/trace2.txt')) 'Recursive matcher failed.'
        $matcher = ConvertTo-ClassificationRegex 'sessions/*/log.txt'
        Assert-Condition ($matcher.IsMatch('SESSIONS/abc-guid/LOG.TXT') -and -not $matcher.IsMatch('sessions/a/b/log.txt')) 'Segment matcher failed.'
        $report = Run-Entry 'volatile' @{ ClassificationTablePath = $tablePath; FixtureCommand = 'New-Item -ItemType Directory -Force logs | Out-Null; [IO.File]::WriteAllText((Join-Path (Get-Location) ''logs/trace.txt''), ''changed'')' }
        $diff = Get-Content (Join-Path $EvidenceRoot 'volatile/diff.json') -Raw | ConvertFrom-Json
        Assert-Condition ($report.allowedVolatile -contains 'logs/trace.txt') 'AllowedVolatile record missing.'
        Assert-Condition ($diff.unchanged -notcontains 'logs/trace.txt' -and $diff.allowedVolatileChanges -contains 'logs/trace.txt') 'Volatile changes were claimed unchanged.'
    }
    Run-Check 'protected-pattern-rejections' {
        foreach ($pattern in @('**', '.codex/**', '.claude.json', '.copilot/mcp-config.json', '.gemini/config/*',
            '.config/opencode/*', 'AppData/Roaming/opencode/*', '.codex/auth.json', '.ccync/**', 'AppData/*/ccyn*/**')) {
            $badTable = Join-Path $EvidenceRoot 'rejected-classification.json'
            Write-EvidenceJson $badTable @{ allowedVolatile = @($pattern) }
            $rejected = $false
            try { Get-ClassificationTable $badTable | Out-Null } catch { $rejected = $true }
            Assert-Condition $rejected "Protected pattern accepted: $pattern"
        }
        $report = Run-Entry 'classification-rejection' @{ ClassificationTablePath = $badTable } 2
        Assert-Condition (-not $report.executionStarted) 'Invalid classification launched a child.'
    }
    Run-Check 'managed-target-pattern-rejections' {
        $targets = @('.claude.json', '.codex/config.toml', '.copilot/mcp-config.json', '.gemini/config/mcp_config.json',
            'opencode/opencode.json', '.config/opencode/opencode.json', 'AppData/Roaming/opencode/opencode.json',
            'Library/Application Support/opencode/opencode.json')
        $badTable = Join-Path $EvidenceRoot 'managed-target-classification.json'
        foreach ($target in $targets) {
            $separator = $target.LastIndexOf('/')
            $patterns = @($target, $target.ToUpperInvariant())
            if ($separator -ge 0) {
                $directory = $target.Substring(0, $separator)
                $filename = $target.Substring($separator + 1)
                $patterns += @("$directory/*", "$directory/**/$filename", "$directory/**")
            } else { $patterns += '.claude*' }
            foreach ($pattern in $patterns) {
                Assert-Condition ((ConvertTo-ClassificationRegex $pattern).IsMatch($target)) "Test pattern misses target: $pattern"
                Write-EvidenceJson $badTable @{ allowedVolatile = @($pattern); excludedEphemeral = @() }
                $rejected = $false
                try { Get-ClassificationTable $badTable | Out-Null } catch { $rejected = $true }
                Assert-Condition $rejected "Managed target pattern accepted: $pattern"
            }
        }
        $patterns = @('opencode/opencode.json', 'opencode/*.json', 'opencode/**/opencode.json', 'OPENCODE/**')
        for ($index = 0; $index -lt $patterns.Count; $index++) {
            $pattern = $patterns[$index]
            Write-EvidenceJson $badTable @{ allowedVolatile = @($pattern); excludedEphemeral = @() }
            $report = Run-Entry "managed-target-rejection-$index" @{ ClassificationTablePath = $badTable } 2
            Assert-Condition ($report.result -eq 'Blocked' -and -not $report.executionStarted) "Managed target pattern launched a child: $pattern"
        }
        Write-EvidenceJson $badTable @{ allowedVolatile = @('opencode/logs/**'); excludedEphemeral = @() }
        $classification = Get-ClassificationTable $badTable
        Assert-Condition ((Get-PathClassification 'opencode/logs/run.txt' $classification) -eq 'allowedVolatile') 'Unprotected OpenCode logs were rejected.'
    }
    Run-Check 'claude-subtrees' {
        $path = Join-Path $EvidenceRoot 'claude/fixture'
        New-Item -ItemType Directory -Force $path | Out-Null
        Write-EvidenceJson (Join-Path $path '.claude.json') @{ mcpServers = @{ server = @{ command = 'first'; args = @('a', 'b') } }; projects = @{ project = @{ mcpServers = @{ local = @{ command = 'same' } }; other = 1 } }; other = 1 }
        $command = '$path = Join-Path (Get-Location) ''.claude.json''; $json = Get-Content $path -Raw | ConvertFrom-Json; $json.other = 2; $json | ConvertTo-Json -Depth 30 | Set-Content $path'
        $report = Run-Entry 'claude' @{ FixtureCommand = $command }
        Assert-Condition ($report.result -eq 'Passed') 'Other Claude fields failed.'
        $command = '$path = Join-Path (Get-Location) ''.claude.json''; $json = Get-Content $path -Raw | ConvertFrom-Json; $json.projects.project.mcpServers.local.command = ''changed''; $json | ConvertTo-Json -Depth 30 | Set-Content $path'
        $report = Run-Entry 'claude' @{ FixtureCommand = $command } 1
        Assert-Condition ($report.result -eq 'Failed') 'Project MCP change passed.'
    }
    Run-Check 'ephemeral-and-new-read-gaps' {
        $path = Join-Path $EvidenceRoot 'locks'
        New-Item -ItemType Directory -Force $path | Out-Null
        $lockPath = Join-Path $path 'coordination.lock'; [IO.File]::WriteAllText($lockPath, 'coordination')
        $stream = [IO.File]::Open($lockPath, 'Open', 'ReadWrite', 'None')
        try {
            $inventory = Get-FixtureInventory $path (Get-ClassificationTable $tablePath)
            Assert-Condition ($inventory.excludedEphemeral.Count -eq 1 -and $inventory.gaps.Count -eq 0) 'Exact unreadable coordination item was not excluded.'
            Assert-Condition ($inventory.excludedEphemeral[0].readError.Length -gt 0) 'Excluded item lost its read error.'
            $inventory = Get-FixtureInventory $path
            Assert-Condition ($inventory.gaps.Count -eq 1 -and $inventory.excludedEphemeral.Count -eq 0) 'New read gap became an exclusion.'
        } finally { $stream.Dispose() }
        foreach ($name in @('blocked-read', 'excluded-read')) {
            $fixture = Join-Path $EvidenceRoot "$name/fixture"
            New-Item -ItemType Directory -Force $fixture | Out-Null
            $file = Join-Path $fixture 'coordination.lock'; [IO.File]::WriteAllText($file, 'coordination')
            $stream = [IO.File]::Open($file, 'Open', 'ReadWrite', 'None')
            try {
                if ($name -eq 'blocked-read') {
                    $report = Run-Entry $name @{} 2
                    Assert-Condition (-not $report.executionStarted -and $report.gaps.Count -eq 1) 'Baseline read gap did not block execution.'
                } else {
                    $report = Run-Entry $name @{ ClassificationTablePath = $tablePath }
                    $diff = Get-Content (Join-Path $EvidenceRoot "$name/diff.json") -Raw | ConvertFrom-Json
                    Assert-Condition ($diff.unchanged -notcontains 'coordination.lock' -and $diff.excludedEphemeral -contains 'coordination.lock') 'Unreadable coordination file was claimed unchanged.'
                }
            } finally { $stream.Dispose() }
        }
        $report = Run-Entry 'new-read-gap' @{ FixtureCommand = '[IO.File]::WriteAllText((Join-Path (Get-Location) ''.claude.json''), ''{invalid'')' } 1
        Assert-Condition ($report.gaps.Count -eq 1 -and $report.executionStarted) 'Post-execution gap did not fail the run.'
    }
    Run-Check 'dedicated-profile-copy-and-cleanup' {
        $profile = Join-Path $EvidenceRoot 'dedicated-profile'
        New-Item -ItemType Directory -Force (Join-Path $profile 'home'), (Join-Path $profile 'appdata') | Out-Null
        [IO.File]::WriteAllText((Join-Path $profile 'home/profile-marker'), 'fixture-marker')
        $report = Run-Entry 'profile' @{ HostProfileRoot = $profile; FixtureCommand = 'if ((Get-Content (Join-Path $env:USERPROFILE ''profile-marker'') -Raw) -ne ''fixture-marker'') { throw ''Profile missing'' }' }
        Assert-Condition (-not (Test-Path $report.childEnvironment.USERPROFILE)) 'Copied profile remains.'
        Assert-Condition (Test-Path (Join-Path $profile 'home/profile-marker')) 'Dedicated source profile was removed.'
    }
    Run-Check 'reparse-classification-and-enumeration-gaps' {
        $root = Join-Path $EvidenceRoot 'reparse'
        $target = Join-Path $EvidenceRoot 'reparse-target'
        New-Item -ItemType Directory -Force $root, $target | Out-Null
        [IO.File]::WriteAllText((Join-Path $target 'outside.txt'), 'outside')
        $link = Join-Path $root 'link'
        if (-not (Test-Path -LiteralPath $link)) { New-Item -ItemType Junction -Path $link -Target $target | Out-Null }
        $inventory = Get-FixtureInventory $root
        Assert-Condition ($inventory.records['link'].type -eq 'reparse' -and -not $inventory.records.ContainsKey('link/outside.txt')) 'Unclassified reparse point was followed.'
        $table = @{ allowedVolatile = @(@{ pattern = 'link'; regex = (ConvertTo-ClassificationRegex 'link') }); excludedEphemeral = @() }
        $inventory = Get-FixtureInventory $root $table
        Assert-Condition ($inventory.allowedVolatile -contains 'link') 'Reparse point was not classified before handling.'
        [IO.File]::WriteAllText((Join-Path $root 'readable.txt'), 'readable')
        New-Item -ItemType Directory -Force (Join-Path $root 'unreadable') | Out-Null
        function Get-ChildItem {
            [CmdletBinding()] param([string] $LiteralPath, [switch] $Force)
            if ($LiteralPath.EndsWith('unreadable')) { throw 'Injected single-item enumeration failure.' }
            Microsoft.PowerShell.Management\Get-ChildItem -LiteralPath $LiteralPath -Force:$Force
        }
        $inventory = Get-FixtureInventory $root
        Assert-Condition ($inventory.gaps.Count -eq 1 -and $inventory.gaps[0].StartsWith('unreadable:')) 'Enumeration failure lost its item scope.'
        Assert-Condition ($inventory.records.ContainsKey('readable.txt')) 'Enumeration failure invalidated a readable sibling.'
    }
    Run-Check 'owned-descendants-and-unowned-negative' {
        $command = '$p = Start-Process powershell -WindowStyle Hidden -PassThru -ArgumentList ''-NoProfile -NonInteractive -Command Start-Sleep -Seconds 30''; Start-Sleep -Seconds 2'
        $report = Run-Entry 'owned-processes' @{ FixtureCommand = $command }
        Assert-Condition (@($report.cleanup | Where-Object { $_.stopped }).Count -ge 1) 'Owned descendant was not cleaned up.'
        $unowned = Get-NativeProcessIdentity $PID
        $empty = [Collections.Generic.List[object]]::new()
        Assert-Condition (-not (Stop-OwnedProcess $unowned $empty)) 'Unowned process was accepted for cleanup.'
        Assert-Condition ($null -ne (Get-NativeProcessIdentity $PID)) 'Unowned process did not survive.'
    }
    Run-Check 'background-change-outside-volatile' {
        $command = '$text = ''[IO.File]::WriteAllText(''''background.json'''', ''''changed'''')''; $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($text)); Start-Process powershell -WindowStyle Hidden -ArgumentList (''-NoProfile -NonInteractive -EncodedCommand '' + $encoded) -Wait; Start-Sleep -Seconds 1'
        $report = Run-Entry 'background-change' @{ FixtureCommand = $command; ClassificationTablePath = $tablePath } 1
        Assert-Condition ($report.result -eq 'Failed') 'Unattributed background change passed.'
    }
    Run-Check 'cargo-environment-and-targets' {
        $path = Join-Path $EvidenceRoot 'cargo'
        & powershell -NoProfile -NonInteractive -File $entry -Case cargo -Mode Fixture -EvidenceRoot $path -CargoCommand 'cargo test -p mcp --lib' | Out-Null
        Assert-Condition ($LASTEXITCODE -eq 0) "Cargo verification exit $LASTEXITCODE"
        $report = Get-Content (Join-Path $path 'report.json') -Raw | ConvertFrom-Json
        foreach ($name in @('CLAUDE_CONFIG_DIR', 'CODEX_HOME', 'COPILOT_HOME', 'OPENCODE_CONFIG_DIR', 'OPENCODE_CONFIG', 'OPENCODE_CONFIG_CONTENT', 'CCYNC_SKILLS')) {
            Assert-Condition ($null -eq $report.childEnvironment.$name) "Cargo override remains: $name"
        }
        foreach ($name in @('CARGO_HOME', 'RUSTUP_HOME', 'CARGO_TARGET_DIR', 'CCYNC_RECORDED_PYTHON')) { Assert-Condition (-not [string]::IsNullOrWhiteSpace($report.childEnvironment.$name)) "Cargo field missing: $name" }
        Assert-Condition (@($report.checks | Where-Object { $_.reason -match '^test result: ok\.' -and $_.result -eq 'Passed' }).Count -ge 1) 'No completed cargo test target recorded.'
        Assert-Condition (@($report.checks | Where-Object { $_.id -match '^unittests ' -and $_.result -eq 'Passed' }).Count -eq 1) 'Cargo target summary was not bound to its executable target.'
        Assert-Condition (@($report.checks | Where-Object { $_.id -match '^\d+ tests$' }).Count -eq 0) 'Test-count progress was mistaken for a cargo target.'
    }
    Run-Check 'unsupported-case-and-native-blocked' {
        foreach ($selection in @(@('unsupported', 'Fixture'), @('isolation', 'Native'))) {
            $path = Join-Path $EvidenceRoot ($selection[0] + $selection[1])
            & powershell -NoProfile -NonInteractive -File $entry -Case $selection[0] -Mode $selection[1] -EvidenceRoot $path | Out-Null
            Assert-Condition ($LASTEXITCODE -eq 2) 'Unsupported entry did not exit 2.'
            $report = Get-Content (Join-Path $path 'report.json') -Raw | ConvertFrom-Json
            Assert-Condition ($report.result -eq 'Blocked' -and -not $report.executionStarted) 'Unsupported entry was not Blocked before execution.'
        }
    }
    return $results.ToArray()
}
