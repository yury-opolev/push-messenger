# Exercises up.ps1's real pre-build path without a cluster or any writes.
$ErrorActionPreference = 'Stop'
$source = Get-Content (Join-Path $PSScriptRoot 'up.ps1') -Raw
$prefix = $source.Substring(0, $source.IndexOf("# Build into the cluster's own Docker daemon"))
$prefix = $prefix.Replace('$PSScriptRoot', "'$($PSScriptRoot.Replace("'", "''"))'")
$script:failureCount = 0
foreach ($case in 'missing-messaging-key', 'legacy-owner', 'query-error', 'prepared', 'fresh') {
    $script:case = $case
    $script:roleQueries = 0
    function minikube {
        $global:LASTEXITCODE = 0
        'Running/Running/Running'
    }
    function kubectl {
        $global:LASTEXITCODE = 0
        $command = $args -join ' '
        if ($command -notmatch '^--context fixture ') { throw 'missing explicit context' }
        switch -Regex ($command) {
            'get job' { return }
            'get secret .* -o name' { if ($script:case -ne 'fresh') { 'secret/push-secrets' }; return }
            'get secret .*jsonpath' {
                if ($command -match 'messaging-db-password' -and $script:case -eq 'missing-messaging-key') { return }
                'synthetic'; return
            }
            'get statefulset' { if ($script:case -ne 'fresh') { 'statefulset.apps/push-postgres' }; return }
            'rollout status' { return }
            'SELECT count\(\*\) FROM pg_database' { '2'; return }
            'SELECT count\(\*\) FROM pg_roles' {
                $script:roleQueries++
                if ($command -notmatch 'psql -X -v ON_ERROR_STOP=1 -U push -d postgres -Atc') {
                    throw 'psql must ignore user config and stop on errors'
                }
                if ($script:case -eq 'query-error') { $global:LASTEXITCODE = 7; return }
                if ($script:case -eq 'legacy-owner') { '0' } else { '1' }
                return
            }
            default { throw "unexpected or mutating command: $command" }
        }
    }
    $caught = $null
    try {
        & ([scriptblock]::Create($prefix)) -Cluster fixture
    } catch { $caught = $_.Exception.Message }
    $shouldPass = $case -in 'fresh', 'prepared'
    $ok = if ($shouldPass) {
        -not $caught -and ($case -eq 'fresh' -or $script:roleQueries -eq 1)
    } elseif ($case -eq 'query-error') {
        $caught -match 'exit code 7'
    } else {
        $caught -match 'docs/deployment-upgrade.md'
    }
    if (-not $ok) { $script:failureCount++; Write-Output "FAIL $case ($caught)" }
    else { Write-Output "ok   $case" }
}
if ($script:failureCount) { throw "$script:failureCount upgrade preflight cases failed" }
