# Halcyon CI 发布

## Workflow

- `build.yml`：PR、main push 和手动运行；只做 UI 构建、核心测试和 workspace check，不接触签名材料。
- `release.yml`：只接受手动指定的已存在 `v*` tag，并有一个 `platforms` 输入
  （默认 `windows`，即 **Windows-only**；显式传 `windows,macos` 才走双平台）。
  构建 Windows 发布包（选了 macos 才构建 macOS `.app`），然后进入 `release`
  Environment，由签名 job 生成 update 资产、`latest.json` 和 Draft Release。

## GitHub 配置

在仓库设置中创建 Environment：

```text
release
```

当前仓库套餐不支持 Environment Required Reviewers，因此不让 tag 自动触发签名发布；
手动启动 workflow 本身就是签名发布门禁。若未来升级套餐，再增加 Required reviewers。
添加公开 Repository Variables：

```text
HALCYON_UPDATE_PUBLIC_KEY
```

添加 Environment Secret：

```text
HALCYON_MINISIGN_PRIVATE_KEY_B64
```

私钥以 base64 形式只用于 `release` Environment 的签名 job；runner 运行时解码到临时文件，
不进入普通构建 job、PR、仓库文件或日志。

## 发布门禁

1. 推送版本 tag；
2. Actions 完成 Windows 构建和测试；普通 `build.yml` 仍检查 macOS 源码；
3. 手动从 Actions 启动 `release.yml`，输入已存在的版本 tag；
4. 签名 job 创建 Draft + Pre-release 并上传资产；
5. 人工验收 Draft；
6. 通过 GitHub UI 将 Draft 转为正式 Release。

Windows portable 包不含 updater；Windows update 包包含 updater。**正式 Release
必须是全套 15 项**（Windows 便携 6 + Windows 安装/Velopack 5 + macOS 4），
门禁定义见 `docs/release-protocol.md`；`platforms` 默认即 `windows,macos`。

Windows job 除自研链两项 zip 外，还会跑一次 Velopack 打包产出安装通道五件
（`Halcyon-win-Setup.exe`、`Halcyon-<version>-full.nupkg`、`RELEASES`、
`releases.win.json`、`assets.win.json`），**与本 job 的 cargo/npm 共用同一次构建**
（`build-velopack.ps1 -FromBuild <dir> -SkipPortableOverride`），并断言 Setup 的
FileVersion/ProductVersion。
紧接着（上传 artifact 之前）调用 `scripts/check-velopack-assets.ps1` 做安装通道的
自洽校验（nupkg 的 Size/SHA256/SHA1、`RELEASES`、`assets.win.json`、Setup 版本号、
nupkg 内 `halcyon.exe` 与 update 包同哈希）。放在构建阶段是有意的：不合格的产物
连 artifact 都不上传，也就不会出现"Draft 建好了却缺件、重跑又被非空 Draft 挡住"。
`vpk` CLI 缺失时脚本会先 `dotnet tool install -g vpk`（旧写法只用 `update`，
在从未装过 vpk 的 runner 上会直接失败）。

macOS job 用 `scripts/build-macos.sh` 产出 `tar.gz` + `dmg`；签名 job 会给两者各出
`.minisig`，并把 `darwin-aarch64` / `darwin-x86_64` 写进 `latest.json`（`manualOnly=true`）。
**当前决定：dmg 按现状发布，不做 Apple 签名/公证**，release notes 写明首次打开方式。

`platforms=windows`（Windows-only）仍保留，用于特殊联调；此时 macOS job 不运行、
`latest.json` 不含任何 `darwin-*` 键。macOS 包为未签 Apple 证书的 .app，首次打开需
右键 → 打开；是否引入 Developer ID 签名与 notarization 待用户决策。
