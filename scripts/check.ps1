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
    # app lib 测试在本机 windows-gnu 工具链下启动即报 STATUS_ENTRYPOINT_NOT_FOUND
    #（仅含 GUI 依赖栈的测试 harness 如此，core/updater/签名工具不受影响）；
    # 逻辑测试门禁覆盖三个纯逻辑 crate，app 编译由 cargo check --workspace 覆盖。
    cargo test --workspace --exclude halcyon-app
    if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }
    cargo check --workspace
    if ($LASTEXITCODE -ne 0) { throw "cargo check failed" }
    if (-not (Test-Path -LiteralPath "ui\node_modules")) { throw "ui/node_modules missing; run npm ci --prefix ui" }
    & (Join-Path $root "ui\node_modules\.bin\tsc.cmd") -p (Join-Path $root "ui\tsconfig.app.json") --pretty false --tsBuildInfoFile (Join-Path $env:TEMP "halcyon-release-tsbuildinfo")
    if ($LASTEXITCODE -ne 0) { throw "TypeScript check failed" }
}
finally { Pop-Location }
