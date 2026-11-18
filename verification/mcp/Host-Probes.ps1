function Invoke-HostInventoryBootstrap {
    param([string] $Root, [string] $EvidenceRoot, [int] $TimeoutSeconds)
    $results = [ordered]@{}
    $shell = (Get-Command powershell -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    foreach ($name in @('claude', 'codex', 'copilot', 'opencode', 'agy')) {
        $environment = Get-IsolatedEnvironment -Root (Join-Path $Root $name) -Cargo $false
        foreach ($key in @('USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'CCYNC_HOME')) {
            New-Item -ItemType Directory -Force -Path $environment[$key] | Out-Null
        }
        $command = Get-Command $name -CommandType Application, ExternalScript -ErrorAction SilentlyContinue | Select-Object -First 1
        $before = @{ home = (Get-FixtureInventory $environment.USERPROFILE); appdata = (Get-FixtureInventory $environment.APPDATA) }
        $child = $null; $gap = ''
        if ($before.home.gaps.Count -or $before.appdata.gaps.Count) { $gap = 'Initial isolated inventory has read gaps.' }
        elseif (-not $command) { $gap = 'Host executable unavailable.' }
        else {
            $executable = $command.Source.Replace("'", "''")
            $script = "Start-Sleep -Milliseconds 400; & '$executable' --version; exit `$LASTEXITCODE"
            $arguments = '-NoProfile -NonInteractive -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
            try { $child = Invoke-IsolatedProcess $shell $arguments $environment $environment.USERPROFILE $TimeoutSeconds }
            catch { $gap = 'Isolated version command could not start.' }
        }
        $after = @{ home = (Get-FixtureInventory $environment.USERPROFILE); appdata = (Get-FixtureInventory $environment.APPDATA) }
        $changes = @{ home = (Compare-FixtureInventory $before.home.records $after.home.records); appdata = (Compare-FixtureInventory $before.appdata.records $after.appdata.records) }
        $version = $null; $sha256 = $null; $listing = $null
        if ($child -and $child.exitCode -eq 0 -and -not $child.failure) {
            $version = $child.stdout
            if ($command) {
                try { $sha256 = Get-FileDigest -Path $command.Source }
                catch { $gap = 'Executable hash unavailable.' }
            }
        }
        if ($command -and $name -in @('claude', 'codex', 'opencode')) {
            if ($name -eq 'codex') { New-Item -ItemType Directory -Force -Path $environment.CODEX_HOME | Out-Null }
            $script = "Start-Sleep -Milliseconds 400; & '$executable' mcp list; exit `$LASTEXITCODE"
            $arguments = '-NoProfile -NonInteractive -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
            try {
                $listChild = Invoke-IsolatedProcess $shell $arguments $environment $environment.USERPROFILE $TimeoutSeconds
                $empty = $listChild.stdout.Trim() -match '^(No MCP servers configured(?:\. Use `claude mcp add` to add a server\.)?|No MCP servers configured yet\..*)$'
                $listing = @{ command = "$name mcp list"; exitCode = $listChild.exitCode; failure = $listChild.failure;
                    zeroServers = $empty; processes = $listChild.processes; cleanup = $listChild.cleanup }
            } catch { $listing = @{ command = "$name mcp list"; failure = 'Isolated listing could not start.' } }
        }
        $results[$name] = @{ command = $(if ($command) { $command.Source } else { $null }); commandClass = 'version'; version = $version;
            sha256 = $sha256; listing = $listing;
            exitCode = $(if ($child) { $child.exitCode } else { $null }); failure = $(if ($gap) { $gap } elseif ($child) { $child.failure } else { '' });
            processes = $(if ($child) { $child.processes } else { @() }); cleanup = $(if ($child) { $child.cleanup } else { @() });
            before = $before; after = $after; changes = $changes }
    }
    Write-EvidenceJson (Join-Path $EvidenceRoot 'bootstrap.json') $results
    return $results
}

function Get-HostCapabilities {
    param([string] $Mode, [string] $FixturePath, $Table, $Bootstrap)
    $names = @('claude', 'codex', 'copilot', 'opencode', 'agy')
    $commands = @{ claude = 'claude'; codex = 'codex'; copilot = 'copilot'; opencode = 'opencode'; agy = 'agy' }
    $fixture = $null
    if ($FixturePath) {
        if ($Mode -ne 'Fixture') { throw 'Capability fixtures are accepted only in Fixture mode.' }
        $fixture = Get-Content -LiteralPath $FixturePath -Raw | ConvertFrom-Json
    }
    $hosts = @(
        foreach ($name in $names) {
            $gaps = [Collections.Generic.List[string]]::new()
            $identity = $null
            $facts = $null
            if ($Mode -eq 'Fixture') {
                if ($fixture) {
                    $property = $fixture.PSObject.Properties[$name]
                    if ($property) { $facts = $property.Value }
                }
                else {
                    $facts = [pscustomobject]@{ isolation = $true; login = $true; headless = $true; usage = 0; allowance = 4; attemptCost = 1; attempts = 0; attemptCap = 1 }
                }
                if (-not $facts) { $gaps.Add('fixture host facts missing') }
                else {
                    foreach ($key in @('isolation', 'login', 'headless')) {
                        if (-not $facts.PSObject.Properties[$key] -or $facts.$key -ne $true) { $gaps.Add("$key unavailable") }
                    }
                    $numeric = $true
                    foreach ($key in @('usage', 'allowance', 'attemptCost', 'attempts', 'attemptCap')) {
                        $value = 0.0
                        if (-not $facts.PSObject.Properties[$key] -or -not [double]::TryParse([string]$facts.$key, [ref]$value) -or [double]::IsNaN($value) -or [double]::IsInfinity($value) -or $value -lt 0) { $numeric = $false }
                    }
                    if (-not $numeric) { $gaps.Add('finite usage and attempt bounds unavailable') }
                    elseif ([double]$facts.allowance -le 0 -or [double]$facts.attemptCap -le 0 -or [double]$facts.attempts -ge [double]$facts.attemptCap -or ([double]$facts.usage + [double]$facts.attemptCost) -gt ([double]$facts.allowance / 4)) { $gaps.Add('usage or attempt budget exhausted') }
                }
            } else {
                foreach ($gap in $Table.hosts.$name.gaps) { $gaps.Add([string]$gap) }
                $command = Get-Command $commands[$name] -CommandType Application, ExternalScript -ErrorAction SilentlyContinue | Select-Object -First 1
                if ($command) {
                    try {
                        $stream = [IO.File]::OpenRead($command.Source)
                        $algorithm = [Security.Cryptography.SHA256]::Create()
                        try { $hash = [BitConverter]::ToString($algorithm.ComputeHash($stream)).Replace('-', '') }
                        finally { $stream.Dispose(); $algorithm.Dispose() }
                        $identity = @{ path = $command.Source; sha256 = $hash; version = $(if ($Bootstrap -and $Bootstrap[$name].version) { $Bootstrap[$name].version } else { 'unverified' }) }
                    } catch { $gaps.Add('executable hash unavailable') }
                }
                else { $gaps.Add('host executable unavailable') }
                foreach ($gap in @('version and install identity unverified', 'isolation unverified', 'dedicated profile login unverified', 'zero-model trigger unverified', 'headless and desktop capability unverified', 'finite usage and attempt bounds unverified', 'native stop and event sources unverified', 'actual MCP load path unverified', 'classification inventory and review missing')) { $gaps.Add($gap) }
                if ($name -eq 'opencode') { $gaps.Add('default load path with XDG_CONFIG_HOME and overrides cleared unverified') }
            }
            [ordered]@{
                host = $name; result = $(if ($gaps.Count) { 'Blocked' } else { 'Passed' })
                reason = $(if ($gaps.Count) { 'Prerequisites are missing or unverified.' } else { 'Synthetic fixture facts satisfy the budget gate.' })
                gaps = @($gaps.ToArray()); install = $identity; source = $(if ($Mode -eq 'Fixture') { 'synthetic fixture' } else { 'read-only executable discovery' })
                stopSettings = $Table.hosts.$name.stopSettings; loadConfirmation = $Table.hosts.$name.loadConfirmation
                actualLoadPath = $Table.hosts.$name.actualLoadPath; managedPathMatches = $Table.hosts.$name.loadedPathEqualsLiveTarget
                credentialLocation = $Table.hosts.$name.credentialLocation
            }
        }
    )
    return [ordered]@{ command_class = $Table.command_class; cwd_class = $Table.cwd_class; entry_version = $Table.entry_version; mode = $Mode; hosts = $hosts }
}
