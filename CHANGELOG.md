# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 与[语义化版本](https://semver.org/lang/zh-CN/)。

## [1.0.4] - 2026-10-14

### 修复

- **路由导出不可用**：WebView2 会静默拦截 `Blob` / `<a download>` 式下载，
  导致「导出」点击后无任何反应。改为 Rust 原生保存对话框 + 直接写盘，
  导出成功后显示落盘完整路径。v1.0.3 的路由导出功能因此从未可用，本版修复。

## [1.0.3] - 2026-10-14

### 新增

- 设置页路由映射支持**导入/导出**：导出为带版本号的 JSON 文件
  （含上游、别名、web_search、用量查询配置；不含 API key，可安全分享）；
  导入时校验格式与地址合法性，同名覆盖、新名追加，导入后仍需手动保存生效。

## [1.0.2] - 2026-10-14

### 新增

- **超大图片条目瘦身**：请求体超过阈值（默认 80 MB）时，`function_call_output` 中的
  `input_image`（`view_image` 工具的 base64 结果）按时间从旧到新替换为占位文本，
  保留最近 5 张。修复 Kimi nginx `413 Payload Too Large`（实测 105.8 MB 请求体）。
  配置：`image_slim_threshold_mb` / `image_slim_keep_recent`。
- 请求日志与 `/health` 新增 `img_slim` / `images_slimmed` 计数。

### 修复

- macOS CI 上 server_test 真 TCP 集成测试偶发 RST/EOF（端口回收 + TIME_WAIT 元组冲突）：
  echo 上游优雅关闭 + 测试互斥串行化。

## [1.0.1] - 2026-10-08

### 变更

- 更新链路零 API 化：更新检查与下载全部走 `releases/latest/download` CDN 路由，
  不再触碰 api.github.com，根除匿名限额导致的「GitHub 请求被限流」；
  清单平台条目新增 `url` 字段（`assetId` 保留给 1.0.0 客户端）。
- Windows 构建切换 MSVC 工具链：WebView2 静态链接，便携包/安装包/更新包
  不再携带 `WebView2Loader.dll`；可复现性由 `/Brepro` 保证。
- 清单未包含本平台键（该平台本次未发布）时按「无更新」处理，不再报错。

### 移除

- `update.include_prerelease` 配置项：零 API 源只读 Latest Release，天然不含
  预发布；旧配置中的该字段会被兼容忽略。

### 内部

- 测试与发布脚本的 `%TEMP%` 产物自清理（成功即删、失败保留现场）。

## [1.0.0] - 2026-10-07

首个公开发布版本。核心能力：

- 本地回环代理（默认 `127.0.0.1:8788`）：六类 Responses 非规范条目在请求出口实时改写
  （孤儿注入、`agent_message`、旧搜索形状、工具回合异常、空文本 / 空 reasoning、
  其余字节透明转发）；
- 托盘红绿灯状态、统计与用量快照、余额/额度查询（声明式规则引擎）；
- 任务修复：统一扫描（深链指定 / 全量），离线修复模型 / 目录失效 / 血缘回填三类元数据，
  结果记录可搜索、可单条删除、可清空；
- 模型切换自动修复：监听默认模型变化，Codex 退出后经官方 app-server 路径修复，
  任务栏上方进度卡片 + 完成通知；
- Windows 便携版与安装包（应用内 minisign 签名更新）、macOS 便携版（未签名，首次右键打开）；
- 自动更新：启动 60 秒首查、之后每 3 小时静默检查；发现更新即静默下载校验，
  托盘菜单 / tooltip / 侧边栏「关于」红点提示，安装始终由用户手动触发。
