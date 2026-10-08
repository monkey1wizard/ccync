[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$bash = 'C:\Program Files\Git\bin\bash.exe'
if (-not (Test-Path -LiteralPath $bash)) {
  throw "Git Bash was not found at $bash"
}

$tempDir = Join-Path $repoRoot (".ccync-release-test-" + [guid]::NewGuid())
$resolvedRepoRoot = [IO.Path]::GetFullPath($repoRoot).TrimEnd('\') + '\'
$resolvedTempDir = [IO.Path]::GetFullPath($tempDir)
if (-not $resolvedTempDir.StartsWith($resolvedRepoRoot, [StringComparison]::OrdinalIgnoreCase)) {
  throw "Release test temp directory escapes the repository: $resolvedTempDir"
}
New-Item -ItemType Directory -Path $tempDir | Out-Null
try {
  $drive = $tempDir.Substring(0, 1).ToLowerInvariant()
  $rest = $tempDir.Substring(2).TrimStart('\').Replace('\', '/')
  $env:CCYNC_TEST_TMP = "/$drive/$rest"
  & $bash -l (Join-Path $PSScriptRoot 'test-release-artifacts.sh')
  if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
finally {
  Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
  Remove-Item Env:CCYNC_TEST_TMP -ErrorAction SilentlyContinue
}
