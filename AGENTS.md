# AGENTS.md — Halcyon（仓库：weyham/halcyon）

> 给在本项目工作的 Agent（Codex 等）与人类协作者的强制规范。
> 项目是什么、怎么用，见 [README.md](README.md)；本文件只写**规则与纪律**。

## 1. 项目速览

- 常驻托盘代理：Tauri 2 + Rust + React（面板），默认监听 `127.0.0.1:8788`；
- 职责一：请求出口实时改写六类 Responses 非规范条目（孤儿注入 / agent_message /
  旧搜索形状 / 工具回合异常 / 空文本与空 reasoning / 其余透传）；
- 职责二：任务修复——离线修复模型 / 目录失效 / 血缘回填三类元数据 +
  模型切换自动修复（经官方 app-server `thread/resume`）；
- 平台：Windows x64（便携 + Velopack 安装包）、macOS universal（便携，未签名）；
- 工作区布局：`core/`（纯逻辑 crate，测试在此）、`src-tauri/`（应用壳）、
  `ui/`（React 面板）、`updater/`（更新助手）、`tools/halcyon-sign/`（发布签名工具）。

## 2. 硬性规则（最高优先级）

1. **不可恢复操作必须先询问用户**：删除（无备份、不被 VCS 管理）、强推、
   历史重写、发布转正等，无论批准模式是什么。
2. **secrets 绝不落文件、进日志、进输出**：核心约束是**透传 Authorization 头**，
   不得读取、记录、缓存或回显用户的 API key 或 token。
   **例外（用户批准）**：托盘余额/额度查询可在**内存中**按路由暂存最近一次请求的
   Authorization 头，仅用于调用该供应商的余额接口；不得持久化、不得进日志、
   不得回显、不得用于其他用途。
3. **原地修改用户文件前**，先在原文件旁建带时间戳的备份。
4. **方案确认原则**：用户未明确表达「执行 / 开工 / 做吧」等执行含义时，
   不视为同意方案；先给方案再动。
5. **更新链 fail closed**：缺公钥、签名失败、SHA-256 不符、降级请求——
   一律不下载、不安装、不降级。

## 3. 安全模型（不得破坏）

- 代理只监听**回环地址**，无身份认证；不要建议把 `listen` 改到非回环地址；
- minisign 签名私钥在仓库之外的本地目录；仓库内只有公钥与变量名；
- 日志只记录路由名、状态码、字节数、改写计数，**不记录请求/响应正文与凭据**；
- 更新检查**零 API**：只走 `releases/latest/download` CDN 路由，不触碰 api.github.com，不携带任何凭据。

## 4. 改写与修复纪律

- 内容形态问题（六类）**只走请求出口实时改写**；正文一个字都不能丢，
  不得改变对话语义；
- **不重写 rollout 的历史 JSONL 行**。离线修复只做三类元数据：
  模型（append-only 权威记录，优先 app-server `thread/resume`）、
  目录失效（writable roots 重定向/移除）、血缘回填（只改偏移字段）；
- 模型修复必须在 **Codex 完全退出**期间执行；每项修复前重新检测进程，
  发现 Codex 启动即中止，剩余项保持待执行；
- 自动修复**只处理有项目归属的线程**；队列只放待执行项，
  执行结果进「修复记录」页（可搜索、可删除）；
- 队列 / 托盘 / 关于页的状态从同一事实源派生，不得各自维护一套。

## 5. 构建与验证（提交前全绿）

```powershell
pwsh scripts\check.ps1          # 门禁：fmt + 全 workspace 测试 + cargo check + TS
cargo clippy --workspace --all-targets   # 零警告
cargo deny check                 # 依赖许可证 / 安全公告 / 来源
cargo fmt --all
```

- MSRV **1.82**；Node 22；首次构建先 `npm ci --prefix ui`；
- Windows 构建一律 MSVC 工具链（WebView2 静态链接；发布构建加 `/Brepro`
  保证可复现）。本机默认 GNU 工具链仍可开发，但 app crate 的测试 harness
  在 GNU 下起不来，`scripts/check.ps1` 会按 host triple 自动跳过；
  发布相关验证请用 `cargo +stable-x86_64-pc-windows-msvc`。

## 6. 部署与生产纪律

- **生产实例 = `app\halcyon.exe`**（便携版，数据在 `app\data\`），
  它在 Codex 的流量路径上：**不要随手 kill、不要手工 kill + copy 换版**；
- 换版必须走原子脚本：`pwsh app\deploy-dev.ps1 -SourceDir dist\<构建目录>`
  （备份 → 停旧 → 替换 → 启新 → 健康检查 → 失败回滚）；
- 不要删除 `app\data\`（便携标记 + 配置 + 队列 + 修复历史）。

## 7. 提交与署名

- 本仓身份（local 覆盖）：`weyham <weyham@users.noreply.github.com>`；
  **不要改全局 git 身份**（用户的其他项目用它）；
- commit message：中文简述 + 涉及模块，粒度小、不混无关改动；
- **不把本机路径、内部项目名、真实线程 id 写进代码、测试或文档**；
- 不强推 main；tag 与已发布资产不可移动或覆盖。

## 8. 术语（强制）

- 应用名 **Halcyon**；说**代理**，不说「垫片」；
- 说**自动修复**，不用「看门狗」；
- `internal-docs/` = 项目内**独立 git 仓**（过程性文档与运维脚本），
  已被主仓 `.gitignore` 排除，任何内容不得进入公开仓。

## 9. 文档纪律

- 公开文档（README / docs\*）只写**结果性描述**：不带开发日期、事故叙事、
  内部编号、内部项目名；
- 规则变更同步 `docs/design.md`；逐条实测证据、重要决策（ADR 格式）记入
  `internal-docs/`（verification.md / decision-log.md）；
- 版本变更同步 `CHANGELOG.md` 与全部版本位（用 `scripts/set-version.ps1`，
  会校验 7 处一致性）。

## 10. 发布

- 完整门禁与资产清单见 [`docs/release-protocol.md`](docs/release-protocol.md)
  （正式 Release = 15 项资产，缺一即 fail closed）；
- 流程：推 tag → `publish-draft.ps1` 派发 CI → 验收 Draft →
  `finalize-release.ps1 -Publish` 转正；
- 正式发布后 tag 与资产不可移动或覆盖。
