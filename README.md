# Halcyon

[![CI](https://github.com/weyham/halcyon/actions/workflows/build.yml/badge.svg)](https://github.com/weyham/halcyon/actions/workflows/build.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/weyham/halcyon)](https://github.com/weyham/halcyon/releases)

> App 显示名：**Halcyon**（托盘与窗口所见即此名）

一个常驻托盘的本地 HTTP 代理（原生应用，Tauri 2 / Rust）：把 Codex 发出的**非规范 Responses 条目**，
在请求发出前改写成严格上游能接受的形态。

用来解决「跨任务消息 / 自动化注入 / 换 provider 后旧搜索记录」导致**整个任务被 400 打挂**的问题。

---

## 1. 要解决的问题

Codex（Desktop / CLI）对**不支持服务端存储链的 provider**（DeepSeek / Kimi / GLM 等第三方
Responses 兼容端点），每条请求都会把**整个上下文窗口**全量回放；`disable_response_storage`
只影响 OpenAI 官方端点的行为，对 custom provider 是死参数。
app-server 会注入一种内部扩展形态：带 `name` / `namespace`，但**没有 `call_id`、也没有配对
`function_call`** 的独立 `function_call_output`（跨任务消息、heartbeat/cron 自动化、
`create_thread`、`/btw` 后台通知都走它）。

- **OpenAI 官方端点接受**这个形态（它是官方 app-server 的扩展）；
- **严格实现公开 Responses 规范的端点会拒绝整个请求**：
  - DeepSeek 原生 `/responses`：`Failed to deserialize the JSON body ... missing field 'call_id'`
  - Kimi：`Invalid request: tool_call_id is not found`
- 坏条目会被**写进 rollout**，于是该任务此后**每一条消息都失败**。

同源问题还有：换 provider 后回放上一个 provider 写下的 `web_search_call`（`action.query`），
而 DeepSeek 要求 `action.queries` 数组 + 前置 reasoning 条目；多智能体消息
（`agent_message`）同样不被第三方网关识别——都是整包 400。

社区讨论（均为 openai/codex 公开 issue）：
[#42067](https://github.com/openai/codex/issues/42067)、
[#45227](https://github.com/openai/codex/issues/45227)、
[#45318](https://github.com/openai/codex/issues/45318)、
[#45450](https://github.com/openai/codex/issues/45450)、
[#44723](https://github.com/openai/codex/issues/44723)。

## 2. 方案：只修请求边界

```
Codex ──请求──▶ Halcyon(127.0.0.1:8788/<route>) ──▶ 真实上游（api.deepseek.com / api.kimi.com/...）
                    │
                    ├─ ① 缺 call_id 的孤儿注入条目 → 改写成带来源标注的 user 消息（正文逐字保留）
                    ├─ ② agent_message（多智能体汇报）→ 改写成等价 assistant 消息（正文逐字保留）
                    ├─ ③ 旧形状 web_search_call → 默认改写成一条「历史记录」说明
                    ├─ ④ 工具回合中间夹层 / 缺 output → 重排 pairing 或补 aborted
                    ├─ ⑤ 空文本消息 / 空 reasoning → 整条丢弃（零语义损失）
                    └─ ⑥ 其余字节透明转发；Authorization 头透传（不读取、不记录、不落盘）
```

六类改写都在**请求出口**做：会话历史、rollout 文件、CC Switch 数据库一律不动。
对话内容形态问题只走出口实时改写，**不做离线文件改写**——rollout 是 Codex 的事实源，
破坏性改写会引发血缘偏移失效、整个任务无法打开。
改写规则、设计细节见 [`docs/design.md`](docs/design.md)。

改写后的来源标注长这样，接收方仍能知道消息是谁发的、并能用深链回看：

```
[跨任务消息 · via=send_message_to_thread · namespace=codex_app · source_thread=01a0abcd · link=codex://threads/01a0abcd]
```

## 3. 安装与启动

绿色便携，无需安装器、不写注册表（开机启动走「启动」文件夹快捷方式，可在托盘右键开关）：

1. 从 [Releases](../../releases) 下载对应平台压缩包、解压到任意目录
   （Windows 另提供安装包 `Halcyon-win-Setup.exe`，安装版数据在 `%APPDATA%\Halcyon`）；
2. 配置放 exe 旁的 `data\config.json`（首次运行自动建好 `data\`；参考
   [`config.example.json`](config.example.json)）；macOS 放
   `~/Library/Application Support/Halcyon/config.json`；
3. 双击 `halcyon.exe`（macOS 为 `Halcyon.app`，未签名，首次右键 → 打开）——
   托盘出现红绿灯即启动完成。
4. **升级时保留 `data\` 目录**（既是便携标记，也放着配置、自动修复队列与修复历史）。

## 4. 托盘与设置

托盘红绿灯：

| 颜色 | 含义 |
| --- | --- |
| 绿 | 运行中且已有流量经过（有流量时图标明暗交替） |
| 黄 | 运行中但无流量：启动以来零流量，或持续 60 秒闲置（tooltip 显示原因） |
| 红 | 服务未运行（监听失败 / 配置错误） |
| 蓝 | 正在自动修复（tooltip 显示进度；任务栏上方有进度卡片） |
| 蓝点 | 正在自动修复（进度见任务栏上方的卡片） |

托盘菜单分两套（都是点击那一刻的快照）：

- **左键**：统计与用量——`请求 N · 改写 N · 上游错误 N`，分隔线下面是各路由的用量快照；
- **右键**：连接信息 + 操作项——监听地址，然后是 **打开面板** / **开机启动**（勾选后登录
  自动启动：Windows 写 `…\Startup\Halcyon.lnk`，macOS 用 LaunchAgent）/ **重启代理** / **退出**。

设置面板（左键双击托盘打开，功能导航：用量 / 修复 / 记录 / 设置 / 关于）：

- **路由映射**：名称 → 上游地址，本地地址自动组合成 `http://127.0.0.1:8788/<名称>`；
  别名让本地地址与上游 base_url 字面等价（如 `/kimi/v1` ≡ `https://api.kimi.com/coding/v1`，
  CC Switch「取模型」直接可用）；改完点「保存并重启代理」；
- **任务修复**：任务报错卡住时，粘贴深链（`codex://threads/…`）或 thread id 只修这一个，
  或扫描全部任务、按项目分组勾选批量修复。面板上的「实时保护 ×N」表示该任务的内容形态
  问题已由代理出口实时改写覆盖（不需要离线修复）；离线修复只做**模型 / 目录失效 /
  血缘回填**三类元数据。结果（成功 / 失败及原因）进「修复记录」页，可搜索、可单条删除、可清空；
- **模型切换自动修复**：检测到 `config.toml` 默认模型变化 → 自动扫描不匹配的任务排队 →
  Codex 完全退出后经官方 app-server 路径自动修复（只动**有项目归属**的任务）；执行期间
  任务栏上方出现进度卡片、托盘图标转蓝点，完成后弹一条通知；
- **自动更新**：启动 60 秒后首查、之后每 3 小时静默检查公开仓 Releases；
  发现更新即静默下载并通过签名校验，托盘菜单 / tooltip / 侧边栏「关于」红点提示，
  安装始终由你手动触发。

## 5. 接入 Codex（CC Switch）

在 CC Switch 里把 Codex 类卡片的「API 请求地址」改成代理地址（API key 不动，代理透传）：

```
https://api.deepseek.com   →   http://127.0.0.1:8788/ds
```

- 只影响 Codex 类卡片；Claude / Gemini 等不受此问题影响；
- 回退 = 把地址改回真实上游，无副作用；
- ⚠️ Codex 桌面 App 只在**启动 / 切 provider 时**读一次配置：改完要重启 App 或切一次 provider。

## 6. 配置

- Windows 便携版：exe 同目录 `data\config.json`
- Windows 安装版：`%APPDATA%\Halcyon\config.json`
- macOS：`~/Library/Application Support/Halcyon/config.json`

```json
{
  "listen": "127.0.0.1:8788",
  "log_level": "info",
  "routes": {
    "ds":    { "upstream": "https://api.deepseek.com" },
    "kimi":  { "upstream": "https://api.kimi.com/coding/v1", "aliases": ["/kimi/v1"] },
    "zhipu": { "upstream": "https://open.bigmodel.cn/api/v1", "aliases": ["/zhipu/v1"] }
  },
  "orphan_header": true
}
```

每条路由可选：`web_search`（`note` 默认 / `drop` / `normalize`）、`web_search_note_max`、`aliases`、
`balance`（余额/额度查询：`{ "template": "deepseek|kimi|glm|custom|none", "url": "...", "custom": {...} }`，
不写则按上游自动识别；供应商改地址时改配置即可，不用等发包；`custom` 为声明式解析规则，
接入全新形状的供应商也只需要配置——设置页每条路由的「用量查询」按钮可视化编辑并即时测试，
规则语法见 `docs/design.md` §9）。

## 7. 验证与测试

```powershell
cargo test -p halcyon-core   # 199 项：改写引擎 / 配置 / 服务集成 / 任务修复 / 自动修复 / 用量查询规则引擎与 e2e
cargo build --release --features custom-protocol
```

核心场景实测：同一份「含孤儿条目 + 旧搜索记录」的报文，DeepSeek 直连 422 / 经代理 200，
Kimi K3 直连 400 / 经代理 200；字节级透传与 SSE 逐块流式有集成测试覆盖。

## 8. 已知限制

- 只改写「Codex 注入类」条目和非法工具回合形状；模型自身正常、顺序合法的
  `function_call` / `function_call_output` 配对**内容不动**；
- 代理是关键路径：它没跑时指向它的请求会连接失败（托盘红绿灯就是用来盯这个的）；
- macOS 产物未签名：首次运行右键 → 打开。

## 9. 开发

```
core/       纯逻辑 crate：改写引擎 / 配置 / HTTP 服务 / 任务修复（无 Tauri 依赖，测试全在这）
src-tauri/  Tauri 应用壳：托盘、设置面板命令、开机自启
ui/         设置面板前端（React + TS + Vite）
updater/    应用内更新助手（随 Windows 更新包分发）
tools/halcyon-sign/  发布签名工具（minisign）
```

```powershell
npm --prefix ui run dev        # 开发时前端热更新
cargo build                    # 开发构建（页面走 vite dev server）
cargo test -p halcyon-core
cargo deny check               # 依赖许可证 / 安全公告 / 来源门禁
```

CI（GitHub Actions）：`build.yml` 做测试与检查，`release.yml` 出正式发布资产。
发布资产的门禁与签名流程见 [`docs/release-protocol.md`](docs/release-protocol.md)。

提交规范见 [`CONTRIBUTING.md`](CONTRIBUTING.md)，版本历史见 [`CHANGELOG.md`](CHANGELOG.md)。

## 许可

MIT，见 [`LICENSE`](LICENSE)。
