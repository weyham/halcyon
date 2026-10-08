function Write-Utf8File([string]$Path, [string]$Content) {
    $directory = Split-Path -Parent $Path
    if ($directory) { New-Item -ItemType Directory -Path $directory -Force | Out-Null }
    [System.IO.File]::WriteAllText($Path, $Content, [System.Text.UTF8Encoding]::new($false))
}

function Get-HalcyonRoot { return (Resolve-Path (Join-Path $PSScriptRoot "..")).Path }

function Read-HalcyonVersion {
    $root = Get-HalcyonRoot
    $text = [System.IO.File]::ReadAllText((Join-Path $root "core\Cargo.toml"), [System.Text.UTF8Encoding]::new($false))
    $match = [regex]::Match($text, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) { throw "Halcyon version not found in core/Cargo.toml" }
    return $match.Groups[1].Value
}

function Assert-HalcyonBuildSecrets {
    if ([string]::IsNullOrWhiteSpace($env:HALCYON_UPDATE_PUBLIC_KEY)) { throw "Missing HALCYON_UPDATE_PUBLIC_KEY" }
}

function Assert-HalcyonVersionConsistency {
    $root = Get-HalcyonRoot
    $expected = Read-HalcyonVersion
    foreach ($file in @("src-tauri\Cargo.toml", "updater\Cargo.toml", "tools\halcyon-sign\Cargo.toml")) {
        $text = [System.IO.File]::ReadAllText((Join-Path $root $file), [System.Text.UTF8Encoding]::new($false))
        $match = [regex]::Match($text, '(?m)^version\s*=\s*"([^"]+)"')
        if (-not $match.Success -or $match.Groups[1].Value -ne $expected) { throw "Version mismatch in $file" }
    }
    $tauri = Get-Content -LiteralPath (Join-Path $root "src-tauri\tauri.conf.json") -Raw -Encoding UTF8 | ConvertFrom-Json
    if ([string]$tauri.version -ne $expected) { throw "Version mismatch in src-tauri/tauri.conf.json" }
    foreach ($file in @("ui\package.json", "ui\package-lock.json")) {
        $package = Get-Content -LiteralPath (Join-Path $root $file) -Raw -Encoding UTF8 | ConvertFrom-Json -AsHashtable
        $rootPackage = if ($file -like '*lock.json') { $package['packages'][''] } else { $null }
        if ([string]$package['version'] -ne $expected -or
            ($file -like '*lock.json' -and [string]$rootPackage['version'] -ne $expected)) {
            throw "Version mismatch in $file"
        }
    }
    Write-Host "Halcyon version: $expected"
    return $expected
}
