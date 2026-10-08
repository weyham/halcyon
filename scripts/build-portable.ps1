[CmdletBinding()]
param([switch]$SkipChecks, [switch]$SingleJob, [string]$OutputDirectory = "", [string]$FromBuild = "")
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")
$root = Get-HalcyonRoot
Push-Location $root
try {
    Assert-HalcyonBuildSecrets
    $version = Assert-HalcyonVersionConsistency
    if (-not $SkipChecks) { & (Join-Path $PSScriptRoot "check.ps1") }
    if ($FromBuild) {
        # H2：一次构建、多通道打包。复用别的通道已经构建好的产物，只打包，
        # 不再跑 npm/cargo —— 两个制品里的 halcyon.exe 因此逐字节相同。
        $build = (Resolve-Path -LiteralPath $FromBuild).Path
        Write-Host "Reusing existing build output: $build"
    } else {
        $target = Join-Path $root "target\release-package"
        $env:CARGO_TARGET_DIR = $target
        if ($SingleJob) { $env:CARGO_BUILD_JOBS = "1" }
        # MSVC link.exe /Brepro：确定性输出（PE 时间戳由内容哈希派生），保证可复现构建
        $env:RUSTFLAGS = "-C link-arg=/Brepro"
        npm --prefix ui run build
        if ($LASTEXITCODE -ne 0) { throw "UI build failed" }
        cargo build --release -p halcyon-app --features custom-protocol
        if ($LASTEXITCODE -ne 0) { throw "Halcyon release build failed" }
        $build = Join-Path $target "release"
    }
    $exe = Join-Path $build "halcyon.exe"
    if (-not (Test-Path -LiteralPath $exe)) { throw "halcyon.exe not found" }
    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $root "dist\halcyon-v$version-windows-x64-portable" }
    if (Test-Path -LiteralPath $OutputDirectory) { throw "Output directory already exists: $OutputDirectory" }
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    Copy-Item -LiteralPath $exe -Destination (Join-Path $OutputDirectory "halcyon.exe")
    Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination (Join-Path $OutputDirectory "LICENSE.txt")
    Write-Utf8File (Join-Path $OutputDirectory "VERSION.txt") "$version`r`n"
    Write-Utf8File (Join-Path $OutputDirectory "README-portable.txt") "Halcyon $version Windows x64 portable`r`n`r`nRun halcyon.exe. Runtime config is stored in the data directory (do NOT delete data).`r`n"
    New-Item -ItemType Directory -Path (Join-Path $OutputDirectory "data") -Force | Out-Null
    Write-Utf8File (Join-Path $OutputDirectory "data" "README.txt") "This folder is the portable mode marker. Do NOT delete it."
    $zip = "$OutputDirectory.zip"
    Compress-Archive -Path (Join-Path $OutputDirectory "*") -DestinationPath $zip
    # Verify zip contains data/ entries (Compress-Archive skips empty dirs)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zipVerify = [IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $dataEntries = @($zipVerify.Entries | Where-Object { $_.FullName -like "data/*" })
        if ($dataEntries.Count -eq 0) { throw "Portable zip is missing data/ entries" }
        Write-Host "Zip data/ entries: $($dataEntries.Count) (OK)"
    } finally { $zipVerify.Dispose() }
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Utf8File "$zip.sha256" "$hash  $([IO.Path]::GetFileName($zip))`r`n"
    Write-Host "Portable archive: $zip"
}
finally { Pop-Location }
