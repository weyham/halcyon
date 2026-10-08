# 设计说明

## 平台支持

- **Windows x64 是完整支持平台**：首次下载包与应用内更新包独立；更新清单、
  制品签名、SHA-256、updater 回滚和健康探测均有验证。
- **macOS 提供 universal 便携构建**（未签名、未公证，首次打开需右键 → 打开），
  有 CI 编译与服务冒烟；实体 Mac 的完整验收仍在进行中。
## B1 · 桌面面板与托盘快照

- 桌面主面板采用左侧功能导航 + 右侧内容布局；侧栏目标宽度约 210px，右侧内容区
  在可用空间内设置最大宽度并居中，窄窗口下退化为自适应宽度。
- 关于页展示编译版本、固定 GitHub 仓库标识和更新状态。更新检查匿名读取公开仓
  Releases；缺少可信签名公钥时更新安装必须 fail closed。
- 设置保存动作明确表示会重启本地代理；设置 UI 不暴露本地配置路径。
- 托盘菜单保持固定结构。用户点击托盘时，将当前缓存的额度结果写入现有菜单项；
  不在菜单展开期间周期性刷新或重建菜单。额度文本来自解析器实际提供的行，余额与
  周期额度分别显示，缺失值不推导或伪造。
- 托盘用量区是独立区块（前后各一条分隔符），按路由分行：标题 `用量快照 · 路由名`，
  数据行格式 `5h: 30% ⌛1h40m`（小写紧凑时长）或余额型 `余额: ¥12.34`。
  菜单采用「后台哈希刷新 + 打开检测」：refresh_loop 计算内容哈希，变化且菜单
  未打开时重建（Win32 枚举 #32768 菜单窗口判定打开中）；点击处理器不碰菜单，
  图标切换在菜单打开期间也延迟——实测点击时 set_menu 会与菜单弹出竞争导致闪关。
  行数由解析结果驱动（无月度档则不显示该行）。
- 托盘图标为 App 图标主体 + 右上角状态圆点（直径 1/8）：绿=运行、黄=闲置/无流量、
  红=未运行；流量闪烁为绿点亮帧（稳态绿）与暗帧（半透明）每 500ms 交替，无外圈。
  菜单重建哈希只含结构性内容（地址/用量区/勾选）；统计计数行就地 set_text
  更新——它每个请求都变，纳入哈希会让流量期间高频 set_menu 损坏菜单
  （实测右键无响应）。
  只显示「本次启动以来有流量的路由」：服务层按路由记录最近请求时间
  （`Stats.route_last_request_ms`），无流量路由整体不出现。

### B1a · 文件日志可用性

- 桌面版不使用 tauri-plugin-log 内建 `LogDir` 作为文件目标：它缓存 File 句柄且不提供写入
  健康状态，active 路径被外部移动/替换或写入失败时，调用方只能看到日志停更。
- 文件目标改为自研 `TrackedRotatingWriter`：每次刷新先重新打开并校验 active 路径，外部移动
  后会重新创建 `halcyon.log`，不再跟随旧句柄写离线目标；达到 10MiB 先关闭句柄、再重命名为
  `halcyon.log.<epoch_ms>.rotated`；重命名失败时继续写旧文件，不让轮转造成日志中断。
- 主日志路径写失败时立即改写相邻的 `halcyon.log.fallback`；两个路径都失败时仅保留有界内存缓冲。
  写入结果进入 `LogHealth`，`GET /health` 暴露 `log_writable`、当前模式、失败次数与最后错误。
  健康状态不包含日志正文，避免把请求内容或凭据带入接口。

## 1. 背景与边界

Codex 与 provider 之间的**唯一契约是 HTTP 请求体**。本项目的职责只有一件事：

> 请求体里出现「对上游非法、但对 Codex 内部合法」的条目时，把它改写成上游能接受的等价形态，
> 并保证语义不丢（尤其是文本正文）。

不负责：provider 路由策略、密钥管理、用量统计、会话修复脚本（这些属于周边工具）。

## 2. 四类要改写的条目

### 2.1 孤儿注入条目（跨任务消息 / 自动化 / create_thread）

Codex 记录成的形状：

```json
{"type":"function_call_output","id":"fco_...","name":"send_message_to_thread","namespace":"codex_app","output":"<codex_delegation>...</codex_delegation>"}
```

特征：**有 `name`/`namespace`，没有 `call_id`，也没有配对的 `function_call`**。

实测（DeepSeek 原生 Responses）：

| 发送形态 | 结果 |
| --- | --- |
| 原样（无 `call_id`） | 400 `missing field 'call_id'` |
| 补一个 `call_id` 但仍无配对 `function_call` | 400 `No tool call found for tool output with call_id ...` |
| 补 `function_call` + 配对 `call_id` | 200 |
| 改写成普通 `user` 消息 | 200 |

**本项目采用「改写成 user 消息」**：正文（含 `codex_delegation` 信封）逐字保留，
不伪造 assistant 的工具调用，且对「任何只实现公开规范的端点」都成立。

不合成 `function_call` 的原因：那会把一条**不是模型发出**的调用写进历史，改变语义，
且不同实现对「合成调用」的一致性没有保证。

### 2.2 旧形状的 web_search_call

不同 provider 写下的形状不同：

```json
{"type":"web_search_call","action":{"type":"search","query":"..."}}
{"type":"web_search_call","action":{"type":"search","queries":["..."]}}
```

前者是宽松/旧形状，后者是严格上游（DeepSeek）要求；DeepSeek 还要求该条目**前面紧跟一条带
`reasoning_text` 的 reasoning 条目**。实测：只把 `query` 换成 `queries` 仍可能因缺 reasoning 而 400。

结论：对严格上游，`web_search_call` 是**跨 provider 不可移植**的条目。处理策略（配置项
`web_search = "note" | "drop" | "normalize"`）：

1. `note`（默认）：把旧搜索痕迹改写成一条普通 user 消息
   `[历史记录] 本会话早期执行过一次联网搜索：<query>`，语义降到"提过一句"，不会伪造 reasoning；
   同一请求里出现超过 `web_search_note_max`（默认 20）条时**自动退化为 `drop`**，避免刷屏；
2. `drop`：直接移除该条目（搜索结论通常已在后续 assistant 消息里）；
3. `normalize`：`query` → `queries`，并在必要时补最小 reasoning 占位，保留结构化搜索语义。
   实测这条路要求"reasoning 必须带 `reasoning_text`"，合成占位有被上游拒绝的风险，故不做默认。

### 2.3 工具回合中的夹层与缺结果

部分严格上游（已实测 DeepSeek Responses）按「工具回合」校验历史：一个
`function_call` 的匹配 `function_call_output` 必须处于同一个回合内；如果中间夹入
`reasoning`、`message` 等非工具条目，上游会把回合视为已关闭，并报
`No tool output found for tool call <call_id>`，即使 output 实际出现在后面。

另一种历史损坏是 Codex 在工具调用落盘后、结果落盘前被中断，产生只有
`function_call`、没有 `function_call_output` 的条目；后续恢复时客户端会补
`aborted` 占位，但不同版本行为不完全一致。

本项目的处理规则：

- 已有匹配 output，但中间夹入非 `function_call` / `function_call_output` 条目：
  保持所有正文不变，把 output 移动到该 call 之后，夹层条目顺延到 output 之后；
- 单个 `function_call` 没有 matching output：紧跟 call 补一个
  `function_call_output { output: "aborted" }`，稳定 id 由 call_id 派生；
- 正常紧邻 pairing，以及「连续 call + 连续 output」的并行调用顺序不改；
- 规则只在发现上述非法形状时触发，失败时继续按原请求透传并记日志。

### 2.4 空文本条目（空 final_answer 消息与跨 provider 加密 reasoning）

两类「上游读到空 assistant 内容」的历史条目：

1. **空 final_answer 消息**：部分模型返回 `content` 全部文本 part 均为空串的
   assistant 消息（实测产生多条
   `{"content":[{"type":"output_text","text":""}]}` 并落盘）；
2. **跨 provider 加密 reasoning**：历史里可能存在的 `encrypted_content` reasoning
   条目（`content` 为空数组）。密文只有原加密方可解，Kimi / DeepSeek / GLM 读到的是
   空 assistant 内容（实测触发 Kimi 报
   `400 message at position N with role 'assistant' must not be empty`）。

Codex 原样重放历史时，严格上游（Kimi `/responses`）会拒绝整个请求：
`400 Invalid request: text content is empty`（DeepSeek 对空文本宽容，
因此同一份历史换模型后才暴露）。

处理规则：请求出口丢弃

- 「所有 content part 均为文本类型且 text 为空串」的 message 条目（至少含一个 part）；
- content / summary 中没有任何可读文本的 reasoning 条目；

含非文本 part（图片等）或任一非空文本的条目不动。空条目本身不携带任何
对目标上游可读的信息，整条丢弃零语义损失；不合成占位文本，避免编造历史。

### 2.4.2 实时修正与静态修复的分野

**裁定**：对话内容形态问题只走请求出口的实时改写（按路由/上游严格度生效），
不再对 rollout 做静态改写；静态修复只保留元数据类。

**理由**：rollout 是 Codex 的事实源（恢复上下文、分页历史、token 统计）。
破坏性静态修复曾截短分片，导致子分片 `history_base.end_byte_offset` 越界、
Codex 启动校验拒绝加载整个线程；且与 sqlite 投影库不一致、
现场不可取证。

**分野表**：

| 类别 | 规则 | 归属 |
|---|---|---|
| 空文本消息 / 空 reasoning / agent_message / 旧形状搜索 / 孤儿注入 / 工具回合 | 上游兼容性 | 仅实时（出口改写） |
| 模型权威记录追加 | Codex 恢复语义 | 静态（append-only，优先 app-server resume 官方路径） |
| 血缘偏移回填 | Codex 启动校验元数据 | 静态（只改首行偏移字段） |
| 目录失效 roots | 应用配置状态 | 静态（全局状态，备份+原子写） |

**面板语义**：统一扫描的「坏条目」改为「实时保护 ×N」展示（绿色），
仅含坏条目的任务不可勾选、不进修复队列；队列只排元数据类。
旧队列文件中带入的 entries 项执行时不再改写，只记录"实时保护中"。

### 2.4.1 agent_message 归一

Codex 0.160 多智能体（multi-agent v2）把代理间消息以 `response_item`、
type=`agent_message` 落盘（带 author/recipient 协议包装）。重建请求 input 时
原样带上，严格上游（Kimi `/responses` 实测）拒绝整个请求：
`item type "agent_message" is not supported`。

出口改写与离线修复同源第六类规则：`agent_message` → assistant `message`，
content 各文本 part 原样映射为 `output_text`（正文逐字保留；协议信封已完整
复制在正文文本里，不带入上游）。请求日志与 `/health` 计数：`agent_msg` /
`agent_messages_normalized`。

### 2.5 模型不匹配自动改写（已移除）

> **已裁定移除**：模型修正全部走 app-server 离线处理（resume/turn 覆盖，
模型修正不再走请求出口：出口只做上游兼容性改写，改模型由本工具的任务修复完成。
> 以下原文保留作历史背景。

Codex 每个 session 把创建时的模型写死在 rollout（provider 恒为 custom）；
CCS 切换只改 `~/.codex/config.toml` 与 `~/.codex/cc-switch-model-catalog.json`。
旧 session 带旧模型打到新 provider → 400。

出口改写只作为兜底：若 Codex 在客户端就按模型目录拒绝任务，请求不会到达垫片。
要让旧任务恢复可继续，主机制是在 rollout 末尾追加权威 `thread_settings_applied` 记录；不得重写历史。端到端验收也必须以
「修复后 Codex 自身接受并继续该任务」为准，不得只看出口 `model_rewrites` 计数。

处理规则：请求出口在逐条目改写之前，对顶层 `model` 字段做独立改写。

- 读取当前默认模型：`~/.codex/config.toml` 的 `model` 字段；
- 读取可用模型表：`~/.codex/cc-switch-model-catalog.json` 中的 `models[].slug`
  集合（路径从 config.toml 的 `model_catalog_json` 字段解析，相对 `~/.codex/`）；
- 请求体 `model` 在目录中 → 原样透传（不动任何字节）；
- 不在目录中 → 改写为当前默认模型；
- `config.toml` 缺失或没有 `model` → 不启用模型改写（原样透传）。模型目录文件缺失、
  不可解析或为空时不停用：目标模型仍取 `config.toml` 的 `model`，合法集合回退为
  `{默认模型}`；目录可用时使用其 slug 集合并始终包含默认模型。

配置：`config.json` 新增 `model_rewrite: "off" | "auto"`，默认 `auto`；
缺省视为 auto；非法值在配置校验时报错。

缓存：启动时创建 `ModelInfoCache`（config.toml 与其显式引用目录文件的 mtime + size 指纹），每次请求通过 cache.get()
获取；文件变化时自动重载，未变化时复用缓存。CCS 运行中切换后下一次请求即生效。
任务面板的 B 层修复按真实 rollout 形状解析 JSONL：

- `session_meta` 行的 thread id 在 `payload.id` / `payload.session_id`；
- `event_msg` 行可在 `payload.thread_id` 补齐同一任务的 id；
- 扫描用 `BufReader` 逐行读取，不把 70MB 级 rollout 整个载入内存；解析失败的行按原逻辑跳过；
- 参与候选发现的字段覆盖五类历史绑定：`thread_settings.model`、
  `world_state.state.collaboration_mode.model`（含 `settings.model` 形态）、
  `turn_context.payload.model`、`session_meta.payload.base_instructions.provenance.model`。
  `payload.model_provider` 是 provider 标识（实测恒为 `custom`），不是模型 slug，
  不参与匹配也不改写；
- 权威判定是**线程级**的：Codex 恢复任务只读最新分片
  （mtime 最大）。权威依次取最新分片内最后一条可解析（纯 RFC3339，带时区名的
  Zoned 字符串不算）`event_msg/thread_settings_applied` 的
  `payload.thread_settings.model`；缺失时回退该分片最后一条 `turn_context.model`
  （resume 的实际回退来源）；两者都缺失则无法判定、不报。历史分片里的旧模型字段
  不参与判定、只在明细中展示。app-server
  `thread/resume` 官方修复只往最新分片追加权威记录，按分片判定会让根分片的过期权威
  把线程永远挂在待修复列表上；
- 修复不重写任何历史 JSONL 行。先流式读取该分片，复制最后一条
  `thread_settings_applied` 的完整 `thread_settings`，仅把 `model` 改为当前默认模型，
  再以当前时间与 `max(ordinal)+1` 追加一条同构权威事件。写入前先以追加写模式探测锁，
  再生成相邻备份；追加后校验行数必须恰好 +1，异常时用备份回滚。重复修复时最后权威
  模型已合法，不再追加。扫描与修复读取均为流式，不把整个 rollout 载入内存。

### 2.6 模型修复自动化的触发设计

触发源只看 `~/.codex/config.toml` 的 **mtime + size 指纹**，不监控
`cc-switch-model-catalog.json` 作为触发，也不和 CCS 捆绑。无论配置由 CCS、用户手改还是
脚本写入，只要 `config.toml` 变化，就重新计算目标模型与合法集合。

- 目标模型：`config.toml` 的 `model`；
- 合法集合：读取 `config.toml` 中 `model_catalog_json` 引用的文件（这是 Codex 配置引用的
  文件，不是 CCS 专属概念）；文件存在则使用其 slug 集合并包含默认模型；
- 目录文件缺失、不可解析或为空：不得报错、不得停用，合法集合回退为
  `{config.toml model}` 单元素，即「与默认模型不同就修」；
- 后续如需给手动配置用户更多控制，可在 Halcyon 自己的 config 中增加可选
  `model_repair.allowed_models` 覆盖项。

唯一硬约束是**修复必须发生在 Codex 完全退出期间**。本机实测：任务一旦在 Codex 中打开，
rollout 立即变为可读不可写；空闲、turn 结束、API 归档都不释放，直到 App 退出。App 启动时
还会缓存任务设置，启动后再修文件，打开任务仍会用缓存旧模型写回。SessionStart hook 的输入
虽含 `model` 与 `transcript_path`，但触发时文件已锁，只能检测/提示，不能写；hook 与
“看护脚本监听 Codex 退出”路线均已放弃。

推荐顺序：

1. 完全退出 Codex；
2. 切换 / 手工修改 `config.toml`；
3. Halcyon 检测到 `config.toml` 指纹变化后修复全部可写任务；
4. 启动 Codex 并打开旧任务。

若用户未退出 Codex 就切换配置，不执行修复（避免与 Codex 抢锁）；
待下一次 Codex 完全退出窗口统一补修。

写入安全：修复前「探测锁 → 备份 → 追加 → 行数/指纹复核」，被锁或指纹变化的
文件一律跳过并保留现场，不得强行写。

## 3. 请求处理流程

```
接收 /<route>/responses
  ├─ 解析 body（JSON），失败 → 原样透传（并记日志）
  ├─ 路由可配 aliases：别名路径与上游 base_url **字面等价**
  │   （如 /kimi/v1 ≡ https://api.kimi.com/coding/v1），其后路径逐字对应，零改写
  ├─ 逐条检查 input[]：
  │    ├─ 孤儿注入条目 → 改写成 user 消息
  │    ├─ agent_message → 改写成等价 assistant 消息
  │    ├─ 旧形状 web_search_call → normalize / drop
  │    ├─ 工具回合夹层 / 缺 output → 重排或补 aborted
  │    ├─ 空文本消息（全部文本 part 为空串）→ 丢弃
  │    └─ 其余条目 → 原样保留（含正常配对的 function_call / function_call_output）
  ├─ 记录改写摘要（条数、类型、字节数变化）
  └─ 转发到 route.upstream + 相对路径；响应（含 SSE 流）逐块回传
```

约束：

- **Authorization 等头部透传**，不读取、不记录、不落盘；
- 判定「孤儿注入条目」必须同时满足：`type == function_call_output`、缺 `call_id`、且有 `name` 或 `namespace`
  —— 正常配对输出绝不改写；
- 工具回合修复只搬移 output 的位置或补协议必需的空结果；不改 arguments、不改正文、
  不在正常的连续并行调用中插入额外条目；
- 任何解析异常都退化为「原样透传 + 记日志」，避免垫片自己变成故障点。

## 4. 启动与生命周期

- 代理是常驻进程，不随 Codex 退出而停止；
- `ensure` 子命令：探测 `http://127.0.0.1:8788/health`，不通则以 detached / 无窗口方式拉起，
  等健康后静默退出（幂等，可供 `SessionStart` hook 反复调用）；
- `/health` 返回版本、路由数量、已改写计数，便于人工确认。

### 4.1 开机启动归属（Windows 单链接策略）

Windows 的开机启动 = 用户启动文件夹里的一条快捷方式 `…\Startup\Halcyon.lnk`
（带 `--autostart`、工作目录为 exe 所在目录）；`StartupApproved\StartupFolder`
里的同名值 `0x02…` 表示「已启用」。

一台机器上可能同时存在两份 Halcyon（安装版 `%LOCALAPPDATA%\Halcyon\Halcyon.exe`
与便携版 `app\halcyon.exe`），它们**共用同一条链接名**。只按「链接是否存在」判断会互相抢：谁启动谁把它改写到自己的 exe 上。所以归属判定必须带上「链接目标是不是本份」。

现策略（方案 B）：**链接名不变，判定带上归属**。

| 链接状态 | 是否算「本份已启用」 | `reconcile()` 动作 |
|---|---|---|
| 链接不存在、无旧 Run 项 | 否 | 什么都不做 |
| 链接目标 == 本 exe | 是 | 重建（自愈：目录搬迁 / 改名 / 参数变化） |
| 链接目标 != 本 exe | 否（托盘显示「由另一份安装持有」） | **不动它** |
| 只有旧版 Run 项 | 是（迁移意图） | 建链接，然后清理 Run 项 |

用户显式点「开机启动」时的动作：

- 当前未启用（含「被另一份持有」）→ **接管**：覆盖链接指向本 exe，重置审批值为 `0x02`；
- 当前是本份 → 关闭：删链接 + 删同名审批值；
- 当前由另一份持有且要求关闭 → 什么都不做（不替别人关）。

比较目标路径时忽略大小写、首尾引号与 `/`、`\` 差异，同一路径的不同写法不算「别人」。
判定全部落在 core 的纯逻辑 `halcyon-core::autostart`（可单测）；
app 侧只做 PowerShell 读写，不再自带判定分支。

## 5. 非目标

- 不做协议转换（Responses 与 Chat Completions 互转）——那是 CC Switch 之类工具的职责；
- 不改写模型产生的正常工具调用链路；
- 不替代客户端修复：上游修好后把 `base_url` 改回即可，本项目可直接停用。

## 6. 来源标注与回复语义（A+）

改写后的 user 消息形状：

```
[跨任务消息 · via=<name> · namespace=<ns> · source_thread=<id> · link=codex://threads/<id>]

<正文，逐字保留；`<codex_delegation>` 信封原样在内>
```

设计取舍：

- **保留正文 + 一行标注**，不试图伪造 `function_call` / `function_call_output` 配对——
  那会把"不是模型发出的调用"写进历史；
- 标注让接收方仍能回答"谁发来的、要不要回复、回到哪个任务"，而不必依赖平台提供的元数据
  （平台元数据在改写后就丢了）；
- 标注可关（`orphan_header = false`）；关掉后只剩正文，接收方**看不出**这是跨任务消息。

跨任务投递本身是**单向**的：发送方只收到 ack（目标线程 id），不会有自动回环。
需要回执时，由发送方在自己的提示里写清楚"完成后回哪个线程"。

## 7. 投递模式矩阵（代理不参与调度）

| 投递模式 | 接收方空闲 | 接收方正在跑 | 垫片的作用 |
| --- | --- | --- | --- |
| `queue` | 直接进上下文 | 排队，当前轮结束后进上下文 | 进上下文的那条历史条目被改写成 user 消息 |
| `steer` | 直接进上下文 | 打断当前轮并转向 | 同上，不改变打断时机 |
| `interrupt` | 直接进上下文 | 中断当前轮 | 同上 |

垫片只改"报文长什么样"，从来不决定"什么时候投递"。

## 8. 能力边界

- 上游必须**实现了 Responses 协议**：垫片只做形状修正，不做协议转换；
- 只能修**请求出口**能看到的东西：需要用户干预的会话级状态（例如客户端内存态缓存）不在此列；
- 垫片是关键路径：它没跑而 base_url 又指向它，请求会连接失败——因此用 `SessionStart` hook 兜住；
- 未知形状一律"原样透传 + 记日志"，不猜、不拦。

## 9. 余额/额度查询（声明式规则引擎）

托盘与面板的用量数据来自各供应商的余额/额度接口。解析走**统一声明式规则引擎**
（`balance::parse_custom`）：内置三家供应商模板只是内置 `CustomSpec` 等价物，
与用户自配的 `template = "custom"` 走**同一条解析路径**，不存在两套逻辑。

路由配置：

```json
"balance": {
  "template": "deepseek | kimi | glm | custom | none",
  "url": "查询地址（custom 必填；其余场景为覆盖内置地址）",
  "custom": { "sources": [ ... ], "scale": 100, "label_map": { ... } }
}
```

- 不写 `balance`：按上游 host 自动识别（deepseek / kimi / glm）；
- `template = "custom"`：用户声明式规则生效，接新供应商**零开发**（设置页
  每条路由的「用量查询」按钮 → 对话框配置 + 即时测试）；
- `template = "none"`：关闭该路由的查询。

规则要点（字段路径语法 `a.b[0].c`，`|` 候选回退，`/` 开头取响应根）：

- `sources[]`：行来源（数组 / `rows_as=map` 键→条目 / 单对象），多来源合并；
- 行字段：`label`（`@key` / `str:字面量` / 路径）、`pct`、`used`、`limit`、
  `remaining`、`reset`、`detail`（插值模板 `{path}` / `{path:默认}` / `{{` 转义）；
- pct 优先级：**pct 直读（×scale，四舍五入）→ used+limit → remaining+limit**；
- `label_map` / `label_strip_prefix`：档位名映射与去前缀（先映射后剥前缀）；
- `field_map`：detail 占位值替换，`"*"` 通配项里 `{raw}` 引用原值；
- `flag_suffix`：条件追加后缀（如 `is_available=false` → `（不可用）`）；
- `row_filter` / `row_overrides` / `sort`：行过滤、map 按键覆盖、分组稳定排序；
- detail 任一占位缺失且无缺省 → **整个 detail 丢弃**（不字面输出）；
- 行的保留条件：`pct` 或 `detail` 至少其一；全部来源提取不到行 → 解析失败
  （面板显示"解析失败"+原始数据仍可见，不崩溃、不显假零）。

内置 GLM/Zhipu 模板语义：响应行为
`{ code, data: { limits } }`；`TOKENS_LIMIT unit=3 number=5` 是 5 小时窗口，
`TOKENS_LIMIT unit=6 number=1` 是周窗口，`TIME_LIMIT unit=5 number=1` 是
MCP 月度工具额度（detail 取 `currentValue/usage`，如 `0/4000`）。

安全边界不变：规则只接触**响应体**；Authorization 仅存内存 KeyRing、仅此用途，
不落盘、不进日志、不回显界面；测试对话框**不提供**粘贴 key 入口。

配置校验：`template=custom` 缺 `url` / 缺 `custom` / 规则不合法（空 sources、
路径或模板语法错误、空 sort 规则等）在配置加载与保存时即拒绝。

## 8. 运行数据目录分离（Velopack 前置）

Windows 按四级判定：

```
1. --data-dir <path>       命令行参数（最高优先级）
2. HALCYON_DATA_DIR        环境变量
3. exe 旁 data\ 目录存在   portable 模式
4. %APPDATA%\Halcyon       安装版默认
```

macOS 不走此判定，维持 `~/Library/Application Support/Halcyon/`。

### legacy 迁移

现有便携用户的 `app\` 目录里只有 `config.json`、没有 `data\`。严格按四级
判定会命中第 4 级 `%APPDATA%\Halcyon`，导致配置"消失"。迁移规则：

- resolve 结果不是 exe 旁 `data\` + exe 旁存在旧版 `config.json`
  + 目标 `data\config.json` 不存在 → 创建 `data\`、复制旧 config 进去
- `data\config.json` 已存在 → 绝不覆盖（幂等）
- 旧 `config.json` 原样保留（天然备份）

`config::default_config_path()` 改为 `<data_dir>/config.json`。
## 9. Velopack 打包与启动钩子

### 启动钩子

`app_lib::run()` 入口最前调用 `VelopackApp::build().run()`。这处理
`--veloapp-*` 生命周期参数（install / uninstall / update / obsolete 等），
安装器握手完成后即退出对应子进程。非 Velopack 环境（portable `app\`、
dev `target\`）下完全 no-op，不影响正常运行。

### 打包（一次构建、多通道共用）

统一构建目录 `target\release-package`，带
`RUSTFLAGS=-C link-arg=-Wl,--no-insert-timestamp`（不写 PE 时间戳 → 可复现）。

`scripts/build-velopack.ps1`：

1. 版本一致性检查
2. 安装/更新 vpk CLI（`dotnet tool update -g vpk`）
3. **一次构建**：`npm --prefix ui run build` +
   `cargo build --release -p halcyon-app --features custom-protocol`（`CARGO_TARGET_DIR=target\release-package`）
4. stage（halcyon.exe + WebView2Loader.dll + LICENSE + VERSION + README）
5. `vpk pack`（packId=Halcyon、mainExe=halcyon.exe、icon=icon.ico、shortcuts=StartMenuRoot；
   `--splashImage` + `--splashProgressColor #F4622F` 为硬要求，缺图直接报错）
6. `build-portable.ps1 -FromBuild target\release-package\release`：**复用同一份产物**打
   portable zip，覆盖 vpk 默认的 `*-win-Portable.zip`
7. 输出到 `dist\velopack\`（含 Setup.exe）

### 双轨产物

- `build-portable.ps1` → portable zip（`data\` 标记目录）；独立运行时自己构建，
  给 `-FromBuild <dir>` 时只打包
- `build-velopack.ps1` → Velopack Setup.exe + nupkg（安装到 `%LOCALAPPDATA%\Halcyon\`）

两个通道共用同一次构建，所以 nupkg/Setup 与 portable zip 里的 `halcyon.exe`
逐字节相同（两个通道共用同一次构建）。
## 10. 安装版更新通道（Velopack）

安装版的应用内更新走 Velopack `UpdateManager` + `HttpSource`；portable 走
自研 minisign 清单 + helper/journal 链。运行形态按 §8 判定。两条通道都是
**零 API**：不触碰 api.github.com，全部读取走 `releases/latest/download`
CDN 路由（见 §10.3）。

### 分流

```
check_update / update_download / update_install
  ├─ Velopack locator 存在且非 portable → Installed：Velopack check/download/apply
  └─ 其余（我们的 app\ portable、dev target\）→ 自研链（不变）
```

`UpdateManager::new(NoneSource, ..)` 成功即 locator 存在；`get_is_portable()`
区分安装版与 Velopack portable。

### 授权

无需授权：公开仓的 `releases/latest/download` 路由匿名可读，且不是
api.github.com——不占匿名 API 限额（60 次/小时/IP），无需任何凭据，
应用不保存任何 GitHub 令牌。
### 错误分类

Velopack 错误映射为 `VelopackErrorKind`（NotInstalled / Network / Checksum /
Size / Package / Unsupported / Internal）与 `retryable`，供 UI 展示。
### 10.1 安装器参数与 portable 覆盖

`build-velopack.ps1` 传给 `vpk pack` 的参数：`--packId Halcyon`、
`--packTitle Halcyon`、`--packAuthors weyham`、`--mainExe halcyon.exe`、
`--icon src-tauri\icons\icon.ico`、`--shortcuts StartMenuRoot`；
`--splashImage packaging\velopack\splash.png` 与
`--splashProgressColor #F4622F`（品牌橙，深色底对比度足够）为硬要求：splash 缺失时
直接报错中止，提示先跑 `packaging/velopack/make-splash.py`。

### 10.2 预发布（Pre-release）不可见

零 API 源只读 Latest Release（GitHub 的 Latest 指针从不指向 pre-release），
两条通道都看不到 Pre-release，也没有运行时开关。联调「已装版本 → 预发布版本」
改为直接安装对应包。
### 10.3 双通道的零 API 读取与失败可见性

两条通道读的都是 Latest Release 的 CDN 资产，不调用 api.github.com：

| 通道 | 取版本的方式 | 下载制品的方式 |
|---|---|---|
| 安装版（Velopack `HttpSource`） | `releases/latest/download/releases.win.json` | 包文件名相对拼接到同一路由 |
| 便携版（自研 minisign 链） | `releases/latest/download/latest.json`（+`.minisig`），清单自带版本号与制品 `url` | 清单里每个平台的 `url`（`releases/download/<tag>/<资产名>`） |

清单平台条目同时保留 `assetId` 字段：只服务于 1.0.0 旧客户端的 API 路径，
新客户端不再使用。`latest.json` 或 `latest.json.minisig` 缺失时自研链报
`InvalidManifest`——是**报错**，不是「已是最新」。清单未包含本平台键
（该平台本次未发布）时按「无更新」处理，不报错。

两条通道都不携带任何凭据，应用不保存 GitHub 令牌。安装版 check / download /
apply 三个入口都是 async，不阻塞 UI 线程。

失败可见性：`set_velopack_error()` 会 `log::warn!` 记录错误分类与原始 message；
`app_info_from_update()` 在 `UpdatePhase::Error` 时把 `view.error.message` 拼进
`update_note`，所以关于页「检查失败」徽标旁就能看到原因。
### 10.4 检查阶段与安装阶段的版本语义

两条语义刻意分开：

| 阶段 | 函数 | 清单版本 < 本地版本 |
|---|---|---|
| 检查（有没有更新） | `compare_versions()` → `VersionOrder` | **`LocalNewer` → 已是最新**（附一条 info 日志） |
| 安装（能不能装） | `ensure_not_downgrade()` | `DowngradeRejected`（保留降级保护） |

检查阶段只回答「有没有更新」，刻意不复用安装阶段的 `evaluate_candidate_version()`：复用会把「清单比本地旧」变成 `DowngradeRejected` → 面板「检查失败」；而清单源停在旧版本（`latest.json` 没跟着升）时，每次检查都会误报。`RemoteNewer` 才算有更新。

**降级语义**：任何构建下检查阶段都不提供降级包（`RemoteNewer` 才算有更新）；**降级联调改为直接安装旧版 Setup**；安装阶段的
`ensure_not_downgrade()` 保护未变，`artifact.allow_downgrade` 字段仍会被解析，
只是不再影响检查阶段。

### 10.5 特殊验收发布的注意事项

1. Latest 归属决定客户端看到什么：`releases/latest/download/latest.json` 跟随
   GitHub 的「Latest」指针；Latest 指向的 release 版本若低于本地，检查更新稳定
   返回「已是最新」，不会误升级、也不会报错；
2. 不是给更新链用的 release 不要放 `latest.json` / `latest.json.minisig`：
   这两个文件由离线私钥（minisign）签名，一旦存在就会进入升级链路。
`vpk pack` 后，用 `build-portable.ps1` 自产 portable zip 覆盖
`dist\velopack\Halcyon-win-Portable.zip`（保持 vpk 文件名），并校验 zip 含
`data/` 条目。原因：vpk 默认 portable（`.portable` + `current\`）没有 `data\`
标记、真程序在 `current\` 下，不符合我们的便携行为。

## 11. 修复记录与队列不变量

- **队列只放待执行项**。执行结果（成功 / 失败及原因）在结算时逐条写进
  `repair-history.jsonl`，随后从队列移除；旧版队列文件里积累的 Done/Failed/Cancelled
  在加载时自动清掉。因此「队列」= 待办清单，「记录」= 结果台账，两处职责不重叠。
- **取消 = 从队列移除**（「本跳过一轮」）。不再保留 Cancelled 状态：线程若仍有问题，
  下次模型切换的自动扫描会重新入队。
- **同一线程只有一行**：模型再次变化时，Failed/Done 的条目原地复活为待执行，
  而不是追加新行（否则队列卡会出现「已完成/失败」残留行 + 新待执行行并存）。
- 「修复记录」页支持搜索、单条删除、清空；删除按「最新在前」的展示下标定位，
  写回时恢复文件顺序（临时文件 + rename，避免半写）。
- 队列项只存身份（thread id + 修复类型）与入队时的展示名；**读取与写记录时都用展示索引
  实时解析项目名/标题**（用户之后改项目归属、或修了名字解析，旧值不会固化）。

## 12. 自动修复进度卡片（任务栏上方）

托盘 tooltip 的「x/N」不显眼（用户反馈），因此执行期间额外显示一个独立无边框窗口
（`repair-progress`，360×86，无焦点、不进任务栏、贴主屏工作区右下角）：

- 阶段一（扫描，30-40 秒，总量未知）：不确定进度光带 + 「默认模型已切换为 <模型>」；
- 阶段二（执行）：`settled / total` 确定进度 + 副标题显示刚处理完的任务与结果；
- 收尾：显示「自动修复完成 · 成功 N 个」并停留 5 秒后自动隐藏；
- 隐藏用代际计数（`PROGRESS_GEN`）作废旧的「到点隐藏」任务，避免新一轮开始时卡片闪没。

托盘图标：红 = 未运行，黄 = 无流量，绿 = 有流量（闪烁），蓝点 = 自动修复中。

## 13. 便携日志目录与跨平台路径解析

- **便携版日志固定写 `exe\data\logs`**，不再用 Tauri 的 `app_log_dir()`：后者取决于
  「谁启动了进程」的用户环境，从 Codex 应用容器（MSIX）内启动时会被重定向到容器视图，
  同一台机器出现两份互不可见的 `halcyon.log`，排障时表现为「日志忽然停了」。
  安装版仍用系统日志目录。
- **cwd → 项目名必须同时认 `\` 与 `/`**：rollout 里的 `cwd` 是写入端平台的写法
  （Windows 会话写 `F:\Projects\X`），用宿主 `Path::file_name()` 解析会在 macOS 上
  把整条路径当成一个文件名。统一实现 `unified_scan::project_name_from_cwd`
  （扫描入口与 `scan_model_mismatches` 共用）。
- 项目名优先级：global-state 用户归属（侧边栏分组依据）→ 状态库 `projects` 连表 →
  cwd 目录名兜底。
