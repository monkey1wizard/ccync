[CmdletBinding()]
param(
    [Parameter(Mandatory)] [ValidateSet('normal', 'allocconsole')] [string] $Control,
    [Parameter(Mandatory)] [string] $ResultPath,
    [Parameter(Mandatory)] [string] $ArgumentValue,
    [Parameter(Mandatory)] [string] $UnicodeArgument
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$utf8 = [Text.UTF8Encoding]::new($false)
$record = [ordered]@{
    control = $Control; pid = $PID; arguments = @($ArgumentValue, $UnicodeArgument)
    unicodeArgument = $UnicodeArgument; cwd = [Environment]::CurrentDirectory
    environment = [Environment]::GetEnvironmentVariable('CCYNC_CALIBRATION_VALUE', 'Process')
    stdin = $null; stdinClosed = $false; allocConsoleSucceeded = $false
    consoleWindow = $null; consoleOwnerPid = $null; failedStep = $null; error = $null
}
$controlExit = 0; $step = 'StandardStreams'
try {
    if ($Control -eq 'normal') {
        $record.stdin = [Console]::ReadLine()
        [Console]::WriteLine($record.stdin)
        [Console]::Error.WriteLine('ccync-calibration-stderr')
        $record.stdinClosed = ($null -eq [Console]::ReadLine())
    } else {
        $step = 'CompileConsoleControl'
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CalibrationConsole
{
    [DllImport("kernel32.dll", SetLastError = true)] public static extern bool AllocConsole();
    [DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
}
'@
        $step = 'AllocConsole'
        if (-not [CalibrationConsole]::AllocConsole()) {
            throw "AllocConsole failed with Win32 error $([Runtime.InteropServices.Marshal]::GetLastWin32Error())."
        }
        $record.allocConsoleSucceeded = $true
        $step = 'ConsoleIdentity'
        $window = [IntPtr]::Zero
        for ($attempt = 0; $attempt -lt 100 -and $window -eq [IntPtr]::Zero; $attempt++) {
            $window = [CalibrationConsole]::GetConsoleWindow()
            if ($window -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 10 }
        }
        $consolePid = [uint32]0
        if ($window -ne [IntPtr]::Zero -and [CalibrationConsole]::GetWindowThreadProcessId($window, [ref]$consolePid) -ne 0) {
            $record.consoleWindow = $window.ToInt64()
            $record.consoleOwnerPid = $consolePid
        } else {
            $record.error = 'AllocConsole succeeded but did not expose a console window identity.'
        }
    }
} catch {
    $record.failedStep = $step; $record.error = $_.Exception.Message; $controlExit = 1
}
[IO.File]::WriteAllText($ResultPath, (ConvertTo-Json -InputObject $record -Depth 10), $utf8)
if ($controlExit -eq 0) { Start-Sleep -Milliseconds 1200 }
exit $controlExit
