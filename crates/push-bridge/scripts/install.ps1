<#
.SYNOPSIS
Starts push-agent-bridge at logon: registers a Scheduled Task for the current user.

.DESCRIPTION
The task runs `<Exe> --dir "<Dir>" run` when the current user logs on, as that user,
without elevation, restarts it every minute after a failure (up to 999 times) and has
no time limit. Run `push-agent-bridge --dir <Dir> keygen` and `enroll --code <code>`
first. The bridge is a console program: the task runs it hidden, through
`%SystemRoot%\System32\conhost.exe --headless "<Exe>" --dir "<Dir>" run`, so no console
window shows; the bridge writes to its log. Where conhost.exe is missing the task runs
<Exe> directly (the script says so), and its console window shows while it runs
(closing the window stops the bridge until the task restarts it).

Works with Windows PowerShell 5.1 and PowerShell 7.

.PARAMETER Exe
The path to push-agent-bridge.exe.

.PARAMETER Dir
The bridge's folder with config.json; by default
%LOCALAPPDATA%\push-messenger\coda-bridge.

.PARAMETER TaskName
The task's name; one per agent instance. By default push-agent-bridge.

.PARAMETER Force
Replaces a task of the same name.

.PARAMETER NoStart
Registers the task without starting it now.

.EXAMPLE
.\install.ps1 -Exe C:\Tools\push-agent-bridge.exe
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Exe,
    [string]$Dir = (Join-Path $env:LOCALAPPDATA 'push-messenger\coda-bridge'),
    [string]$TaskName = 'push-agent-bridge',
    [switch]$Force,
    [switch]$NoStart
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
    Write-Error "No program at $Exe."
    exit 1
}
$Exe = (Resolve-Path -LiteralPath $Exe).ProviderPath

$Dir = [System.IO.Path]::GetFullPath($Dir)
# A trailing backslash would escape the closing quote of --dir "<Dir>".
if ($Dir.Length -gt 3) {
    $Dir = $Dir.TrimEnd('\')
}
$config = Join-Path $Dir 'config.json'
if (-not (Test-Path -LiteralPath $config -PathType Leaf)) {
    Write-Error "No configuration at $config. Create it first (see docs/agent-bridge.md)."
    exit 1
}

$existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($null -ne $existing -and -not $Force) {
    Write-Error "A task named $TaskName exists already. Use -Force to replace it."
    exit 1
}

$arguments = '--dir "{0}" run' -f $Dir
# conhost --headless gives the console program its console without a window.
$conhost = Join-Path $env:SystemRoot 'System32\conhost.exe'
if (Test-Path -LiteralPath $conhost -PathType Leaf) {
    $action = New-ScheduledTaskAction -Execute $conhost -Argument ('--headless "{0}" {1}' -f $Exe, $arguments) -WorkingDirectory $Dir
} else {
    Write-Host "No $conhost; the task runs $Exe directly, so its console window shows."
    $action = New-ScheduledTaskAction -Execute $Exe -Argument $arguments -WorkingDirectory $Dir
}
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet `
    -RestartCount 999 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -MultipleInstances IgnoreNew

Register-ScheduledTask `
    -TaskName $TaskName `
    -Description 'push-agent-bridge: reaches a local coding agent from push-messenger.' `
    -Action $action `
    -Trigger $trigger `
    -Principal $principal `
    -Settings $settings `
    -Force:$Force | Out-Null
Write-Host "Registered the task $TaskName."

if (-not $NoStart) {
    Start-ScheduledTask -TaskName $TaskName
    Write-Host "Started it. The log is $(Join-Path $Dir 'logs\bridge.log')."
}
