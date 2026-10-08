[CmdletBinding()]
param([switch]$SkipChecks, [switch]$SingleJob, [string]$OutputDirectory = "")
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")
$root = Get-HalcyonRoot
Push-Location $root
try {
    Assert-HalcyonBuildSecrets
    $version = Assert-HalcyonVersionConsistency
    if (-not $SkipChecks) { & (Join-Path $PSScriptRoot "check.ps1") }
    $target = Join-Path $root "target\update-package"
    $env:CARGO_TARGET_DIR = $target
    if ($SingleJob) { $env:CARGO_BUILD_JOBS = "1" }
    # MSVC link.exe /Brepro：确定性输出（PE 时间戳由内容哈希派生），保证可复现构建
    $env:RUSTFLAGS = "-C link-arg=/Brepro"
    npm --prefix ui run build
    if ($LASTEXITCODE -ne 0) { throw "UI build failed" }
    cargo build --release -p halcyon-app --features custom-protocol
    if ($LASTEXITCODE -ne 0) { throw "Halcyon release build failed" }
    cargo build --release -p halcyon-updater
    if ($LASTEXITCODE -ne 0) { throw "Updater release build failed" }
    $build = Join-Path $target "release"
    $exe = Join-Path $build "halcyon.exe"
    $helper = Join-Path $build "halcyon-updater.exe"
    if (-not (Test-Path -LiteralPath $exe)) { throw "halcyon.exe not found" }
    if (-not (Test-Path -LiteralPath $helper)) { throw "halcyon-updater.exe not found" }
    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $root "dist\halcyon-v$version-windows-x64-update" }
    if (Test-Path -LiteralPath $OutputDirectory) { throw "Output directory already exists: $OutputDirectory" }
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    Copy-Item -LiteralPath $exe -Destination (Join-Path $OutputDirectory "halcyon.exe")
    Copy-Item -LiteralPath $helper -Destination (Join-Path $OutputDirectory "halcyon-updater.exe")
    Write-Utf8File (Join-Path $OutputDirectory "VERSION.txt") "$version`r`n"
    $zip = "$OutputDirectory.zip"
    Compress-Archive -Path (Join-Path $OutputDirectory "*") -DestinationPath $zip
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Utf8File "$zip.sha256" "$hash  $([IO.Path]::GetFileName($zip))`r`n"
    Write-Host "Update archive: $zip"
}
finally { Pop-Location }
