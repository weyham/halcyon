# Halcyon 更新链

## 当前状态

- 更新源：**公开 GitHub Releases**，仓库 `weyham/halcyon`。
- **零 API、匿名读取，无需任何授权**：所有读取走 `releases/latest/download` CDN 路由，
  不触碰 api.github.com（不受匿名 API 60 次/小时/IP 限额约束）；应用不保存任何 GitHub 令牌。
  清单平台条目带 `url`（制品 CDN 地址）；`assetId` 仅保留给 1.0.0 旧客户端。
- 校验：Release 清单、制品和 Windows updater helper 均由 minisign 公钥校验，并检查 SHA-256、大小、
  版本降级和 portable 文件白名单。
- Windows：下载后的更新包内携带 `halcyon-updater.exe`；它只在 `app/updates/staging/<version>` 中作为临时 helper
  运行，通过 journal、锁、回滚目录和 readiness marker 执行替换。基础 `app/` 目录不常驻 updater helper。
- macOS：当前只检查并提示手动下载，不执行自动替换。

## 构建变量

构建时注入（只有验签公钥）：

```powershell
$env:HALCYON_UPDATE_PUBLIC_KEY = Get-Content -Raw <本地公钥文件路径>
cargo build --release --features custom-protocol -p halcyon-app -p halcyon-updater
```

发布签名私钥只用于发布，保存在**仓库之外的本地目录**，不得入库、入日志或随应用分发。

## Release 资产约定

每个 Release 需要包含：

- `latest.json`：签名清单；
- `latest.json.minisig`：清单签名；
- 清单中声明的平台制品、SHA-256、大小和 Windows helper；
- Windows portable 包包含 `halcyon.exe`、`WebView2Loader.dll`、配置之外的可替换程序文件和 `halcyon-updater.exe`；
- 配置、日志和系统凭据不进入 Release 制品。

没有公钥、签名清单或校验失败时，Halcyon fail closed，不下载、不安装、不降级。
