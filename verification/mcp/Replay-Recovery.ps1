function Invoke-RecoveryReplay {
    param([string] $Mode, [string] $EvidenceRoot, [string] $RepositoryRoot, [int] $TimeoutSeconds)
    $result = @{ result = 'Blocked'; reason = ''; executionStarted = $false; checks = @(); gaps = @(); missingPrerequisites = @(); evidencePaths = @() }
    if ($Mode -ne 'Fixture') {
        $result.reason = 'Recovery replay is available only for synthetic fixtures.'
        $result.missingPrerequisites = @('Separately authorized real prior records and live recovery')
        return $result
    }
    $binary = Join-Path $RepositoryRoot 'target/debug/ccync.exe'
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        $result.reason = 'Build target/debug/ccync.exe before running the fixture replay.'
        $result.missingPrerequisites = @($result.reason)
        return $result
    }
    $run = Join-Path $EvidenceRoot ('replay-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $run | Out-Null
    $result.evidencePaths = @([IO.Path]::GetFileName($run))
    $result.executionStarted = $true
    $checks = [Collections.Generic.List[object]]::new()
    try {
        foreach ($language in @('en', 'zh-Hant')) {
            $root = Join-Path $run $language
            $fixtureHome = Join-Path $root 'home'
            $envs = Get-IsolatedEnvironment -Root $root -Cargo $false
            $envs.CCYNC_HOME = Join-Path $fixtureHome '.ccync'
            $envs.APPDATA = Join-Path $fixtureHome 'AppData/Roaming'
            $envs.LOCALAPPDATA = Join-Path $fixtureHome 'AppData/Local'
            $envs.PATH = Join-Path $root 'empty-bin'
            $envs.CCYNC_ASSUME_YES = $null; $envs.CCYNC_TEST_WINDOWS_STDIO = $null
            foreach ($path in @($fixtureHome, $envs.CCYNC_HOME, $envs.PATH, (Join-Path $fixtureHome '.codex'))) { New-Item -ItemType Directory -Force -Path $path | Out-Null }
            [IO.File]::WriteAllText((Join-Path $envs.CCYNC_HOME 'config.json'), '{"selectedProjectionTargets":["codex"]}')
            [IO.File]::WriteAllText((Join-Path $envs.CCYNC_HOME 'plugins.json'), '{"plugins":[]}')
            [IO.File]::WriteAllText((Join-Path $envs.CCYNC_HOME 'mcp.json'), '{"mcpServers":{"alpha":{"url":"https://example.com/alpha","hosts":["codex","claude"]},"beta":{"url":"https://example.com/beta","hosts":["codex","claude"]}}}')
            $shell = (Get-Command powershell -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
            $steps = [Collections.Generic.List[object]]::new()
            function Invoke-ReplayStep([string] $Name, [string[]] $Arguments) {
                $quoted = @($Arguments | ForEach-Object { "'" + $_.Replace("'", "''") + "'" }) -join ','
                $code = 'Start-Sleep -Milliseconds 400; & ' + "'" + $binary.Replace("'", "''") + "' @($quoted); exit `$LASTEXITCODE"
                $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($code))
                $child = Invoke-IsolatedProcess $shell ('-NoProfile -NonInteractive -EncodedCommand ' + $encoded) $envs $root $TimeoutSeconds
                $entry = @{ name = $Name; argv = $Arguments; exitCode = $child.exitCode; stdout = $child.stdout; stderr = $child.stderr; failure = $child.failure }
                $steps.Add($entry)
                Write-EvidenceJson (Join-Path $root 'steps.json') @($steps.ToArray())
                if ($child.failure) { throw "$Name`: $($child.failure)" }
                return $child.exitCode
            }
            function Save-ReplaySnapshot([string] $Name) {
                $destination = Join-Path $root $Name
                New-Item -ItemType Directory -Path $destination | Out-Null
                $files = @('.claude.json', '.codex/config.toml', '.ccync/mcp.json', 'AppData/Roaming/ccync/mcp.json', 'AppData/Local/ccync/state/build/mcp/projected-state.json')
                $inventory = @{}
                foreach ($relative in $files) {
                    $source = Join-Path $fixtureHome $relative
                    if (Test-Path -LiteralPath $source -PathType Leaf) {
                        $target = Join-Path $destination $relative
                        New-Item -ItemType Directory -Force -Path ([IO.Path]::GetDirectoryName($target)) | Out-Null
                        [IO.File]::WriteAllBytes($target, [IO.File]::ReadAllBytes($source))
                        $inventory[$relative] = Get-FileDigest $source
                    } else { $inventory[$relative] = $null }
                }
                Write-EvidenceJson (Join-Path $destination 'inventory.json') $inventory
                return $inventory
            }
            if ((Invoke-ReplayStep 'initial-sync' @('sync', '--yes')) -ne 0) { throw 'Initial fixture publication failed.' }
            $prior = Save-ReplaySnapshot 'prior'
            $statePath = Join-Path $fixtureHome 'AppData/Local/ccync/state/build/mcp/projected-state.json'
            if (-not $prior['.claude.json'] -or -not $prior['.codex/config.toml'] -or -not (Test-Path $statePath)) { throw 'Prior publication lacks host bytes or ownership.' }
            Write-EvidenceJson (Join-Path $root 'prior-records.json') @(
                @{ user = 'fixture'; source = 'user'; server = 'alpha'; host = 'claude'; record = 'prior/.claude.json'; sha256 = $prior['.claude.json']; classification = 'confirmed missing after deletion' },
                @{ user = 'fixture'; source = 'user'; server = 'alpha'; host = 'codex'; record = 'prior/.codex/config.toml'; sha256 = $prior['.codex/config.toml']; classification = 'confirmed missing after deletion' },
                @{ user = 'fixture'; source = 'user'; server = 'beta'; host = 'claude'; record = 'prior/.claude.json'; sha256 = $prior['.claude.json']; classification = 'current choice: unselected' },
                @{ user = 'fixture'; source = 'user'; server = 'beta'; host = 'codex'; record = 'prior/.codex/config.toml'; sha256 = $prior['.codex/config.toml']; classification = 'current choice: unselected' },
                @{ user = 'fixture'; source = 'unknown'; server = 'unknown'; host = 'codex'; record = 'synthetic count only'; classification = 'uncertain history' }
            )
            [IO.File]::WriteAllText((Join-Path $fixtureHome '.claude.json'), '{"mcpServers":{}}')
            [IO.File]::WriteAllText((Join-Path $fixtureHome '.codex/config.toml'), '')
            if ((Invoke-ReplayStep 'detect-holds' @('sync', '--yes')) -eq 0) { throw 'Missing fixture entries did not produce holds.' }
            $before = Save-ReplaySnapshot 'before'
            if ((Invoke-ReplayStep 'preview' @('sync', '--dry-run')) -eq 0) { throw 'Held items were not reported by preview.' }
            $preview = Save-ReplaySnapshot 'preview'
            foreach ($path in $before.Keys) { if ($before[$path] -cne $preview[$path]) { throw "Preview changed $path." } }
            foreach ($hostName in @('claude', 'codex')) {
                if ((Invoke-ReplayStep "preview-$hostName" @('sync', '--reselect-mcp', 'user', 'alpha', $hostName, '--dry-run')) -ne 0) { throw "Reselection preview failed for $hostName." }
                if ((Invoke-ReplayStep "reselect-$hostName" @('sync', '--reselect-mcp', 'user', 'alpha', $hostName)) -ne 0) { throw "Reselection failed for $hostName." }
            }
            $after = Save-ReplaySnapshot 'after'
            $state = Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json
            foreach ($hostName in @('claude', 'codex')) {
                if (@($state.holds | Where-Object { $_.identity.server -eq 'beta' -and $_.host -eq $hostName }).Count -ne 1) { throw "Unselected hold missing for $hostName." }
                if (@($state.publications | Where-Object { $_.identity.source -eq 'user' -and $_.identity.server -eq 'alpha' -and $_.host -eq $hostName }).Count -ne 1) { throw "Ownership record missing for $hostName." }
                if (@($state.holds | Where-Object { $_.identity.server -eq 'alpha' -and $_.host -eq $hostName }).Count -ne 0) { throw "Selected hold remains for $hostName." }
            }
            $claude = Get-Content -LiteralPath (Join-Path $fixtureHome '.claude.json') -Raw
            $codex = Get-Content -LiteralPath (Join-Path $fixtureHome '.codex/config.toml') -Raw
            if ($claude -notmatch 'alpha' -or $codex -notmatch 'alpha' -or $claude -match 'beta' -or $codex -match 'beta') { throw 'Host entries do not match the selected items.' }
            $null = Invoke-ReplayStep 'second-sync' @('sync', '--yes')
            $second = Save-ReplaySnapshot 'second'
            foreach ($path in @('.claude.json', '.codex/config.toml', '.ccync/mcp.json')) {
                if ($after[$path] -cne $second[$path]) { throw "Second sync changed $path." }
            }
            if ((Invoke-ReplayStep 'not-held' @('sync', '--reselect-mcp', 'user', 'alpha', 'codex')) -eq 0) { throw 'Reselecting an item without a hold succeeded.' }
            if ((Invoke-ReplayStep 'unknown-host' @('sync', '--reselect-mcp', 'user', 'beta', 'unknown')) -eq 0) { throw 'An unknown host was accepted.' }
            $negative = Save-ReplaySnapshot 'negative'
            foreach ($path in $second.Keys) { if ($second[$path] -cne $negative[$path]) { throw "Invalid reselection changed $path." } }
            Write-EvidenceJson (Join-Path $root 'diff.json') @{ before = $before; after = $after; second = $second }
            $hostPath = Join-Path $fixtureHome '.claude.json'
            $hostConfig = Get-Content -LiteralPath $hostPath -Raw | ConvertFrom-Json
            $hostConfig.mcpServers | Add-Member -NotePropertyName 'gamma' -NotePropertyValue @{ url = 'https://foreign.example.com' }
            Write-EvidenceJson $hostPath $hostConfig
            $sourcePath = Join-Path $fixtureHome 'AppData/Roaming/ccync/mcp.json'
            $sourceConfig = Get-Content -LiteralPath $sourcePath -Raw | ConvertFrom-Json
            $sourceConfig.mcpServers | Add-Member -NotePropertyName 'gamma' -NotePropertyValue @{ url = 'https://example.com/gamma'; hosts = @('claude') }
            Write-EvidenceJson $sourcePath $sourceConfig
            $conflictBefore = Save-ReplaySnapshot 'conflict-before'
            $conflictExit = Invoke-ReplayStep 'foreign-conflict' @('sync', '--yes')
            $conflictAfter = Save-ReplaySnapshot 'conflict-after'
            if ($conflictExit -eq 0 -or $steps[$steps.Count - 1].stdout -notmatch 'gamma / claude: Blocked' -or $conflictBefore['.claude.json'] -cne $conflictAfter['.claude.json'] -or $conflictBefore['AppData/Roaming/ccync/mcp.json'] -cne $conflictAfter['AppData/Roaming/ccync/mcp.json']) { throw 'Foreign conflict was not refused and preserved.' }
            Write-EvidenceJson (Join-Path $root 'conflict-diff.json') @{ before = $conflictBefore; after = $conflictAfter; exitCode = $conflictExit }
            Write-EvidenceJson (Join-Path $root 'offline-checklist.json') @{ source = 'user'; selected = @('alpha/claude', 'alpha/codex'); unselected = 'beta remains held'; conflict = 'foreign gamma preserved'; version = 'unverified'; connection = 'unverified'; scope = 'synthetic fixture only' }
            $checks.Add(@{ id = $language; result = 'Passed'; reason = 'Fixture replay completed.'; evidencePaths = @([IO.Path]::GetFileName($run) + '/' + $language) })
        }
        $result.result = 'Passed'; $result.reason = 'Synthetic replay completed in both languages. No real history was recovered.'
    } catch {
        $result.result = 'Failed'; $result.reason = $_.Exception.Message
        $result.gaps = @($_.Exception.Message)
    }
    $result.checks = @($checks.ToArray())
    return $result
}
