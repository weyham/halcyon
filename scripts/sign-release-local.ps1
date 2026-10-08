[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$ArtifactsDirectory,
    # 私钥路径不设默认值：历史上默认指向开发者本机目录，公开仓库里既无意义
    # 也容易误导。优先取环境变量，其次要求显式传参。
    [string]$PrivateKey = $env:HALCYON_MINISIGN_KEY,
    [string]$PublicKey = $env:HALCYON_UPDATE_PUBLIC_KEY_FILE,
    [string]$Repository = 'weyham/halcyon',
    [string]$OutputDirectory = ''
)

# 本地完成发布签名（与 .github/workflows/release.yml 的 Sign and publish Draft 步骤等价）：
# 当 CI 的 release Environment 暂不可用（如账单限制）时，用本脚本把 CI 构建好的
# Windows/macOS 包签名并上传到既有空 Draft。私钥必须在仓库之外。
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($PrivateKey)) {
    throw 'Missing -PrivateKey (or env HALCYON_MINISIGN_KEY): minisign private key path, must live outside the repository'
}
if ([string]::IsNullOrWhiteSpace($PublicKey)) {
    throw 'Missing -PublicKey (or env HALCYON_UPDATE_PUBLIC_KEY_FILE): minisign public key path'
}
. (Join-Path $PSScriptRoot 'common.ps1')
$root = Get-HalcyonRoot
$version = Assert-HalcyonVersionConsistency
$releaseTag = "v$version"
$artifacts = (Resolve-Path -LiteralPath $ArtifactsDirectory).Path
$key = (Resolve-Path -LiteralPath $PrivateKey).Path
$rootPrefix = $root.TrimEnd('\','/') + [IO.Path]::DirectorySeparatorChar
if ($key.StartsWith($rootPrefix, [System.StringComparison]::OrdinalIgnoreCase)) { throw 'Private key must be outside source repository' }
$publicKeyText = ([IO.File]::ReadAllText((Resolve-Path -LiteralPath $PublicKey).Path)).Trim()

$update = Get-ChildItem -LiteralPath $artifacts -Filter '*-windows-x64-update.zip' | Select-Object -First 1
$portable = Get-ChildItem -LiteralPath $artifacts -Filter '*-windows-x64-portable.zip' | Select-Object -First 1
$mac = Get-ChildItem -LiteralPath $artifacts -Filter '*-macos-universal.tar.gz' | Select-Object -First 1
if (-not $update -or -not $portable) { throw "Artifacts missing in $artifacts (need halcyon-*-windows-x64-update.zip and -portable.zip)" }
$dmg = Get-ChildItem -LiteralPath $artifacts -Filter '*-macos-universal.dmg' | Select-Object -First 1
# macOS 可选：Windows-only 联调时清单里不出现 darwin-* 键（与 CI 的 platforms=windows 一致）。
$withMac = $null -ne $mac

# 安装通道五件是正式 Release 的必备项（B6）：本地签名路径必须与 CI 一样带上它们，
# 否则 Draft 永远少 5 项，finalize-release.ps1 的门禁会 fail closed。
$vpkDir = Join-Path $artifacts 'velopack'
$vpkNames = @('Halcyon-win-Setup.exe', "Halcyon-$version-full.nupkg", 'RELEASES',
    'releases.win.json', 'assets.win.json')
$vpkAssets = @()
foreach ($name in $vpkNames) {
    $path = Join-Path $vpkDir $name
    if (-not (Test-Path -LiteralPath $path)) { throw "Velopack asset missing: $path" }
    $vpkAssets += (Resolve-Path -LiteralPath $path).Path
}

if (-not $OutputDirectory) { $OutputDirectory = Join-Path $env:TEMP "halcyon-signed-$version" }
if (Test-Path -LiteralPath $OutputDirectory) {
    if (@(Get-ChildItem -LiteralPath $OutputDirectory -Force).Count -ne 0) { throw "OutputDirectory must be empty: $OutputDirectory" }
} else {
    New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
}

Push-Location $root
try {
    $toolTarget = Join-Path $root 'target\sign-tool'
    cargo build -p halcyon-sign --release --target-dir $toolTarget
    if ($LASTEXITCODE -ne 0) { throw 'halcyon-sign build failed' }
    $signer = Join-Path $toolTarget 'release\halcyon-sign.exe'

    # 上传前先把安装通道五件验一遍（与 CI、check-release.ps1 共用同一份口径）。
    & (Join-Path $PSScriptRoot 'check-velopack-assets.ps1') -VelopackDirectory $vpkDir -UpdateZip $update.FullName -Version $version
    if (-not $?) { throw 'Velopack asset verification failed' }

    $updateCopy = Join-Path $OutputDirectory $update.Name
    Copy-Item -LiteralPath $update.FullName -Destination $updateCopy
    $macCopy = $null
    $macSig = $null
    if ($withMac) {
        $macCopy = Join-Path $OutputDirectory $mac.Name
        Copy-Item -LiteralPath $mac.FullName -Destination $macCopy
    }

    $artifactSig = "$updateCopy.minisig"
    & $signer sign --secret-key $key --input $updateCopy --output $artifactSig --trusted-comment "Halcyon $version update artifact"
    if ($LASTEXITCODE -ne 0) { throw 'Windows artifact signing failed' }
    if ($withMac) {
        $macSig = "$macCopy.minisig"
        & $signer sign --secret-key $key --input $macCopy --output $macSig --trusted-comment "Halcyon $version macOS artifact"
        if ($LASTEXITCODE -ne 0) { throw 'macOS artifact signing failed' }
    }
    $dmgSig = $null
    if ($dmg) {
        $dmgSig = "$OutputDirectory\$($dmg.Name).minisig"
        Copy-Item -LiteralPath $dmg.FullName -Destination (Join-Path $OutputDirectory $dmg.Name)
        & $signer sign --secret-key $key --input (Join-Path $OutputDirectory $dmg.Name) --output $dmgSig --trusted-comment "Halcyon $version macOS dmg"
        if ($LASTEXITCODE -ne 0) { throw 'macOS dmg signing failed' }
        & $signer verify --public-key $PublicKey --input (Join-Path $OutputDirectory $dmg.Name) --signature $dmgSig
        if ($LASTEXITCODE -ne 0) { throw 'macOS dmg signature verification failed' }
    }
    $verifyPairs = @(,@($updateCopy,$artifactSig))
    if ($withMac) { $verifyPairs += ,@($macCopy,$macSig) }
    foreach ($pair in $verifyPairs) {
        & $signer verify --public-key $PublicKey --input $pair[0] --signature $pair[1]
        if ($LASTEXITCODE -ne 0) { throw "Artifact signature verification failed: $($pair[0])" }
    }

    $existingDraft = gh release view $releaseTag --repo $Repository --json isDraft --jq .isDraft 2>$null
    if ($LASTEXITCODE -ne 0) {
        $draftNotes = if ($withMac) { "Windows x64 (portable + installer) and macOS universal (tar.gz + dmg). Draft Release $version (locally signed)" } else { "Windows x64 (portable + installer). Draft Release $version (locally signed)" }
        gh release create $releaseTag --repo $Repository --draft --prerelease --title "Halcyon $version" --notes $draftNotes --verify-tag
        if ($LASTEXITCODE -ne 0) { throw "Creating Draft Release failed: $releaseTag" }
    } elseif ($existingDraft -ne 'true') {
        throw "Refusing to overwrite a published Release: $releaseTag"
    }

    # 列表端点有读延迟，轮询等待 Draft 可见且为空
    $releaseObject = $null
    foreach ($attempt in 1..6) {
        $allReleases = (gh api "repos/$Repository/releases?per_page=100" | ConvertFrom-Json)
        if ($LASTEXITCODE -ne 0) { throw 'Could not inspect Draft before upload' }
        $releaseObject = $allReleases | Where-Object tag_name -eq $releaseTag | Select-Object -First 1
        if ($releaseObject) { break }
        Start-Sleep -Seconds 3
    }
    if (-not $releaseObject -or -not $releaseObject.draft) { throw "Draft Release not found for $releaseTag" }
    if ($releaseObject.assets.Count -ne 0) { throw 'Draft must be empty; refusing to overwrite existing assets' }

    $uploads = @((Join-Path $artifacts $portable.Name), (Join-Path $artifacts $update.Name), $artifactSig) + $vpkAssets
    if ($withMac) { $uploads += @((Join-Path $artifacts $mac.Name), $macSig) }
    if ($dmg) { $uploads += @((Join-Path $artifacts $dmg.Name), $dmgSig) }
    gh release upload $releaseTag @uploads --repo $Repository
    if ($LASTEXITCODE -ne 0) { throw 'Uploading Draft assets failed' }

    foreach ($attempt in 1..6) {
        $allReleases = (gh api "repos/$Repository/releases?per_page=100" | ConvertFrom-Json)
        if ($LASTEXITCODE -ne 0) { throw 'Could not read Draft Release' }
        $releaseObject = $allReleases | Where-Object tag_name -eq $releaseTag | Select-Object -First 1
        # 上传后至少要有：Windows 自研链 3 项 + 安装通道 5 项。
        if ($releaseObject -and $releaseObject.assets.Count -ge 8) { break }
        Start-Sleep -Seconds 3
    }
    $assetId = ($releaseObject.assets | Where-Object name -eq $update.Name).id
    if (-not $assetId) { throw 'Uploaded Windows update asset not found in Draft' }
    $macAssetId = $null
    if ($withMac) {
        $macAssetId = ($releaseObject.assets | Where-Object name -eq $mac.Name).id
        if (-not $macAssetId) { throw 'Uploaded macOS asset not found in Draft' }
    }

    $helper = Join-Path $env:TEMP "halcyon-helper-$version"
    if (Test-Path -LiteralPath $helper) { Remove-Item -LiteralPath $helper -Recurse -Force }
    Expand-Archive (Join-Path $artifacts $update.Name) $helper
    # url 是零 API 更新源（releases/download CDN 路由）的制品地址；assetId 保留给 1.0.0 客户端
    $downloadBase = "https://github.com/$Repository/releases/download/$releaseTag"
    $platforms = [ordered]@{}
    $platforms['windows-x86_64'] = [ordered]@{ assetId=[int64]$assetId; url="$downloadBase/$($update.Name)"; signature=[IO.File]::ReadAllText($artifactSig); sha256=(Get-FileHash $updateCopy -Algorithm SHA256).Hash.ToLowerInvariant(); size=(Get-Item $updateCopy).Length; helper=[ordered]@{ path='halcyon-updater.exe'; sha256=(Get-FileHash (Join-Path $helper 'halcyon-updater.exe') -Algorithm SHA256).Hash.ToLowerInvariant(); size=(Get-Item (Join-Path $helper 'halcyon-updater.exe')).Length }; manualOnly=$false; allowDowngrade=$false }
    if ($withMac) {
        $macEntry = [ordered]@{ assetId=[int64]$macAssetId; url="$downloadBase/$($mac.Name)"; signature=[IO.File]::ReadAllText($macSig); sha256=(Get-FileHash $macCopy -Algorithm SHA256).Hash.ToLowerInvariant(); size=(Get-Item $macCopy).Length; manualOnly=$true; allowDowngrade=$false }
        $platforms['darwin-aarch64'] = $macEntry
        $platforms['darwin-x86_64'] = $macEntry
    }
    $manifest = [ordered]@{ schema=1; protocol=1; version=$version; notes=""; platforms=$platforms }
    $manifestPath = Join-Path $OutputDirectory 'latest.json'
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    & $signer sign --secret-key $key --input $manifestPath --output "$manifestPath.minisig" --trusted-comment "Halcyon $version update manifest"
    if ($LASTEXITCODE -ne 0) { throw 'Manifest signing failed' }
    & $signer verify --public-key $PublicKey --input $manifestPath --signature "$manifestPath.minisig"
    if ($LASTEXITCODE -ne 0) { throw 'Manifest signature verification failed' }

    $checksumFiles = @((Join-Path $artifacts $portable.Name), (Join-Path $artifacts $update.Name))
    if ($withMac) { $checksumFiles += (Join-Path $artifacts $mac.Name) }
    if ($dmg) { $checksumFiles += (Join-Path $artifacts $dmg.Name) }
    $checksumFiles += $manifestPath
    $checksums = $checksumFiles | ForEach-Object {
        "$( (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant() )  $([IO.Path]::GetFileName($_))"
    }
    $checksumFile = Join-Path $OutputDirectory 'SHA256SUMS.txt'
    [IO.File]::WriteAllLines($checksumFile, $checksums, [Text.UTF8Encoding]::new($false))
    gh release upload $releaseTag $manifestPath "$manifestPath.minisig" $checksumFile --repo $Repository
    if ($LASTEXITCODE -ne 0) { throw 'Uploading signed manifest failed' }
    Write-Host "Locally signed Draft uploaded: $releaseTag -> $OutputDirectory"
} finally { Pop-Location }
