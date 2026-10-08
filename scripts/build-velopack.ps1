[CmdletBinding()]
param([switch]$SkipBuild, [string]$OutputDirectory = "", [string]$FromBuild = "", [switch]$SkipPortableOverride)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")
$root = Get-HalcyonRoot
Push-Location $root
try {
    $version = Assert-HalcyonVersionConsistency

    # Ensure vpk CLI is available（幂等：装了就用/升级，没装就安装）。
    # 不能只写 `dotnet tool update -g vpk`：在从未装过 vpk 的机器上（例如全新的
    # 自托管 runner）update 会直接失败，CI 第一次发布就断在这一步（2026-10-06 审计）。
    if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
        throw 'dotnet CLI not found; vpk (Velopack CLI) needs the .NET tool runtime'
    }
    $vpk = Get-Command vpk -ErrorAction SilentlyContinue
    if (-not $vpk) {
        # 已安装但不在 PATH 上的情况（CI 常见）：先补 PATH，避免无谓重装。
        $toolDir = Join-Path $env:USERPROFILE '.dotnet\tools'
        $vpkExe = Join-Path $toolDir 'vpk.exe'
        if (Test-Path -LiteralPath $vpkExe) {
            $env:PATH = "$toolDir;$env:PATH"
        } else {
            Write-Host "Installing vpk CLI globally..."
            dotnet tool install -g vpk
            if ($LASTEXITCODE -ne 0) {
                Write-Host "Install failed; retrying with 'dotnet tool update -g vpk'..."
                dotnet tool update -g vpk
            }
            if ($LASTEXITCODE -ne 0) { throw "vpk installation failed" }
            $env:PATH = "$toolDir;$env:PATH"
        }
        $vpk = Get-Command vpk -ErrorAction Stop
    }

    # 一次构建、多通道打包（H2）：统一构建到 target\release-package，并带
    # --no-insert-timestamp（可复现）。这一份产物同时交给 vpk pack 和
    # build-portable.ps1，两个制品里的 halcyon.exe 因此逐字节相同。
    $target = Join-Path $root "target\release-package"
    $build = Join-Path $target "release"
    if ($FromBuild) {
        # 复用别的通道已经构建好的产物（CI 里与自研链 portable/update 共用同一次构建）
        $build = (Resolve-Path -LiteralPath $FromBuild).Path
        Write-Host "Reusing existing build output: $build"
    } elseif (-not $SkipBuild) {
        $env:CARGO_TARGET_DIR = $target
        $env:RUSTFLAGS = "-C link-arg=-Wl,--no-insert-timestamp"
        Write-Host "Building UI..."
        npm --prefix ui run build
        if ($LASTEXITCODE -ne 0) { throw "UI build failed" }

        Write-Host "Building Halcyon release exe..."
        cargo build --release -p halcyon-app --features custom-protocol
        if ($LASTEXITCODE -ne 0) { throw "Halcyon release build failed" }
    }

    # Stage directory for vpk pack
    $stage = Join-Path $env:TEMP "halcyon-velopack-stage-$version"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $stage | Out-Null

    $exe = Join-Path $build "halcyon.exe"
    $dll = Get-ChildItem $build -Recurse -Filter WebView2Loader.dll | Select-Object -First 1
    $icon = Join-Path $root "src-tauri\icons\icon.ico"

    if (-not (Test-Path $exe)) { throw "halcyon.exe not found at $exe" }
    if (-not $dll) { throw "WebView2Loader.dll not found" }
    if (-not (Test-Path $icon)) { throw "icon.ico not found" }

    Copy-Item $exe -Destination (Join-Path $stage "halcyon.exe")
    Copy-Item $dll.FullName -Destination (Join-Path $stage "WebView2Loader.dll")
    Copy-Item (Join-Path $root "LICENSE") -Destination (Join-Path $stage "LICENSE.txt")
    Write-Utf8File (Join-Path $stage "VERSION.txt") "$version`r`n"
    Write-Utf8File (Join-Path $stage "README.txt") "Halcyon $version - Codex Responses Proxy`r`n`r`nRun Halcyon.exe to start. Configuration is stored in the data directory.`r`n"

    # Output directory
    if (-not $OutputDirectory) {
        $OutputDirectory = Join-Path $root "dist\velopack"
    }
    if (Test-Path $OutputDirectory) { Remove-Item -Recurse -Force $OutputDirectory }
    New-Item -ItemType Directory -Force $OutputDirectory | Out-Null

    # Splash image (UX asset pending; skip with a loud warning when absent).
    # Progress color choice: brand orange #F4622F reads clearly on the dark splash
    # background, so we keep it (no need for the light #F5EEDC fallback here).
    $splash = Join-Path $root "packaging\velopack\splash.png"
    $vpkArgs = @(
        "pack",
        "--packId", "Halcyon",
        "--packVersion", $version,
        "--packDir", $stage,
        "--mainExe", "halcyon.exe",
        "--outputDir", $OutputDirectory,
        "--icon", $icon,
        "--packTitle", "Halcyon",
        "--packAuthors", "weyham",
        "--shortcuts", "StartMenuRoot"
    )
    # Hard requirement: the final splash must exist before packing.
    if (-not (Test-Path -LiteralPath $splash)) {
        throw "Splash image not found: $splash`nRun 'python packaging/velopack/make-splash.py' first to generate it."
    }
    $splashSizeKB = [math]::Round((Get-Item -LiteralPath $splash).Length / 1KB, 0)
    Write-Host "Using splash image: $splash ($splashSizeKB KB)"
    $vpkArgs += @("--splashImage", $splash, "--splashProgressColor", "#F4622F")

    Write-Host "Packing with vpk..."
    & $vpk.Source @vpkArgs
    if ($LASTEXITCODE -ne 0) { throw "vpk pack failed" }

    # --- Portable override -----------------------------------------------
    # vpk's default *-win-Portable.zip uses the Velopack portable layout
    # (.portable + current\), which has no data\ marker and places the real exe
    # under current\ (replaced on update). That is NOT our portable behavior.
    # We overwrite it with our own build-portable.ps1 zip, keeping the vpk
    # filename so `vpk upload` still finds the expected asset name.
    $vpkPortable = Join-Path $OutputDirectory "Halcyon-win-Portable.zip"
    $beforeSize = if (Test-Path -LiteralPath $vpkPortable) { (Get-Item -LiteralPath $vpkPortable).Length } else { 0 }

    $afterSize = $beforeSize
    if ($SkipPortableOverride) {
        # CI 的发布 job 自己产出便携通道 zip，这里只取安装通道资产，
        # 不需要再用自研 portable 覆盖 vpk 的默认 portable。
        Write-Host "SkipPortableOverride: 保留 vpk 默认 portable，不做自研覆盖"
    } else {
        # Unique temp dir per run -> repeated runs never collide on a leftover
        # directory/zip from the previous run.
        $runId = [guid]::NewGuid().ToString("N")
        $portableOut = Join-Path $env:TEMP "halcyon-portable-override-$version-$runId"
        $ourZip = "$portableOut.zip"
        Write-Host "Building our own portable package for override (temp: $portableOut)..."
        try {
            & (Join-Path $PSScriptRoot "build-portable.ps1") -SkipChecks -FromBuild $build -OutputDirectory $portableOut
            if ($LASTEXITCODE -ne 0) { throw "build-portable.ps1 failed; portable override aborted" }
            if (-not (Test-Path -LiteralPath $ourZip)) { throw "own portable zip not found: $ourZip" }

            # Overwrite the vpk default portable (target may already exist).
            if (Test-Path -LiteralPath $vpkPortable) { Remove-Item -LiteralPath $vpkPortable -Force }
            Copy-Item -LiteralPath $ourZip -Destination $vpkPortable -Force
            Write-Host "已用自研 portable 覆盖 vpk 默认 portable: $vpkPortable"
        } finally {
            if (Test-Path -LiteralPath $portableOut) { Remove-Item -Recurse -Force $portableOut }
            if (Test-Path -LiteralPath $ourZip) { Remove-Item -LiteralPath $ourZip -Force }
        }

        # Verify the override really carries our data\ marker.
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [IO.Compression.ZipFile]::OpenRead($vpkPortable)
        try {
            $dataEntries = @($zip.Entries | Where-Object { $_.FullName -like "data/*" })
            if ($dataEntries.Count -eq 0) { throw "Overridden portable zip is missing data/ entries" }
            Write-Host "Portable override data/ entries: $($dataEntries.Count) (OK)"
        } finally { $zip.Dispose() }

        $afterSize = (Get-Item -LiteralPath $vpkPortable).Length
    }

    # List output files
    $files = Get-ChildItem $OutputDirectory -File | Sort-Object Name
    Write-Host "`nVelopack output:"
    foreach ($f in $files) {
        $sizeKB = [math]::Round($f.Length / 1024, 0)
        Write-Host "  $($f.Name) - $sizeKB KB"
    }
    Write-Host "`nPortable override size: before=$([math]::Round($beforeSize/1KB,0)) KB after=$([math]::Round($afterSize/1KB,0)) KB"
    $setupExe = $files | Where-Object Name -like "*-win-Setup.exe" | Select-Object -First 1
    if (-not $setupExe) { throw "Setup.exe not found in output" }
    Write-Host "Setup.exe: $($setupExe.FullName)"
    Write-Host "Do NOT install this Setup.exe in CI/dev; leave for user manual acceptance."
} finally {
    Pop-Location
}
