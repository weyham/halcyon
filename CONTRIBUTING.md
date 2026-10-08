# 参与开发

## 环境准备

- Rust（stable；Windows 用 MSVC 或 GNU 工具链均可，macOS 自带 clang）
- Node.js 22+
- Windows 需 WebView2 Runtime（Win10/11 一般自带）

## 常用命令

```powershell
npm ci --prefix ui             # 首次安装前端依赖
pwsh -File scripts\check.ps1  # 门禁：fmt + 全 workspace 测试 + TS 检查（提交前必须全绿）
cargo test -p halcyon-core     # 核心测试
npm --prefix ui run dev        # 开发时前端热更新
cargo run                      # 开发构建运行（托盘 + 面板）
```

## 提交前

- `scripts\check.ps1` 必须全绿；`cargo fmt --all` 已格式化
- `cargo deny check`（依赖许可证 / 安全公告 / 来源门禁，需先 `cargo install cargo-deny --locked`）
- 改动改写规则时，同步更新 `docs/design.md`（实测证据按维护流程内部归档）
- 提交粒度小、范围单一；commit message 用中文简述 + 涉及模块
- UI 改动请附一张截图

## 不要做的事

- 不要读取、记录、缓存或回显任何 API key 或 token（本项目的硬约束是只透传）
- 不要改写正常配对的工具调用链路
- 不要让解析失败演变成请求失败，一律退化为原样透传 + 记日志
- 不要把本机路径、内部项目名、真实线程 id 写进代码、测试或文档

## 报告问题

用 Issue 模板，附**最小报文**（脱敏）与上游类型；不要粘贴真实密钥或对话内容。
安全相关问题请走私密通道，见 [SECURITY.md](SECURITY.md)。
