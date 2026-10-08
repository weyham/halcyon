[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][ValidatePattern('^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')][string]$Tag,
    [Parameter(Mandatory=$true)][string]$AssetsDirectory,
    [Parameter(Mandatory=$true)][string]$PublicKey,
    [string]$Repository = 'weyham/halcyon',
    [switch]$Publish
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$version = Read-HalcyonVersion
if ($Tag -ne "v$version") { throw "Tag $Tag does not match source version $version" }
$json = & gh release view $Tag --repo $Repository --json isDraft,isPrerelease,assets,tagName
if ($LASTEXITCODE -ne 0) { throw "Release $Tag could not be read" }
$release = $json | ConvertFrom-Json
if (-not $release.isDraft) { throw "Release $Tag is already published; refusing to change it" }
if ($release.tagName -ne $Tag) { throw 'Release tag mismatch' }

$assets = (Resolve-Path -LiteralPath $AssetsDirectory).Path
$names = @($release.assets | ForEach-Object name)
# K1/K2：正式 Release = 全套。Windows 自研链六项 + 安装通道五件是**必备**；
# macOS 四项由 latest.json 里有没有 darwin-* 决定（Windows-only 联调时可不带）。
$expected = @("halcyon-$Tag-windows-x64-portable.zip", "halcyon-$Tag-windows-x64-update.zip",
    "halcyon-$Tag-windows-x64-update.zip.minisig", 'latest.json', 'latest.json.minisig', 'SHA256SUMS.txt',
    'Halcyon-win-Setup.exe', "Halcyon-$($Tag.TrimStart('v'))-full.nupkg", 'RELEASES',
    'releases.win.json', 'assets.win.json')
$localManifest = Get-Content -LiteralPath (Join-Path $assets 'latest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
if (@($localManifest.platforms.PSObject.Properties.Name | Where-Object { $_ -like 'darwin-*' }).Count -gt 0) {
    $expected += @("halcyon-$Tag-macos-universal.tar.gz", "halcyon-$Tag-macos-universal.tar.gz.minisig")
    $dmgName = "halcyon-$Tag-macos-universal.dmg"
    if (Test-Path -LiteralPath (Join-Path $assets $dmgName)) {
        $expected += @($dmgName, "$dmgName.minisig")
    }
}
if ($names.Count -ne $expected.Count -or @($expected | Where-Object { $_ -notin $names }).Count -or
    @($names | Where-Object { $_ -notin $expected }).Count) {
    throw "Draft assets do not match the required names ($($expected.Count)): $($names -join ', ')"
}
& (Join-Path $PSScriptRoot 'check-release.ps1') -AssetDirectory $assets -PublicKey $PublicKey
if (-not $?) { throw 'Local release verification failed' }
foreach ($asset in $release.assets) {
    $file = Get-Item -LiteralPath (Join-Path $assets $asset.name)
    $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($file.Length -ne $asset.size -or $asset.digest -ne "sha256:$hash") {
        throw "Local asset does not match Draft: $($asset.name)"
    }
}
if (-not $Publish) {
    Write-Host "Dry run: $Tag Draft and all remote assets verified. Use -Publish for the final state change."
    return
}
& gh release edit $Tag --repo $Repository --draft=false --prerelease=false --latest
if ($LASTEXITCODE -ne 0) { throw "Failed to publish $Tag" }
Write-Host "Release $Tag published."
