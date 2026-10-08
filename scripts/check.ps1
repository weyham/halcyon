[CmdletBinding()]
param()
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")
$root = Get-HalcyonRoot
Push-Location $root
try {
    & (Join-Path $PSScriptRoot "check-version.ps1")
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed" }
    # MSVC 下 WebView2 静态链接，app lib 测试可直接运行；GNU 下测试 harness
    # 缺 WebView2Loader.dll 会报 STATUS_ENTRYPOINT_NOT_FOUND，仍跳过 app。
    $hostTriple = ((rustc -vV | Select-String '^host:') -split '\s+')[1]
    if ($hostTriple -like '*-msvc') {
        cargo test --workspace
    } else {
        cargo test --workspace --exclude halcyon-app
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }
    cargo check --workspace
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed" }
    if (-not (Test-Path -LiteralPath "ui\node_modules")) { throw "ui/node_modules missing; run npm ci --prefix ui" }
    & (Join-Path $root "ui\node_modules\.bin\tsc.cmd") -p (Join-Path $root "ui\tsconfig.app.json") --pretty false --tsBuildInfoFile (Join-Path $env:TEMP "halcyon-release-tsbuildinfo")
    if ($LASTEXITCODE -ne 0) { throw "TypeScript check failed" }
    # B5：tsbuildinfo 是临时产物，成功后即清
    Remove-Item (Join-Path $env:TEMP "halcyon-release-tsbuildinfo") -Force -ErrorAction SilentlyContinue
}
finally { Pop-Location }
