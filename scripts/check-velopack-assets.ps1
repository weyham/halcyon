[CmdletBinding()]
param(
    # 安装通道五件所在目录（vpk pack 的 outputDir）。
    [Parameter(Mandatory=$true)][string]$VelopackDirectory,
    # 自研链的 update 包（用来验证"两个通道的 halcyon.exe 同哈希"）。
    [Parameter(Mandatory=$true)][string]$UpdateZip,
    # 缺省取仓库当前版本；CI 里显式传版本，避免与打包目录漂移。
    [string]$Version = ''
)

# 安装通道（Velopack）五件的自洽校验（B6/K1）。
#
# 单独成脚本的原因：这套校验必须**在 CI 构建 job 里就跑**——等 Draft 建好、
# 资产上传完再发现不一致，已经太晚（Draft 非空会让重跑 fail）。
# `check-release.ps1` 与 `.github/workflows/release.yml` 共用本脚本，避免两处口径漂移。
#
# 校验内容：
#   - releases.win.json 的 Version / FileName / Size / SHA256 / SHA1 == 实际 nupkg；
#   - RELEASES 行内 SHA1 == 实际 nupkg；
#   - assets.win.json 同时列出 nupkg 与 Setup；
#   - Setup.exe 的 FileVersion / ProductVersion == 版本号；
#   - nupkg 内 lib/app/halcyon.exe 与 update 包内 halcyon.exe 逐字节同哈希。
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')

$vpkDir = (Resolve-Path -LiteralPath $VelopackDirectory).Path
$updatePath = (Resolve-Path -LiteralPath $UpdateZip).Path
$version = if ([string]::IsNullOrWhiteSpace($Version)) { Read-HalcyonVersion } else { $Version }
$nupkgName = "Halcyon-$version-full.nupkg"
$nupkgPath = Join-Path $vpkDir $nupkgName
$setupPath = Join-Path $vpkDir 'Halcyon-win-Setup.exe'

foreach ($path in @($nupkgPath, $setupPath, (Join-Path $vpkDir 'RELEASES'),
        (Join-Path $vpkDir 'releases.win.json'), (Join-Path $vpkDir 'assets.win.json'))) {
    if (-not (Test-Path -LiteralPath $path)) { throw "Velopack asset missing: $path" }
}

function Get-ZipEntrySha256([string]$ZipPath, [string]$EntryName) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($ZipPath)
    try {
        $entry = $archive.Entries | Where-Object { $_.FullName -eq $EntryName }
        if (-not $entry) { throw "Zip entry not found: $EntryName in $ZipPath" }
        $stream = $entry.Open()
        $sha = [Security.Cryptography.SHA256]::Create()
        try { return [Convert]::ToHexString($sha.ComputeHash($stream)).ToLowerInvariant() }
        finally { $stream.Dispose(); $sha.Dispose() }
    } finally { $archive.Dispose() }
}

$nupkg = Get-Item -LiteralPath $nupkgPath
$releasesWin = Get-Content -LiteralPath (Join-Path $vpkDir 'releases.win.json') -Raw -Encoding UTF8 | ConvertFrom-Json
$releaseEntry = @($releasesWin.Assets)[0]
if ($releaseEntry.Version -ne $version) { throw 'releases.win.json version mismatch' }
if ($releaseEntry.FileName -ne $nupkgName) { throw 'releases.win.json nupkg name mismatch' }
if ($releaseEntry.Size -ne $nupkg.Length) { throw 'releases.win.json nupkg size mismatch' }
if ($releaseEntry.SHA256 -ine (Get-FileHash -LiteralPath $nupkgPath -Algorithm SHA256).Hash) {
    throw 'releases.win.json nupkg SHA256 mismatch'
}
if ($releaseEntry.SHA1 -ine (Get-FileHash -LiteralPath $nupkgPath -Algorithm SHA1).Hash) {
    throw 'releases.win.json nupkg SHA1 mismatch'
}

$releasesText = (Get-Content -LiteralPath (Join-Path $vpkDir 'RELEASES') -Raw -Encoding UTF8).Trim()
if ($releasesText -notlike "*$nupkgName*") { throw 'RELEASES does not reference the nupkg' }
$releasesSha1 = ($releasesText -split '\s+')[0]
if ($releasesSha1 -ine (Get-FileHash -LiteralPath $nupkgPath -Algorithm SHA1).Hash) {
    throw 'RELEASES SHA1 mismatch'
}

$assetsWin = Get-Content -LiteralPath (Join-Path $vpkDir 'assets.win.json') -Raw -Encoding UTF8 | ConvertFrom-Json
foreach ($want in @($nupkgName, 'Halcyon-win-Setup.exe')) {
    if (@($assetsWin | Where-Object RelativeFileName -eq $want).Count -ne 1) {
        throw "assets.win.json missing $want"
    }
}

$setupInfo = (Get-Item -LiteralPath $setupPath).VersionInfo
if ($setupInfo.FileVersion -ne $version -or $setupInfo.ProductVersion -ne $version) {
    throw "Setup.exe version mismatch: $($setupInfo.FileVersion)/$($setupInfo.ProductVersion)"
}

# 一次构建、多通道打包（H2）：两个通道给用户的 halcyon.exe 必须逐字节相同。
$nupkgExe = Get-ZipEntrySha256 $nupkgPath 'lib/app/halcyon.exe'
$updateExe = Get-ZipEntrySha256 $updatePath 'halcyon.exe'
if ($nupkgExe -ne $updateExe) {
    throw "两个通道的 halcyon.exe 不同哈希：nupkg=$nupkgExe update=$updateExe"
}

Write-Host "Velopack asset verification passed: v$version"
