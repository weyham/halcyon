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
    $env:RUSTFLAGS = "-C link-arg=-Wl,--no-insert-timestamp"
    npm --prefix ui run build
    if ($LASTEXITCODE -ne 0) { throw "UI build failed" }
    cargo build --release -p halcyon-app --features custom-protocol
    if ($LASTEXITCODE -ne 0) { throw "Halcyon release build failed" }
    cargo build --release -p halcyon-updater
    if ($LASTEXITCODE -ne 0) { throw "Updater release build failed" }
    $build = Join-Path $target "release"
    $exe = Join-Path $build "halcyon.exe"
    $helper = Join-Path $build "halcyon-updater.exe"
    $dll = Get-ChildItem -LiteralPath $build -Recurse -Filter WebView2Loader.dll | Select-Object -First 1
    if (-not (Test-Path -LiteralPath $exe)) { throw "halcyon.exe not found" }
    if (-not (Test-Path -LiteralPath $helper)) { throw "halcyon-updater.exe not found" }
    if (-not $dll) { throw "WebView2Loader.dll not found" }
    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $root "dist\halcyon-v$version-windows-x64-update" }
    if (Test-Path -LiteralPath $OutputDirectory) { throw "Output directory already exists: $OutputDirectory" }
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    Copy-Item -LiteralPath $exe -Destination (Join-Path $OutputDirectory "halcyon.exe")
    Copy-Item -LiteralPath $helper -Destination (Join-Path $OutputDirectory "halcyon-updater.exe")
    Copy-Item -LiteralPath $dll.FullName -Destination (Join-Path $OutputDirectory "WebView2Loader.dll")
    Write-Utf8File (Join-Path $OutputDirectory "VERSION.txt") "$version`r`n"
    $zip = "$OutputDirectory.zip"
    Compress-Archive -Path (Join-Path $OutputDirectory "*") -DestinationPath $zip
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Utf8File "$zip.sha256" "$hash  $([IO.Path]::GetFileName($zip))`r`n"
    Write-Host "Update archive: $zip"
}
finally { Pop-Location }
