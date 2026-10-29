<#
.SYNOPSIS
Removes the push-agent-bridge logon task, and optionally the agent's key and state.

.DESCRIPTION
Stops and unregisters the Scheduled Task. -DeleteKey also deletes the agent's key from
Windows key storage (both the TPM and the software provider) or, with
"storage": "software-file", the key file; the agent then has to be enrolled again with
a new key. -DeleteState removes the bridge's folder (configuration, state and logs).
Both ask first unless -Force is given.

Works with Windows PowerShell 5.1 and PowerShell 7.

.PARAMETER TaskName
The task's name. By default push-agent-bridge.

.PARAMETER Dir
The bridge's folder; by default %LOCALAPPDATA%\push-messenger\coda-bridge.

.PARAMETER KeyName
The key's name; by default the key.name in config.json, else
push-messenger-coda-bridge.

.PARAMETER DeleteKey
Deletes the agent's key.

.PARAMETER DeleteState
Deletes the bridge's folder.

.PARAMETER Force
Does not ask before deleting.

.EXAMPLE
.\uninstall.ps1 -DeleteKey -DeleteState
#>
[CmdletBinding()]
param(
    [string]$TaskName = 'push-agent-bridge',
    [string]$Dir = (Join-Path $env:LOCALAPPDATA 'push-messenger\coda-bridge'),
    [string]$KeyName,
    [switch]$DeleteKey,
    [switch]$DeleteState,
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Confirm-Step([string]$question) {
    if ($Force) {
        return $true
    }
    $answer = Read-Host "$question [y/N]"
    return $answer -match '^(y|yes)$'
}

$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($null -ne $task) {
    Stop-ScheduledTask -TaskName $TaskName
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-Host "Removed the task $TaskName."
} else {
    Write-Host "There is no task named $TaskName."
}

$Dir = [System.IO.Path]::GetFullPath($Dir)
$storage = 'windows'
$configPath = Join-Path $Dir 'config.json'
if (Test-Path -LiteralPath $configPath -PathType Leaf) {
    $config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
    if ($null -ne $config.PSObject.Properties['key']) {
        $key = $config.key
        if (-not $KeyName -and $null -ne $key.PSObject.Properties['name']) {
            $KeyName = $key.name
        }
        if ($null -ne $key.PSObject.Properties['storage']) {
            $storage = $key.storage
        }
    }
}
if (-not $KeyName) {
    $KeyName = 'push-messenger-coda-bridge'
}

if ($DeleteKey) {
    if ($storage -eq 'software-file') {
        $file = Join-Path $Dir 'agent-key.p8'
        if (-not (Test-Path -LiteralPath $file)) {
            Write-Host "There is no key file $file."
        } elseif (Confirm-Step "Delete the key file $file?") {
            Remove-Item -LiteralPath $file
            Write-Host "Deleted $file."
        }
    } elseif (Confirm-Step "Delete the key $KeyName from Windows key storage? The agent must then be enrolled again.") {
        $providers = @(
            (New-Object System.Security.Cryptography.CngProvider 'Microsoft Platform Crypto Provider'),
            [System.Security.Cryptography.CngProvider]::MicrosoftSoftwareKeyStorageProvider
        )
        foreach ($provider in $providers) {
            try {
                if ([System.Security.Cryptography.CngKey]::Exists($KeyName, $provider)) {
                    $cngKey = [System.Security.Cryptography.CngKey]::Open($KeyName, $provider)
                    $cngKey.Delete()
                    Write-Host "Deleted the key $KeyName from $($provider.Provider)."
                }
            } catch {
                Write-Warning "$($provider.Provider): $($_.Exception.Message)"
            }
        }
    }
}

if ($DeleteState) {
    if (-not (Test-Path -LiteralPath $Dir)) {
        Write-Host "There is no folder $Dir."
    } elseif (Confirm-Step "Delete $Dir (configuration, state and logs)?") {
        Remove-Item -LiteralPath $Dir -Recurse
        Write-Host "Deleted $Dir."
    }
}
