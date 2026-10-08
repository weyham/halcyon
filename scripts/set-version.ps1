[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)]
    [ValidatePattern('^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')]
    [string]$Version,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$root = Get-HalcyonRoot
$utf8 = [Text.UTF8Encoding]::new($false)

function Update-FirstVersion([string]$RelativePath, [string]$Pattern) {
    $path = Join-Path $root $RelativePath
    $text = [IO.File]::ReadAllText($path, $utf8)
    $match = [regex]::Match($text, $Pattern)
    if (-not $match.Success) { throw "Version field not found: $RelativePath" }
    $replacement = $match.Groups[1].Value + $Version + $match.Groups[2].Value
    if ($DryRun) { Write-Host "Would set $RelativePath to $Version"; return }
    $updated = $text.Remove($match.Index, $match.Length).Insert($match.Index, $replacement)
    [IO.File]::WriteAllText($path, $updated, $utf8)
}

Push-Location $root
try {
    if (-not $DryRun) {
        $dirty = @(git status --porcelain)
        if ($LASTEXITCODE -ne 0 -or $dirty.Count -ne 0) {
            throw 'Working tree must be clean before version changes'
        }
    }
    foreach ($file in @('core\Cargo.toml','src-tauri\Cargo.toml','updater\Cargo.toml','tools\halcyon-sign\Cargo.toml')) {
        Update-FirstVersion $file '(?m)^(version\s*=\s*")[^"]+(")'
    }
    Update-FirstVersion 'src-tauri\tauri.conf.json' '("version"\s*:\s*")[^"]+(")'

    $uiPackage = Get-Content -LiteralPath 'ui\package.json' -Raw | ConvertFrom-Json
    if ($DryRun) { Write-Host "Would set ui/package.json and ui/package-lock.json to $Version" }
    elseif ($uiPackage.version -ne $Version) {
        npm --prefix ui version $Version --no-git-tag-version
        if ($LASTEXITCODE -ne 0) { throw 'npm version failed' }
    }

    if ($DryRun) { Write-Host "Would prepare CHANGELOG.md section for $Version" }
    else {
        $changelog = Join-Path $root 'CHANGELOG.md'
        $text = [IO.File]::ReadAllText($changelog, $utf8)
        if ($text -notmatch [regex]::Escape("## [$Version]")) {
            $marker = '## [Unreleased]'
            if (-not $text.Contains($marker)) { throw 'CHANGELOG.md missing Unreleased section' }
            # User documentation gets a timestamped adjacent backup before in-place modification.
            $backup = "$changelog.bak-$(Get-Date -Format yyyyMMdd-HHmmss)"
            Copy-Item -LiteralPath $changelog -Destination $backup
            $text = $text.Replace($marker, "$marker`r`n`r`n## [$Version] - $(Get-Date -Format yyyy-MM-dd)")
            [IO.File]::WriteAllText($changelog, $text, $utf8)
        }
        cargo check --workspace --offline
        if ($LASTEXITCODE -ne 0) { throw 'Cargo version/lock synchronization failed' }
        & (Join-Path $PSScriptRoot 'check-version.ps1')
    }
}
finally { Pop-Location }
