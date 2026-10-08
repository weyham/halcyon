[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$UpdateZip,
    [Parameter(Mandatory=$true)][ValidateRange(1,[long]::MaxValue)][long]$AssetId,
    [Parameter(Mandatory=$true)][string]$PrivateKey,
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [string]$StagingDirectory = ""
)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")
$root = Get-HalcyonRoot
$toolTarget = Join-Path $root "target\sign-tool"
Push-Location $root
try {
    $version = Assert-HalcyonVersionConsistency
    $zip = (Resolve-Path -LiteralPath $UpdateZip).Path
    $key = (Resolve-Path -LiteralPath $PrivateKey).Path
    $rootPrefix = $root.TrimEnd('\','/') + [IO.Path]::DirectorySeparatorChar
    if ($key.StartsWith($rootPrefix, [System.StringComparison]::OrdinalIgnoreCase)) { throw "Private key must be outside source repository" }
    if (-not $StagingDirectory) { throw "StagingDirectory is required to hash halcyon-updater.exe" }
    $staging = (Resolve-Path -LiteralPath $StagingDirectory).Path
    $helper = Join-Path $staging "halcyon-updater.exe"
    if (-not (Test-Path -LiteralPath $helper)) { throw "Updater helper not found: $helper" }
    if (Test-Path -LiteralPath $OutputDirectory) {
        if (@(Get-ChildItem -LiteralPath $OutputDirectory -Force).Count -ne 0) {
            throw "OutputDirectory must be empty to avoid overwriting signed assets: $OutputDirectory"
        }
    } else {
        New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    }
    $output = (Resolve-Path -LiteralPath $OutputDirectory).Path
    if ($output -eq $root -or $output -eq $staging -or $output -eq (Split-Path -Parent $zip)) {
        throw 'OutputDirectory must be separate from source and staging directories'
    }
    cargo build -p halcyon-sign --release --target-dir $toolTarget
    if ($LASTEXITCODE -ne 0) { throw "halcyon-sign build failed" }
    $signer = Join-Path $toolTarget "release\halcyon-sign.exe"
    $zipName = [IO.Path]::GetFileName($zip)
    $zipCopy = Join-Path $output $zipName
    Copy-Item -LiteralPath $zip -Destination $zipCopy
    $zipSig = "$zipCopy.minisig"
    & $signer sign --secret-key $key --input $zipCopy --output $zipSig --trusted-comment "Halcyon $version update artifact"
    if ($LASTEXITCODE -ne 0) { throw "artifact signing failed" }
    $zipHash = (Get-FileHash $zipCopy -Algorithm SHA256).Hash.ToLowerInvariant()
    $helperHash = (Get-FileHash $helper -Algorithm SHA256).Hash.ToLowerInvariant()
    $manifest = [ordered]@{
        schema=1; protocol=1; version=$version; notes=""
        platforms=[ordered]@{ "windows-x86_64"=[ordered]@{
            assetId=$AssetId
            signature=[IO.File]::ReadAllText($zipSig, [Text.UTF8Encoding]::new($false))
            sha256=$zipHash; size=(Get-Item $zipCopy).Length
            helper=[ordered]@{ path="halcyon-updater.exe"; sha256=$helperHash; size=(Get-Item $helper).Length }
            manualOnly=$false; allowDowngrade=$false
        }}
    }
    $manifestPath = Join-Path $output "latest.json"
    Write-Utf8File $manifestPath ($manifest | ConvertTo-Json -Depth 8)
    $manifestSig = Join-Path $output "latest.json.minisig"
    & $signer sign --secret-key $key --input $manifestPath --output $manifestSig --trusted-comment "Halcyon $version update manifest"
    if ($LASTEXITCODE -ne 0) { throw "manifest signing failed" }
    $manifestHash = (Get-FileHash $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Utf8File (Join-Path $output "SHA256SUMS.txt") "$zipHash  $zipName`r`n$manifestHash  latest.json`r`n"
    Write-Host "Signed update output: $output"
}
finally { Pop-Location }
