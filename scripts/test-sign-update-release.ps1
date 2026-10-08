[CmdletBinding()]
param(
    [string]$UpdateZip = '',
    [string]$StagingDirectory = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$root = Get-HalcyonRoot
$version = Read-HalcyonVersion
if (-not $UpdateZip) { $UpdateZip = Join-Path $root "dist\halcyon-v$version-windows-x64-update.zip" }
if (-not $StagingDirectory) { $StagingDirectory = Join-Path $root "dist\halcyon-v$version-windows-x64-update" }
if (-not (Test-Path -LiteralPath $UpdateZip) -or -not (Test-Path -LiteralPath $StagingDirectory)) {
    throw 'Generate the local Windows update package first, then run this signing test.'
}

$scratch = Join-Path $env:TEMP ("halcyon-sign-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    $toolTarget = Join-Path $root 'target\sign-tool'
    Push-Location $root
    try {
        cargo build -p halcyon-sign --release --target-dir $toolTarget
        if ($LASTEXITCODE -ne 0) { throw 'halcyon-sign build failed' }
    } finally { Pop-Location }
    $signer = Join-Path $toolTarget 'release\halcyon-sign.exe'
    & $signer test-keygen --dir $scratch
    if ($LASTEXITCODE -ne 0) { throw 'Test key generation failed' }
    $secret = Join-Path $scratch 'halcyon-release.key'
    $public = Join-Path $scratch 'halcyon-release.pub'
    $signed = Join-Path $scratch 'signed'
    & (Join-Path $PSScriptRoot 'sign-update-release.ps1') `
        -UpdateZip $UpdateZip -AssetId 123456 `
        -PrivateKey $secret -OutputDirectory $signed -StagingDirectory $StagingDirectory
    if (-not $?) { throw 'Local signing script failed' }
    $artifact = Join-Path $signed ([IO.Path]::GetFileName($UpdateZip))
    $artifactName = [IO.Path]::GetFileName($artifact)
    foreach ($pair in @(@('latest.json','latest.json.minisig'),
                       @($artifactName,"$artifactName.minisig"))) {
        & $signer verify --public-key $public --input (Join-Path $signed $pair[0]) --signature (Join-Path $signed $pair[1])
        if ($LASTEXITCODE -ne 0) { throw "Test signature failed: $($pair[0])" }
    }
    $manifest = Get-Content (Join-Path $signed 'latest.json') -Raw | ConvertFrom-Json
    if ($manifest.version -ne $version -or $manifest.platforms.'windows-x86_64'.assetId -ne 123456) {
        throw 'Signed test manifest has unexpected version or asset ID'
    }
    $originalHash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash
    try {
        & (Join-Path $PSScriptRoot 'sign-update-release.ps1') `
            -UpdateZip $UpdateZip -AssetId 123456 -PrivateKey $secret `
            -OutputDirectory $signed -StagingDirectory $StagingDirectory
        throw 'Existing output directory was not rejected'
    } catch {
        if ($_.Exception.Message -notmatch 'OutputDirectory must be empty') { throw }
    }
    if ((Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash -ne $originalHash) {
        throw 'Previously signed artifact changed during overwrite rejection'
    }
    $portable = Join-Path $root "dist\halcyon-v$version-windows-x64-portable.zip"
    if (-not (Test-Path -LiteralPath $portable)) { throw "Portable ZIP missing: $portable" }
    $portableCopy = Join-Path $signed ([IO.Path]::GetFileName($portable))
    Copy-Item -LiteralPath $portable -Destination $portableCopy
    $hashLines = @($portableCopy, $artifact, (Join-Path $signed 'latest.json')) | ForEach-Object {
        '{0}  {1}' -f (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant(), [IO.Path]::GetFileName($_)
    }
    [IO.File]::WriteAllLines((Join-Path $signed 'SHA256SUMS.txt'), $hashLines, [Text.UTF8Encoding]::new($false))
    & (Join-Path $PSScriptRoot 'check-release.ps1') -AssetDirectory $signed -PublicKey $public
    if (-not $?) { throw 'Windows-only six-asset release check failed' }
    Write-Host 'Local signed update release test passed.'
} finally {
    $resolved = [IO.Path]::GetFullPath($scratch)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if (-not $resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        -not [IO.Path]::GetFileName($resolved).StartsWith('halcyon-sign-test-')) {
        throw "Refusing to clean unexpected test directory: $resolved"
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
