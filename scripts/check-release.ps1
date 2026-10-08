[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$AssetDirectory,
    [Parameter(Mandatory=$true)][string]$PublicKey
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$root = Get-HalcyonRoot
$assets = (Resolve-Path -LiteralPath $AssetDirectory).Path
$key = (Resolve-Path -LiteralPath $PublicKey).Path
$version = Read-HalcyonVersion
$portableName = "halcyon-v$version-windows-x64-portable.zip"
$updateName = "halcyon-v$version-windows-x64-update.zip"
$macName = "halcyon-v$version-macos-universal.tar.gz"
$dmgName = "halcyon-v$version-macos-universal.dmg"

$manifest = Get-Content -LiteralPath (Join-Path $assets 'latest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
if ($manifest.version -ne $version -or $manifest.schema -ne 1 -or $manifest.protocol -ne 1) {
    throw 'Manifest version/schema/protocol mismatch'
}
# 平台集合由清单自描述：含任一 darwin-* 键则要求 macOS 资产，否则纯 Windows 六项。
$darwinKeys = @($manifest.platforms.PSObject.Properties.Name | Where-Object { $_ -like 'darwin-*' })
$hasMac = $darwinKeys.Count -gt 0
$nupkgName = "Halcyon-$version-full.nupkg"
# 安装通道（K1）：五个资产是正式 Release 的必备项（Windows-only 联调也要带）
$velopackNames = @('Halcyon-win-Setup.exe', $nupkgName, 'RELEASES', 'releases.win.json', 'assets.win.json')
$required = @($portableName, $updateName, "$updateName.minisig",
    'latest.json', 'latest.json.minisig', 'SHA256SUMS.txt') + $velopackNames
if ($hasMac) { $required += @($macName, "$macName.minisig") }
if (Test-Path -LiteralPath (Join-Path $assets $dmgName)) {
    $required += @($dmgName, "$dmgName.minisig")
}

foreach ($name in $required) {
    if (-not (Test-Path -LiteralPath (Join-Path $assets $name))) { throw "Missing release asset: $name" }
}
$unexpected = @(Get-ChildItem -LiteralPath $assets -File | Where-Object Name -NotIn $required)
if ($unexpected.Count) { throw "Unexpected release assets: $($unexpected.Name -join ', ')" }

Add-Type -AssemblyName System.IO.Compression.FileSystem
function Check-WindowsZip([string]$Name, [string[]]$Expected) {
    $zip = [IO.Compression.ZipFile]::OpenRead((Join-Path $assets $Name))
    try {
        $entries = @($zip.Entries | ForEach-Object FullName)
        $missing = @($Expected | Where-Object { $_ -notin $entries })
        $extra = @($entries | Where-Object { $_ -notin $Expected })
        if ($missing.Count -or $extra.Count) { throw "Archive contents mismatch: $Name missing=$missing extra=$extra" }
        $reader = [IO.StreamReader]::new($zip.GetEntry('VERSION.txt').Open())
        try { $actual = $reader.ReadToEnd().Trim() } finally { $reader.Dispose() }
        if ($actual -ne $version) { throw "VERSION.txt mismatch in $Name" }
        if ($Name -eq $updateName) {
            $helper = $zip.GetEntry('halcyon-updater.exe')
            $stream = $helper.Open()
            try {
                $sha = [Security.Cryptography.SHA256]::Create()
                $hash = [Convert]::ToHexString($sha.ComputeHash($stream)).ToLowerInvariant()
            } finally { $stream.Dispose(); $sha.Dispose() }
            $expectedHelper = $manifest.platforms.'windows-x86_64'.helper
            if ($expectedHelper.path -ne $helper.FullName -or $expectedHelper.sha256 -ne $hash -or
                $expectedHelper.size -ne $helper.Length) { throw 'Updater helper hash/size mismatch' }
        }
    } finally { $zip.Dispose() }
}

Check-WindowsZip $portableName @('halcyon.exe','VERSION.txt','LICENSE.txt','README-portable.txt')
Check-WindowsZip $updateName @('halcyon.exe','halcyon-updater.exe','VERSION.txt')
if ($hasMac) {
    $macListing = @(& tar -tzf (Join-Path $assets $macName))
    if ($LASTEXITCODE -ne 0) { throw 'macOS archive unreadable' }
    $normalized = $macListing | ForEach-Object { $_.TrimStart('./') }
    $hasApp = @($normalized | Where-Object { $_ -eq 'Halcyon.app/Contents/MacOS/halcyon' }).Count -eq 1 -and
        @($normalized | Where-Object { $_ -eq 'Halcyon.app/Contents/Info.plist' }).Count -eq 1
    $hasLegacyBare = @($normalized | Where-Object { $_ -eq 'halcyon' }).Count -eq 1
    # 1.0.4 起为 .app 结构；早期归档备份允许裸可执行文件形态。
    if (-not $hasApp -and -not $hasLegacyBare) { throw 'macOS archive missing Halcyon.app bundle (or legacy bare executable)' }
}

$target = Join-Path $root 'target\sign-tool'
Push-Location $root
try {
    cargo build -p halcyon-sign --release --target-dir $target
    if ($LASTEXITCODE -ne 0) { throw 'halcyon-sign build failed' }
} finally { Pop-Location }
$signer = Join-Path $target 'release\halcyon-sign.exe'
$signatures = @(@('latest.json','latest.json.minisig'), @($updateName,"$updateName.minisig"))
if ($hasMac) { $signatures += ,@($macName,"$macName.minisig") }
if (Test-Path -LiteralPath (Join-Path $assets $dmgName)) { $signatures += ,@($dmgName,"$dmgName.minisig") }
foreach ($pair in $signatures) {
    & $signer verify --public-key $key --input (Join-Path $assets $pair[0]) --signature (Join-Path $assets $pair[1])
    if ($LASTEXITCODE -ne 0) { throw "Signature failed: $($pair[0])" }
}

$platforms = @(,@('windows-x86_64',$updateName,$false))
foreach ($key in $darwinKeys) { $platforms += ,@($key,$macName,$true) }
foreach ($platform in $platforms) {
    $entry = $manifest.platforms.($platform[0]); $file = Get-Item -LiteralPath (Join-Path $assets $platform[1])
    $sig = [IO.File]::ReadAllText("$($file.FullName).minisig")
    $digest = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    $expectedUrl = "https://github.com/weyham/halcyon/releases/download/v$version/$($platform[1])"
    if ($entry.assetId -le 0 -or $entry.url -cne $expectedUrl -or $entry.size -ne $file.Length -or
        $entry.sha256 -ne $digest -or $entry.signature -cne $sig -or
        $entry.manualOnly -ne $platform[2] -or $entry.allowDowngrade) {
        throw "Manifest artifact mismatch: $($platform[0])"
    }
}

$seen = @{}
foreach ($line in Get-Content -LiteralPath (Join-Path $assets 'SHA256SUMS.txt')) {
    if ($line -notmatch '^([0-9a-fA-F]{64})\s+(.+)$') { throw 'Malformed SHA256SUMS line' }
    $name = $Matches[2]; $expectedHash = $Matches[1]; $file = Join-Path $assets $name
    if (-not (Test-Path -LiteralPath $file)) { throw "SHA256SUMS refers to missing file: $name" }
    if ((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash -ine $expectedHash) { throw "SHA256SUMS mismatch: $name" }
    if ($seen.ContainsKey($name)) { throw "Duplicate SHA256SUMS entry: $name" }
    $seen[$name] = $true
}
$checksummed = @($portableName,$updateName,'latest.json')
if ($hasMac) { $checksummed += $macName }
if (Test-Path -LiteralPath (Join-Path $assets $dmgName)) { $checksummed += $dmgName }
if ($seen.Count -ne $checksummed.Count) { throw 'SHA256SUMS has unexpected entries' }
foreach ($name in $checksummed) {
    if (-not $seen.ContainsKey($name)) { throw "SHA256SUMS missing: $name" }
}
# --- 安装通道（Velopack）与自研链的交叉自洽（K1）---
# 口径与 CI 构建阶段完全一致：共用 scripts/check-velopack-assets.ps1。
& (Join-Path $PSScriptRoot 'check-velopack-assets.ps1') `
    -VelopackDirectory $assets -UpdateZip (Join-Path $assets $updateName) -Version $version
if (-not $?) { throw 'Velopack asset verification failed' }

Write-Host "Release asset verification passed: v$version (macOS=$hasMac)"
