#!/usr/bin/env pwsh
# Replaces the vendored SQLite3 Multiple Ciphers (crates/push-sdk/vendor/sqlite3mc) with
# the amalgamation of an upstream release, after verifying it.
#
#   pwsh scripts/update-sqlite3mc.ps1 -Version 2.5.1
#
# Verification (any failure stops the script before a vendored file is touched):
#   1. `gh attestation verify` checks the GitHub Artifact Attestations (Sigstore) of the
#      amalgamation zip and of the release's SHA256SUMS: signed by upstream's release
#      workflow ($Signer below), OIDC issuer GitHub Actions, on a GitHub-hosted runner.
#   2. If cosign is on the PATH (or given with -Cosign), `cosign verify-blob` checks the
#      Sigstore signature of SHA256SUMS against the same identity. Without cosign this
#      step is skipped with a warning; step 1 already covers the same files.
#   3. The zip's SHA-256 must be the one SHA256SUMS lists for it and, when the release is
#      the one already vendored, the asset-sha256 pinned in VERSION.
#   4. The header must declare the requested SQLite3MC version and the SQLite version the
#      asset name gives.
# LICENSE is not in the zip: it is fetched over HTTPS from the repository at the commit
# the attestation names as the release's source.
#
# Needs gh (signed in) and optionally cosign. Downloads go to target/sqlite3mc-update.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$Cosign = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Repo = 'utelle/SQLite3MultipleCiphers'
# The trust anchor: Sigstore's public-good root (through gh and cosign) plus this exact
# certificate identity of upstream's release workflow.
$Signer = 'https://github.com/utelle/SQLite3MultipleCiphers/.github/workflows/build-reusable.yml@refs/heads/main'
$Issuer = 'https://token.actions.githubusercontent.com'

$root = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $root 'crates/push-sdk/vendor/sqlite3mc'
$work = Join-Path $root 'target/sqlite3mc-update'
$tag = "v$Version"
$files = @('sqlite3mc_amalgamation.c', 'sqlite3mc_amalgamation.h')

function Get-Sha256([string]$Path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Invoke-Checked([string]$What, [scriptblock]$Command) {
    $output = & $Command 2>&1
    if ($LASTEXITCODE -ne 0) {
        $output | Out-Host
        throw "$What failed"
    }
    $output
}

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "-Version must look like 2.5.1, not $Version"
}
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw 'gh (GitHub CLI) is required'
}
if (Test-Path $work) {
    Remove-Item -Recurse -Force $work
}
New-Item -ItemType Directory -Force $work | Out-Null

Write-Host "Downloading $Repo $tag"
$sums = "sqlite3mc-$Version-SHA256SUMS"
$sumsPath = Join-Path $work $sums
Invoke-Checked 'download' {
    gh release download $tag -R $Repo -D $work -p $sums -p "$sums.pem" -p "$sums.sig" -p "sqlite3mc-$Version-sqlite-*-amalgamation.zip"
} | Out-Null
$zips = @(Get-ChildItem -LiteralPath $work -Filter "sqlite3mc-$Version-sqlite-*-amalgamation.zip")
if ($zips.Count -ne 1) {
    throw "expected one amalgamation zip in release $tag"
}
$zip = $zips[0]
$nameMatch = [regex]::Match($zip.Name, "^sqlite3mc-$([regex]::Escape($Version))-sqlite-(\d+\.\d+\.\d+)-amalgamation\.zip$")
if (-not $nameMatch.Success) {
    throw "unexpected asset name $($zip.Name)"
}
$sqliteVersion = $nameMatch.Groups[1].Value

Write-Host 'Verifying the GitHub Artifact Attestations'
$sourceCommit = $null
foreach ($file in @($zip.FullName, $sumsPath)) {
    $json = Invoke-Checked "attestation of $file" {
        gh attestation verify $file -R $Repo --cert-identity $Signer --cert-oidc-issuer $Issuer --deny-self-hosted-runners --format json
    }
    $results = @(($json | Out-String) | ConvertFrom-Json)
    $commit = $results[0].verificationResult.signature.certificate.sourceRepositoryDigest
    if (-not $commit) {
        throw "the attestation of $file names no source commit"
    }
    if ($sourceCommit -and $sourceCommit -ne $commit) {
        throw 'the attestations name different source commits'
    }
    $sourceCommit = $commit
}

if (-not $Cosign) {
    $found = Get-Command cosign -ErrorAction SilentlyContinue
    if ($found) {
        $Cosign = $found.Source
    }
}
if ($Cosign) {
    Write-Host 'Verifying the cosign signature of SHA256SUMS'
    Invoke-Checked 'cosign verify-blob' {
        & $Cosign verify-blob --certificate "$sumsPath.pem" --signature "$sumsPath.sig" --certificate-identity $Signer --certificate-oidc-issuer $Issuer $sumsPath
    } | Out-Null
}
else {
    Write-Warning 'cosign not found: the cosign signature of SHA256SUMS was not checked (the attestations were)'
}

$zipSha = Get-Sha256 $zip.FullName
$listed = @()
foreach ($line in Get-Content -LiteralPath $sumsPath) {
    $lineMatch = [regex]::Match($line, "^([0-9a-f]{64})\s+\*?$([regex]::Escape($zip.Name))$")
    if ($lineMatch.Success) {
        $listed += $lineMatch.Groups[1].Value
    }
}
if ($listed.Count -ne 1 -or $listed[0] -ne $zipSha) {
    throw "$($zip.Name) has SHA-256 $zipSha, which SHA256SUMS does not list"
}
$versionFile = Join-Path $vendor 'VERSION'
if (Test-Path $versionFile) {
    $pinned = @{}
    foreach ($line in Get-Content -LiteralPath $versionFile) {
        $lineMatch = [regex]::Match($line, '^([a-z0-9-]+):\s*(.*)$')
        if ($lineMatch.Success -and $lineMatch.Groups[1].Value -ne 'file') {
            $pinned[$lineMatch.Groups[1].Value] = $lineMatch.Groups[2].Value
        }
    }
    if ($pinned['release'] -eq $tag -and $pinned['asset-sha256'] -ne $zipSha) {
        throw "$($zip.Name) has SHA-256 $zipSha, but VERSION pins $($pinned['asset-sha256']) for $tag"
    }
}

$extract = Join-Path $work 'amalgamation'
Expand-Archive -LiteralPath $zip.FullName -DestinationPath $extract
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath (Join-Path $extract $file))) {
        throw "$file is missing from $($zip.Name)"
    }
}
$header = Get-Content -LiteralPath (Join-Path $extract 'sqlite3mc_amalgamation.h')
$mcPattern = '^#define SQLITE3MC_VERSION_STRING\s+"SQLite3 Multiple Ciphers ' + [regex]::Escape($Version) + '"'
if (-not ($header -match $mcPattern)) {
    throw "the header does not declare SQLite3 Multiple Ciphers $Version"
}
$sqlitePattern = '^#define SQLITE_VERSION\s+"' + [regex]::Escape($sqliteVersion) + '"'
if (-not ($header -match $sqlitePattern)) {
    throw "the header does not declare SQLite $sqliteVersion"
}

Write-Host "Fetching LICENSE at $sourceCommit"
$license = Join-Path $work 'LICENSE'
Invoke-WebRequest -UseBasicParsing -Uri "https://raw.githubusercontent.com/$Repo/$sourceCommit/LICENSE" -OutFile $license
$tagCommit = (Invoke-Checked 'tag lookup' { gh api "repos/$Repo/commits/$tag" --jq '.sha' } | Out-String).Trim()
if ($tagCommit -ne $sourceCommit) {
    Write-Warning "tag $tag is commit $tagCommit, but the release was built from $sourceCommit"
}

New-Item -ItemType Directory -Force $vendor | Out-Null
foreach ($file in $files) {
    Copy-Item -LiteralPath (Join-Path $extract $file) -Destination (Join-Path $vendor $file) -Force
}
Copy-Item -LiteralPath $license -Destination (Join-Path $vendor 'LICENSE') -Force
# Copy-Item keeps the old modification time; touch the files so cargo rebuilds.
foreach ($file in $files + @('LICENSE')) {
    (Get-Item -LiteralPath (Join-Path $vendor $file)).LastWriteTime = Get-Date
}

$lines = @(
    "# SQLite3 Multiple Ciphers, vendored for push-sdk's build.rs.",
    '# Written by scripts/update-sqlite3mc.ps1 (or .sh); do not edit by hand.',
    '# build.rs fails the build unless every file: line below has exactly its SHA-256.',
    "release: $tag",
    "sqlite3mc: $Version",
    "sqlite: $sqliteVersion",
    "asset: $($zip.Name)",
    "asset-sha256: $zipSha",
    "sha256sums-sha256: $(Get-Sha256 $sumsPath)",
    "source-commit: $sourceCommit",
    "signer: $Signer",
    "oidc-issuer: $Issuer",
    "retrieved: $((Get-Date).ToUniversalTime().ToString('yyyy-MM-dd'))"
)
foreach ($file in $files + @('LICENSE')) {
    $lines += "file: $(Get-Sha256 (Join-Path $vendor $file))  $file"
}
[IO.File]::WriteAllText($versionFile, ($lines -join "`n") + "`n")

Remove-Item -Recurse -Force $work
Write-Host "Vendored SQLite3 Multiple Ciphers $Version (SQLite $sqliteVersion), built from $sourceCommit."
Write-Host 'Next: cargo test -p push-sdk --locked, then review and commit crates/push-sdk/vendor/sqlite3mc.'
