# Weyham Portable Release Protocol v1

本协议定义 Halcyon 的统一桌面发布、签名和应用内更新边界。

## 资产分工

- `*-windows-x64-portable.zip`：首次下载包，不包含 updater、配置、日志、token 或缓存。
- `*-windows-x64-update.zip`：应用内更新包，包含主程序、运行依赖、`VERSION.txt` 和 updater helper。
- `*-macos-universal.tar.gz`：macOS 手动下载包（universal2，arm64 + x86_64），
  `manualOnly=true`，不自动替换；清单中 `darwin-aarch64` 与 `darwin-x86_64`
  两个平台键指向同一资产。内容为 `Halcyon.app` + `config.example.json`。
- `*-macos-universal.dmg`：macOS 拖拽安装包（面向手工安装的用户体验）；
  更新清单仍指向 tar.gz，DMG 不进 `latest.json`。
  **当前决定：按现状发布 dmg——不做 Apple Developer ID
  签名与 notarization**，release notes 必须写明首次打开需右键 → 打开，或
  `xattr -dr com.apple.quarantine Halcyon.app`。是否引入正式签名仍留待后续决策。
- `latest.json` / `latest.json.minisig`：签名更新清单及其 minisign 签名。
- `SHA256SUMS.txt`：发布资产校验记录。

## 正式 Release 的资产门禁

**每个正式 Release 必须是下面 15 项，缺一不可**（`check-release.ps1` /
`finalize-release.ps1` 会逐项校验；缺失或不一致即 fail closed，不许创建或转正）：

```text
Windows 便携通道（6）
  halcyon-<tag>-windows-x64-portable.zip
  halcyon-<tag>-windows-x64-update.zip
  halcyon-<tag>-windows-x64-update.zip.minisig
  latest.json              # 含 windows-x86_64 与 darwin-aarch64 / darwin-x86_64
  latest.json.minisig
  SHA256SUMS.txt
Windows 安装通道（5，Velopack）
  Halcyon-win-Setup.exe
  Halcyon-<version>-full.nupkg
  RELEASES
  releases.win.json
  assets.win.json
macOS（4）
  halcyon-<tag>-macos-universal.tar.gz   + .minisig   # 更新通道用，manualOnly=true
  halcyon-<tag>-macos-universal.dmg      + .minisig   # 手工安装用
```

**交叉自洽校验**（不只看名字存在）：

- `releases.win.json` 里 nupkg 的 `SHA256` / `Size` == 实际上传的 nupkg；
- `RELEASES` 行内 SHA1 == 实际 nupkg 的 SHA1；`assets.win.json` 列出 nupkg 与 Setup；
- `latest.json` 里 update 的 `sha256` / `size` == 实际 update.zip，
  `helper.sha256` / `size` == update.zip 内的 `halcyon-updater.exe`；
- **两个通道的 `halcyon.exe` 必须同哈希**（nupkg 内 `lib/app/halcyon.exe` == update.zip 内
  `halcyon.exe`）——它们在 CI 里共用同一次构建（`CARGO_TARGET_DIR` + no-insert-timestamp）；
- macOS 的两个 `.minisig` 都要通过验签。

`latest.json` 的 Windows 平台资产必须指向 update 包，而不是 portable 包。

**注（`assets.win.json` 与便携包）**：`assets.win.json` 是 vpk 自己生成的资产索引，
里面会引用 `Halcyon-win-Portable.zip`；但发布时**有意不上传那个文件**——Release 里
唯一的便携包是自研链产出的 `halcyon-<tag>-windows-x64-portable.zip`（带 `data\` 标记）。
这样做是为了避免出现「两个行为不同的便携包」。`assets.win.json` 只作为安装通道的
元数据上传，不参与便携通道分发；看到它引用 vpk portable 属于预期，不是漏资产。

## 平台选择（Windows-only 属特例）

`release.yml` 的 `workflow_dispatch` 有 `platforms` 输入：

- `windows,macos`（**默认**）：Windows 11 项（自研链 6 + 安装通道 5）+ macOS 4 项，共 15 项；
- `windows`：只构建/签名/上传 Windows 11 项，macOS job 不运行，`latest.json` 只有
  `windows-x86_64`，`SHA256SUMS.txt` 只含 Windows 资产与清单。**仅供特殊联调**，
  

本地入口 `scripts/publish-draft.ps1` 默认 `-Platforms 'windows,macos'`；
`scripts/sign-release-local.ps1` 在 artifacts 里找不到 macOS 包时自动退化为
Windows-only 清单（但仍强制要求安装通道五件）。`finalize-release.ps1` /
`check-release.ps1` 的期望资产清单由 `latest.json` 自描述（有没有 `darwin-*` 键），
所以两种模式共用同一套校验。

## 安装通道五件的校验点（B6）

同一套自洽校验由 `scripts/check-velopack-assets.ps1` 提供，三个入口共用同一口径：

1. **CI 构建 job**（`release.yml`）：`vpk pack` 之后、上传 artifact 之前就校验——
   不合格的产物连 artifact 都不上传，也不会留下"半填的 Draft"；
2. `scripts/sign-release-local.ps1`：本地签名前校验；
3. `scripts/check-release.ps1`：Draft 验收/转正时再校验一次。

校验内容：`releases.win.json` 的 Version / FileName / Size / SHA256 / SHA1 == 实际
nupkg；`RELEASES` 行内 SHA1 == 实际 nupkg；`assets.win.json` 同时列出 nupkg 与 Setup；
`Setup.exe` 的 FileVersion / ProductVersion == 版本号；nupkg 内 `lib/app/halcyon.exe`
与 update 包内 `halcyon.exe` 逐字节同哈希。


## Latest 归属（正式发布必须抢 Latest）

GitHub 的「Latest」归属在**建稿那一刻**就按当时形态定了：CI 用
`gh release create --draft --prerelease` 建稿，事后只翻转 `draft/prerelease`
**不会**让最新版自动成为 Latest（实测确认翻转 draft/prerelease 不改变 Latest 归属）。

因此约定：**Latest 一定跟随最新正式版**。`scripts/finalize-release.ps1` 转正时显式带
`--latest`（`gh release edit $Tag --draft=false --prerelease=false --latest`）。

这也是自研链（便携版）唯一的版本发现入口：它读 `GET /repos/.../releases/latest`，
Latest 不跟随 = 用户点「检查更新」只会看到「已是最新」。验收专用发布若刻意不抢 Latest
（例如验收发布刻意不抢 Latest），属于**特例**。

## 构建与发布阶段

1. 版本检查和完整测试；
2. Windows/macOS 构建（macOS 经 `scripts/build-macos.sh` 产出 .app 打包）；
3. 生成 Windows portable/update 包与 macOS tar.gz；
4. 人工触发受限的 `release` workflow，签名 job 从 `release` Environment 读取私钥，签名 update 包和 `latest.json`；
5. 创建 Draft Release 并上传全部资产；
6. 人工验收 Draft；
7. 明确批准后将 Draft 转正式 Release。

当前仓库套餐不支持 Environment Required Reviewers，因此人工门禁是手动触发 workflow，
以及验收 Draft 后才转为正式版，而不是 GitHub 自动审批。构建 job 不读取签名私钥；
只有签名 job 可以访问 `HALCYON_MINISIGN_PRIVATE_KEY_B64`。两个项目不共享私钥。

Halcyon 的本地入口是 `scripts/publish-draft.ps1`（默认预演，`-Dispatch` 才派发 CI）和
`scripts/finalize-release.ps1`（默认验签及比对远端资产，`-Publish` 才转换发布状态）。
正式发布后不可移动 tag，也不可覆盖资产；后续发布工具改动只能进入下一版本。

## 更新运行时清理

updater 在 `updates/staging/<version>` 中运行。新版本启动并写入 readiness 后，
主程序负责清理全部更新临时状态：staging、readiness marker、journal、更新锁，
以及本次的 rollback（旧版本随时可从 GitHub Releases 重新下载，本地不保留）。
失败 journal 与其 rollback/staging 保留供人工修复。基础 app 目录不常驻 updater helper。

## 禁止事项

- 不把 `config.json`、系统凭据、日志和用户数据放进 Release 包；
- 不把 minisign 私钥、GitHub token、Client Secret 或 App private key 放进仓库；
- 不允许降级；
- 不让 Pull Request 或普通分支构建访问发布签名环境。
