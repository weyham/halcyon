[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][ValidatePattern('^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')][string]$Tag,
    [string]$Repository = 'weyham/halcyon',
    # K2：默认发全套（Windows 便携 + 安装包 + macOS tar.gz/dmg）；'windows' 仅供特殊联调
    [ValidateSet('windows', 'windows,macos')][string]$Platforms = 'windows,macos',
    [switch]$Dispatch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'common.ps1')
$root = Get-HalcyonRoot
Push-Location $root
try {
    $version = Assert-HalcyonVersionConsistency
    if ($Tag -ne "v$version") { throw "Tag $Tag does not match source version $version" }

    $existing = @(& gh release view $Tag --repo $Repository --json isDraft 2>&1)
    if ($LASTEXITCODE -eq 0) { throw "Release $Tag already exists; refusing to replace its assets" }
    if (($existing -join ' ') -notmatch '(?i)(release not found|HTTP 404|not found)') {
        throw "Cannot confirm release absence: $($existing -join ' ')"
    }

    $tagCommit = & git rev-parse "refs/tags/$Tag^{}" 2>&1
    if ($LASTEXITCODE -ne 0 -or @($tagCommit).Count -ne 1) { throw "Local tag $Tag is missing" }
    $remoteTag = @(& git ls-remote origin "refs/tags/$Tag")
    if ($LASTEXITCODE -ne 0 -or $remoteTag.Count -ne 1) { throw "Remote tag $Tag is missing or ambiguous" }
    $localRef = & git rev-parse "refs/tags/$Tag"
    if ($LASTEXITCODE -ne 0 -or $remoteTag[0].Split("`t")[0] -ne $localRef) {
        throw "Local and remote tag $Tag differ"
    }
    & git merge-base --is-ancestor $tagCommit origin/main
    if ($LASTEXITCODE -ne 0) { throw "Tag $Tag is not reachable from origin/main" }

    if (-not $Dispatch) {
        Write-Host "Dry run: $Tag is ready (platforms=$Platforms). Use -Dispatch to trigger the release workflow, which creates a Draft."
        return
    }
    $dirty = @(git status --porcelain)
    if ($LASTEXITCODE -ne 0 -or $dirty.Count) { throw 'Working tree must be clean before dispatch' }
    & gh workflow run release.yml --repo $Repository --ref main -f "tag=$Tag" -f "platforms=$Platforms"
    if ($LASTEXITCODE -ne 0) { throw "Failed to dispatch release workflow for $Tag" }
    Write-Host "Release workflow dispatched for $Tag (platforms=$Platforms); verify the Draft before finalizing."
} finally { Pop-Location }
