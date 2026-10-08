[CmdletBinding()]
param([switch]$SkipBuild)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$root = Get-HalcyonRoot
$scratch = Join-Path $env:TEMP ("halcyon-updater-test-" + [guid]::NewGuid().ToString('N'))
$helper = Join-Path $root 'target\debug\halcyon-updater.exe'
$fake = Join-Path $scratch 'fixtures\halcyon.exe'
$noReady = Join-Path $scratch 'fixtures\no-readiness.exe'
$token = 'test-readiness-token'

function Write-TestFile([string]$Path, [string]$Text) {
    New-Item -ItemType Directory -Path (Split-Path -Parent $Path) -Force | Out-Null
    [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false))
}
function New-TestCase([string]$Name) {
    $appDir = Join-Path $scratch $Name
    $staging = Join-Path $appDir 'updates\staging\1.0.1'
    $rollback = Join-Path $appDir 'updates\rollback\1.0.0'
    $journal = Join-Path $appDir 'updates\halcyon-update.journal.json'
    New-Item -ItemType Directory -Path $staging -Force | Out-Null
    Copy-Item -LiteralPath $fake -Destination (Join-Path $appDir 'halcyon.exe')
    Copy-Item -LiteralPath $fake -Destination (Join-Path $staging 'halcyon.exe')
    Copy-Item -LiteralPath $helper -Destination (Join-Path $staging 'halcyon-updater.exe')
    Write-TestFile (Join-Path $appDir 'VERSION.txt') "1.0.0`n"
    Write-TestFile (Join-Path $staging 'VERSION.txt') "1.0.1`n"
    Write-TestFile (Join-Path $appDir 'config.json') '{"preserve":true}'
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    $record = [ordered]@{
        schema=1; protocol=1; sourceKind='private_github'; fromVersion='1.0.0'; toVersion='1.0.1'
        appDir=$appDir; stagingDir=$staging; rollbackDir=$rollback
        helperPath=(Join-Path $staging 'halcyon-updater.exe'); parentPid=4294967294
        readinessToken=$token; state='prepared'; createdAt=$now; updatedAt=$now; error=$null
    }
    Write-TestFile $journal ($record | ConvertTo-Json -Depth 5)
    return @{ App=$appDir; Staging=$staging; Rollback=$rollback; Journal=$journal }
}
function Invoke-TestUpdater($Case, [string]$Launch) {
    & (Join-Path $Case.Staging 'halcyon-updater.exe') --protocol 1 --journal $Case.Journal `
      --target-app $Case.App --launch $Launch --expected-version 1.0.1 `
      --parent-pid 4294967294 --readiness-token $token
    return $LASTEXITCODE
}
function Read-TestJournal($Case) {
    return (Get-Content -LiteralPath $Case.Journal -Raw | ConvertFrom-Json)
}
function Assert-OldFiles($Case) {
    if ((Get-Content -LiteralPath (Join-Path $Case.App 'VERSION.txt') -Raw).Trim() -ne '1.0.0') { throw 'Rollback failed' }
    if ((Get-Content -LiteralPath (Join-Path $Case.App 'config.json') -Raw) -ne '{"preserve":true}') { throw 'User configuration changed' }
    if ((Read-TestJournal $Case).state -ne 'rolled_back') { throw 'Rollback journal state invalid' }
    if (-not (Test-Path -LiteralPath (Join-Path $Case.App "updates\readiness\$token.json"))) { throw 'Old version did not restart' }
}

New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    Push-Location $root
    try {
        if (-not $SkipBuild) {
            cargo build -p halcyon-updater
            if ($LASTEXITCODE -ne 0) { throw 'Updater build failed' }
        }
        if (-not (Test-Path -LiteralPath $helper)) { throw "Updater helper missing: $helper" }
        New-Item -ItemType Directory -Path (Split-Path -Parent $fake) -Force | Out-Null
        rustc updater/tests/fixtures/readiness_app.rs -o $fake
        if ($LASTEXITCODE -ne 0) { throw 'Readiness fixture build failed' }
        rustc updater/tests/fixtures/no_readiness_app.rs -o $noReady
        if ($LASTEXITCODE -ne 0) { throw 'No-readiness fixture build failed' }
    } finally { Pop-Location }

    $case = New-TestCase 'success'
    if ((Invoke-TestUpdater $case (Join-Path $case.App 'halcyon.exe')) -ne 0) { throw 'Successful update failed' }
    if ((Get-Content -LiteralPath (Join-Path $case.App 'VERSION.txt') -Raw).Trim() -ne '1.0.1' -or
        (Read-TestJournal $case).state -ne 'completed') { throw 'Successful update not completed' }
    if (-not (Test-Path -LiteralPath (Join-Path $case.App 'config.json'))) { throw 'User configuration lost' }

    $case = New-TestCase 'spawn-failure'
    Write-TestFile (Join-Path $case.Staging 'halcyon.exe') 'not-an-executable'
    if ((Invoke-TestUpdater $case (Join-Path $case.App 'halcyon.exe')) -eq 0) { throw 'Spawn failure unexpectedly succeeded' }
    Assert-OldFiles $case

    $case = New-TestCase 'readiness-timeout'
    Copy-Item -LiteralPath $noReady -Destination (Join-Path $case.Staging 'halcyon.exe') -Force
    $previous = $env:HALCYON_UPDATER_READINESS_TIMEOUT_MS
    try {
        $env:HALCYON_UPDATER_READINESS_TIMEOUT_MS = '300'
        if ((Invoke-TestUpdater $case (Join-Path $case.App 'halcyon.exe')) -eq 0) { throw 'Timeout unexpectedly succeeded' }
    } finally {
        if ($null -eq $previous) { Remove-Item Env:HALCYON_UPDATER_READINESS_TIMEOUT_MS -ErrorAction SilentlyContinue }
        else { $env:HALCYON_UPDATER_READINESS_TIMEOUT_MS = $previous }
    }
    Assert-OldFiles $case

    # Wait for fixture children before cleaning the exclusive scratch directory.
    Start-Sleep -Seconds 3
    Write-Host 'Windows updater end-to-end passed: replace, rollback, readiness timeout, configuration preservation.'
} finally {
    $resolved = [IO.Path]::GetFullPath($scratch)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if (-not $resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        -not [IO.Path]::GetFileName($resolved).StartsWith('halcyon-updater-test-')) {
        throw "Refusing to clean unexpected test directory: $resolved"
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
