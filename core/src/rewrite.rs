//! 请求体改写规则（与 Python 版 rewrite.py 一一对应，完整规则见 docs/design.md）。
//!
//! 纯函数式条目改写，不涉及网络与文件 IO，便于单元测试。

use regex::Regex;
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

const TEXT_PART_TYPES: [&str; 3] = ["input_text", "output_text", "text"];

static SOURCE_THREAD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<source_thread_id>\s*([^<\s]+)\s*</source_thread_id>").unwrap());

/// 一次改写的结果摘要，用于日志与 /health 计数。
#[derive(Debug, Default, Clone)]
pub struct RewriteReport {
    pub orphan_outputs: u32,
    pub tool_rounds_repaired: u32,
    pub empty_messages_dropped: u32,
    pub empty_reasoning_dropped: u32,
    pub web_search_noted: u32,
    pub web_search_dropped: u32,
    pub web_search_normalized: u32,
    /// 多智能体 agent_message 归一为 assistant message 的条数。
    pub agent_messages_normalized: u32,
    pub notes: Vec<String>,
}

impl RewriteReport {
    pub fn changed(&self) -> bool {
        self.orphan_outputs > 0
            || self.tool_rounds_repaired > 0
            || self.empty_messages_dropped > 0
            || self.empty_reasoning_dropped > 0
            || self.web_search_noted > 0
            || self.web_search_dropped > 0
            || self.web_search_normalized > 0
            || self.agent_messages_normalized > 0
    }
}

/// 可用模型表（从 Codex `config.toml` 引用的模型目录文件提取 slug 列表）。
#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    /// 当前默认模型（`~/.codex/config.toml` 的 `model` 字段）。
    pub default_model: String,
    /// 当前目录中所有可用模型的 slug 集合。
    pub available_models: HashSet<String>,
}

/// 带文件指纹（mtime+size）缓存的模型信息；文件变化时自动重载。
#[derive(Debug)]
pub struct ModelInfoCache {
    codex_home: std::path::PathBuf,
    inner: std::sync::Mutex<Option<(u64, ModelInfo)>>,
}

impl ModelInfoCache {
    pub fn new(codex_home: std::path::PathBuf) -> Self {
        Self {
            codex_home,
            inner: std::sync::Mutex::new(None),
        }
    }

    /// 获取当前 ModelInfo；文件指纹变化时自动重载。
    /// 目录不可读时返回 None（fail-safe 透传）。
    pub fn get(&self) -> Option<ModelInfo> {
        let stamp = Self::fingerprint(&self.codex_home);
        let mut guard = self.inner.lock().unwrap();
        if let Some((cached_stamp, ref info)) = *guard {
            if cached_stamp == stamp {
                return Some(info.clone());
            }
        }
        match ModelInfo::load(&self.codex_home) {
            Some(info) => {
                *guard = Some((stamp, info.clone()));
                Some(info)
            }
            None => {
                *guard = None;
                None
            }
        }
    }

    /// 计算 config.toml 及其显式引用目录文件的 mtime+size 拼合指纹。
    fn fingerprint(codex_home: &std::path::Path) -> u64 {
        let mut hash: u64 = 0;
        let mut paths = vec![codex_home.join("config.toml")];
        if let Ok(config_text) = std::fs::read_to_string(codex_home.join("config.toml")) {
            if let Some(catalog_rel) = extract_toml_string(&config_text, "model_catalog_json") {
                paths.push(codex_home.join(catalog_rel));
            }
        }
        for path in paths {
            if let Ok(meta) = std::fs::metadata(&path) {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                hash = hash
                    .wrapping_mul(31)
                    .wrapping_add(mtime)
                    .wrapping_add(meta.len());
            }
        }
        hash
    }
}

impl ModelInfo {
    /// 从 Codex 配置读取默认模型与合法模型集合。
    ///
    /// 目录文件缺失、不可解析或为空时不得停用模型改写：目标模型取
    /// `config.toml` 的 `model`，合法集合回退为 `{默认模型}`。
    pub fn load(codex_home: &std::path::Path) -> Option<Self> {
        let config_path = codex_home.join("config.toml");
        let config_text = std::fs::read_to_string(&config_path).ok()?;
        let default_model = extract_toml_string(&config_text, "model")?;
        let catalog_rel = extract_toml_string(&config_text, "model_catalog_json");
        let catalog_path = catalog_rel.as_deref().map(|rel| codex_home.join(rel));
        let mut available = HashSet::new();
        if let Some(catalog_path) = catalog_path.as_deref() {
            if let Ok(catalog_text) = std::fs::read_to_string(catalog_path) {
                if let Ok(catalog) = serde_json::from_str::<serde_json::Value>(&catalog_text) {
                    if let Some(models) = catalog.get("models").and_then(Value::as_array) {
                        for model in models {
                            if let Some(slug) = model.get("slug").and_then(Value::as_str) {
                                available.insert(slug.to_string());
                            }
                        }
                    }
                }
            }
        }
        available.insert(default_model.clone());
        Some(Self {
            default_model,
            available_models: available,
        })
    }
}

/// 从简单 TOML 文本提取 `key = "value"` 形式的顶层字符串值。
fn extract_toml_string(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} = ");
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&prefix) {
            let value = trimmed[prefix.len()..].trim().trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 对应 Python 的 `str()`：字符串原样；None→"None"；True/False→"True"/"False"；数字按原样。
/// 对应 Python 的 `str()`：字符串原样；None→"None"；True/False→"True"/"False"；数字按原样。
fn json_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// 对应 Python 的 `str(option)`：键不存在（None）→ "None"。
fn py_str(value: Option<&Value>) -> String {
    value.map(json_str).unwrap_or_else(|| "None".into())
}

/// 对应 Python 的真值判定：None/null/false/0/空串/空数组/空对象为 falsy。
fn is_falsy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => !*b,
        Some(Value::Number(n)) => {
            n.as_i64() == Some(0) || n.as_u64() == Some(0) || n.as_f64() == Some(0.0)
        }
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
    }
}

/// 由内容派生的稳定 id：同一段历史每次生成的报文逐字节一致。
pub fn stable_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(parts.join("\0").as_bytes());
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{prefix}{}", &digest[..24])
}

/// 把条目的 `output` 归一化成 content parts（字符串 → 单个 input_text part）。
pub fn output_parts(output: &Value) -> Vec<Value> {
    match output {
        Value::String(s) => vec![json!({"type": "input_text", "text": s})],
        Value::Array(items) => items.iter().filter(|p| p.is_object()).cloned().collect(),
        Value::Null => vec![json!({"type": "input_text", "text": ""})],
        other => vec![json!({"type": "input_text", "text": json_str(other)})],
    }
}

/// 提取 `output` 里的纯文本（用于日志与来源标注解析，不用于改写正文）。
pub fn output_text(output: &Value) -> String {
    output_parts(output)
        .iter()
        .filter(|p| {
            p.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| TEXT_PART_TYPES.contains(&t))
        })
        .map(|p| p.get("text").map(json_str).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 判定 Codex 注入的孤儿 function_call_output。
///
/// 条件：type 为 function_call_output、缺 call_id、且有 name 或 namespace。
/// 正常配对的工具输出必定带 call_id，因此不会被误判。
pub fn is_orphan_injected_output(item: &Value) -> bool {
    if !item.is_object() {
        return false;
    }
    if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
        return false;
    }
    if !is_falsy(item.get("call_id")) {
        return false;
    }
    !is_falsy(item.get("name")) || !is_falsy(item.get("namespace"))
}

/// 判定多智能体（multi-agent v2）写进历史的 `agent_message` 条目。
///
/// Codex 多智能体协议把
/// 代理间消息以 `response_item` 落盘（type=agent_message，带 author/recipient），
/// 重建请求 input 时原样带上；严格上游（Kimi `/responses`）拒绝整个请求：
/// `item type "agent_message" is not supported`。
pub fn is_agent_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("agent_message")
}

/// `agent_message` → assistant `message`：正文逐字保留（content 各文本 part
/// 原样映射为 output_text），author/recipient 等协议包装不带入——协议信封
/// 已完整复制在正文文本里（"Message Type: …\nSender: …"），不丢对话语义。
pub fn agent_message_to_message(item: &Value) -> Value {
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .map(|part| {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                    json!({ "type": "output_text", "text": text })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "type": "message",
        "id": stable_id("msg_am_", &[&py_str(item.get("id"))]),
        "role": "assistant",
        "content": content,
    })
}

/// 判定「空文本消息」：message 的所有 content part 均为文本类型且 text 为空串。
///
/// 这类条目由部分模型返回的空 final_answer 落盘形成（如真实会话中
/// 多条 `{"content":[{"type":"output_text","text":""}]}`）。
/// 严格上游（Kimi `/responses`）会拒绝整个请求：
/// `400 Invalid request: text content is empty`。空消息不携带任何信息，
/// 整条丢弃零语义损失；含非文本 part 或任一非空文本的条目不动。
/// 判定「空文本消息」（离线修复复用本判定）。
pub fn is_empty_text_message(item: &Value) -> bool {
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return false;
    }
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return false;
    };
    if content.is_empty() {
        return false;
    }
    content.iter().all(|part| {
        matches!(
            part.get("type").and_then(Value::as_str),
            Some("input_text" | "output_text" | "text")
        ) && part.get("text").and_then(Value::as_str) == Some("")
    })
}

/// 判定「空 reasoning」：content / summary 里没有任何可读文本（part 为空数组、
/// 文本 part 全部为空串或缺 text 键）。这类条目是跨 provider 会话里残留的
/// **加密 reasoning**（`encrypted_content` 只有原加密方才能解开，Kimi / DeepSeek /
/// GLM 读到的是空 assistant 内容）——Kimi 报
/// `400 message at position N with role 'assistant' must not be empty`。
/// 对无法解密的上游整条丢弃零语义损失；带明文 reasoning_text / summary 的条目不动。
/// 判定「空 reasoning」（离线修复复用本判定）。
pub fn is_empty_reasoning(item: &Value) -> bool {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return false;
    }
    fn readable_text(parts: &[Value]) -> usize {
        parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .map(str::len)
            .sum()
    }
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let summary = item
        .get("summary")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    readable_text(content) + readable_text(summary) == 0
}

/// 从注入条目的信封里取出 `<source_thread_id>`。
pub fn source_thread_id(item: &Value) -> Option<String> {
    let text = output_text(item.get("output").unwrap_or(&Value::Null));
    SOURCE_THREAD_RE.captures(&text).map(|c| c[1].to_string())
}

/// A+ 来源标注：把被丢弃的 name/namespace/来源线程以事实文本补回。
pub fn provenance_header(item: &Value) -> String {
    let via = if is_falsy(item.get("name")) {
        "unknown".to_string()
    } else {
        json_str(item.get("name").unwrap())
    };
    let namespace = if is_falsy(item.get("namespace")) {
        "unknown".to_string()
    } else {
        json_str(item.get("namespace").unwrap())
    };
    let source = source_thread_id(item);
    let source_field = source.clone().unwrap_or_else(|| "unknown".into());
    let link = source
        .map(|s| format!("codex://threads/{s}"))
        .unwrap_or_else(|| "unknown".into());
    format!(
        "[跨任务消息 · via={via} · namespace={namespace} · source_thread={source_field} · link={link}]"
    )
}

/// 把孤儿注入条目改写成 user 消息：正文逐字保留，可选前置来源标注。
pub fn orphan_to_user_message(item: &Value, include_header: bool) -> Value {
    let raw_id = item.get("id").map(json_str).unwrap_or_default();
    let message_id = if raw_id.starts_with("msg_") {
        raw_id
    } else {
        let source = if !raw_id.is_empty() {
            raw_id.clone()
        } else {
            output_text(item.get("output").unwrap_or(&Value::Null))
        };
        stable_id("msg_", &[&source])
    };

    let mut content: Vec<Value> = Vec::new();
    if include_header {
        content.push(json!({"type": "input_text", "text": provenance_header(item)}));
    }
    content.extend(output_parts(item.get("output").unwrap_or(&Value::Null)));

    let mut message = Map::new();
    message.insert("type".into(), json!("message"));
    message.insert("id".into(), json!(message_id));
    message.insert("role".into(), json!("user"));
    message.insert("content".into(), Value::Array(content));
    if let Some(md) = item.get("internal_chat_message_metadata_passthrough") {
        if !is_falsy(Some(md)) {
            message.insert(
                "internal_chat_message_metadata_passthrough".into(),
                md.clone(),
            );
        }
    }
    Value::Object(message)
}

/// 判定"旧形状"的 web_search_call（只带 action.query）。
pub fn is_legacy_web_search(item: &Value) -> bool {
    if !item.is_object() {
        return false;
    }
    if item.get("type").and_then(Value::as_str) != Some("web_search_call") {
        return false;
    }
    let Some(action) = item.get("action") else {
        return false;
    };
    if !action.is_object() {
        return false;
    }
    if action.get("type").and_then(Value::as_str) != Some("search") {
        return false;
    }
    action.get("query").is_some() && action.get("queries").is_none()
}

/// 把搜索痕迹改写成一条说明性消息（不伪造工具调用、不伪造 reasoning）。
pub fn web_search_note_message(item: &Value) -> Value {
    let query = item["action"]
        .get("query")
        .map(json_str)
        .unwrap_or_default();
    let text = format!("[历史记录] 本会话早期执行过一次联网搜索：{query}");
    json!({
        "type": "message",
        "id": stable_id("msg_ws_", &[&py_str(item.get("id")), &query]),
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    })
}

/// 把搜索痕迹改写成「reasoning + queries 调用」对（需要合成 reasoning 文本）。
pub fn web_search_normalized_items(item: &Value) -> Vec<Value> {
    let query = item["action"]
        .get("query")
        .map(json_str)
        .unwrap_or_default();
    let reasoning = json!({
        "type": "reasoning",
        "id": stable_id("rs_shim_", &[&py_str(item.get("id")), &query]),
        "summary": [{"type": "summary_text", "text": format!("检索历史：{query}")}],
        "content": [{"type": "reasoning_text", "text": format!("检索历史：{query}")}],
    });
    let mut call = item.clone();
    call["action"] = json!({"type": "search", "queries": [query]});
    vec![reasoning, call]
}

fn call_id_of(item: &Value) -> Option<&str> {
    item.get("call_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn is_tool_round_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "function_call_output")
    )
}

/// 为缺失结果的 call 合成明确的 `aborted` 结果（离线修复复用）。
pub fn synthetic_aborted_output(call: &Value) -> Value {
    let call_id = call_id_of(call).unwrap_or_default();
    json!({
        "type": "function_call_output",
        "id": stable_id("fco_halcyon_", &[call_id]),
        "call_id": call_id,
        "output": "aborted",
    })
}

/// 工具回合检测结果：需要修补的 call（索引）→ 它的 output（索引；None = 缺失需补 aborted）。
/// 检测规则同时服务请求出口改写与离线 rollout 修复。
pub struct ToolRoundFix {
    pub call_idx: usize,
    pub output_idx: Option<usize>,
}

/// DeepSeek 的 Responses 校验把 `function_call`/`function_call_output` 视为工具回合；
/// reasoning/message 夹在中间会关闭回合，导致即使 output 存在也报 "No tool output found"。
/// 「需要修补」= call 与 output 之间有非工具回合条目，或 call 根本没有 output。
pub fn detect_tool_round_issues(items: &[Value]) -> Vec<ToolRoundFix> {
    let mut call_positions: HashMap<String, Vec<usize>> = HashMap::new();
    let mut output_positions: HashMap<String, usize> = HashMap::new();

    for (idx, item) in items.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                if let Some(call_id) = call_id_of(item) {
                    call_positions
                        .entry(call_id.to_string())
                        .or_default()
                        .push(idx);
                }
            }
            Some("function_call_output") => {
                if let Some(call_id) = call_id_of(item) {
                    output_positions.entry(call_id.to_string()).or_insert(idx);
                }
            }
            _ => {}
        }
    }

    let mut fixes = Vec::new();
    for (call_id, positions) in &call_positions {
        let call_idx = positions[0];
        if let Some(&output_idx) = output_positions.get(call_id) {
            let dirty = if output_idx > call_idx {
                items[call_idx + 1..output_idx]
                    .iter()
                    .any(|item| !is_tool_round_item(item))
            } else {
                true
            };
            if dirty {
                fixes.push(ToolRoundFix {
                    call_idx,
                    output_idx: Some(output_idx),
                });
            }
        } else if positions.len() == 1 {
            fixes.push(ToolRoundFix {
                call_idx,
                output_idx: None,
            });
        }
    }
    fixes
}

/// 这里只在发现这种夹层或 call 确实没有 output 时修补：已有 output 移到 call 后，
/// 缺失 output 补一个明确的 `aborted` 结果；正常的并行调用顺序不变。
fn repair_tool_rounds(items: &[Value], report: &mut RewriteReport) -> Vec<Value> {
    let fixes = detect_tool_round_issues(items);

    let mut skip_outputs: HashSet<usize> = HashSet::new();
    let mut insert_after: HashMap<usize, Vec<Value>> = HashMap::new();
    let mut repaired = 0u32;

    for fix in fixes {
        let call_idx = fix.call_idx;
        let call = &items[call_idx];
        if let Some(output_idx) = fix.output_idx {
            if skip_outputs.insert(output_idx) {
                insert_after
                    .entry(call_idx)
                    .or_default()
                    .push(items[output_idx].clone());
                repaired += 1;
            }
        } else {
            insert_after
                .entry(call_idx)
                .or_default()
                .push(synthetic_aborted_output(call));
            repaired += 1;
        }
    }

    if repaired == 0 {
        return items.to_vec();
    }
    report.tool_rounds_repaired += repaired;

    let mut rewritten = Vec::with_capacity(items.len() + repaired as usize);
    for (idx, item) in items.iter().enumerate() {
        if skip_outputs.contains(&idx) {
            continue;
        }
        rewritten.push(item.clone());
        if let Some(inserted) = insert_after.get(&idx) {
            rewritten.extend(inserted.iter().cloned());
        }
    }
    rewritten
}

/// 改写 `input[]`：空文本/空 reasoning 删除；agent_message → assistant 消息；
/// 孤儿注入条目 → user 消息；旧形状搜索 → note / drop / normalize。
pub fn rewrite_input_items(
    items: &[Value],
    web_search: &str,
    web_search_note_max: usize,
    include_header: bool,
) -> (Vec<Value>, RewriteReport) {
    let mut report = RewriteReport::default();
    let legacy_count = items.iter().filter(|i| is_legacy_web_search(i)).count();
    let mut effective_mode = web_search.to_string();
    if web_search == "note" && legacy_count > web_search_note_max {
        effective_mode = "drop".to_string();
        report.notes.push(format!(
            "旧搜索条目数 {legacy_count} 超过上限 {web_search_note_max}，本请求退化为 drop"
        ));
    }

    let mut rewritten: Vec<Value> = Vec::new();
    for item in items {
        if is_empty_text_message(item) {
            report.empty_messages_dropped += 1;
            continue;
        }
        if is_empty_reasoning(item) {
            report.empty_reasoning_dropped += 1;
            continue;
        }
        if is_agent_message(item) {
            report.agent_messages_normalized += 1;
            rewritten.push(agent_message_to_message(item));
            continue;
        }
        if is_orphan_injected_output(item) {
            report.orphan_outputs += 1;
            rewritten.push(orphan_to_user_message(item, include_header));
            continue;
        }
        if is_legacy_web_search(item) {
            match effective_mode.as_str() {
                "drop" => {
                    report.web_search_dropped += 1;
                    continue;
                }
                "normalize" => {
                    report.web_search_normalized += 1;
                    rewritten.extend(web_search_normalized_items(item));
                    continue;
                }
                _ => {
                    report.web_search_noted += 1;
                    rewritten.push(web_search_note_message(item));
                    continue;
                }
            }
        }
        rewritten.push(item.clone());
    }
    let rewritten = repair_tool_rounds(&rewritten, &mut report);
    (rewritten, report)
}

/// 改写一次 Responses 请求体；`input` 不是条目数组时原样返回。
pub fn rewrite_request_body(
    body: &Value,
    web_search: &str,
    web_search_note_max: usize,
    include_header: bool,
) -> (Value, RewriteReport) {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return (body.clone(), RewriteReport::default());
    };
    let (rewritten, report) =
        rewrite_input_items(items, web_search, web_search_note_max, include_header);
    if !report.changed() {
        return (body.clone(), report);
    }
    let mut new_body = body.clone();
    new_body["input"] = Value::Array(rewritten);
    (new_body, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELEGATION_OUTPUT: &str = "<codex_delegation>\n  <source_thread_id>01a0abcd-0000-7000-8000-000000000000</source_thread_id>\n  <input>补齐并合并 WorkBuddy Agent 的安装适配。</input>\n</codex_delegation>";

    fn orphan() -> Value {
        json!({
            "type": "function_call_output",
            "id": "fco_01a0cdef-0000-7000-8000-000000000001",
            "name": "send_message_to_thread",
            "namespace": "codex_app",
            "output": DELEGATION_OUTPUT,
            "internal_chat_message_metadata_passthrough": {"turn_id": "01a0cdef"},
        })
    }

    /// 多智能体 agent_message 的真实落盘形态。
    fn agent_message() -> Value {
        json!({
            "type": "agent_message",
            "id": "amsg_01a111ea-747c-71b1-8eba-c16316a6faa0",
            "author": "/root/velopack_ci_audit",
            "recipient": "/root",
            "content": [{
                "type": "input_text",
                "text": "Message Type: FINAL_ANSWER\nTask name: /root\nPayload:\n完成"
            }],
            "internal_chat_message_metadata_passthrough": {"turn_id": "x"}
        })
    }

    #[test]
    fn agent_message_normalizes_to_assistant_message() {
        let item = agent_message();
        assert!(is_agent_message(&item));
        let converted = agent_message_to_message(&item);
        assert_eq!(converted["type"], "message");
        assert_eq!(converted["role"], "assistant");
        assert_eq!(converted["content"][0]["type"], "output_text");
        assert_eq!(
            converted["content"][0]["text"].as_str().unwrap(),
            "Message Type: FINAL_ANSWER\nTask name: /root\nPayload:\n完成",
            "正文必须逐字保留"
        );

        let (out, report) = rewrite_input_items(&[item], "note", 100, false);
        assert_eq!(report.agent_messages_normalized, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "message");
        assert_eq!(out[0]["role"], "assistant");
    }

    fn legacy_search() -> Value {
        json!({
            "type": "web_search_call",
            "id": "ws_abc",
            "status": "completed",
            "action": {"type": "search", "query": "sprite-anim 现状"},
        })
    }

    fn paired_call() -> Value {
        json!({"type": "function_call", "call_id": "call_1", "name": "exec_command", "arguments": "{}"})
    }

    fn paired_output() -> Value {
        json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"})
    }

    fn plain_message() -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]})
    }

    fn body(items: Vec<Value>) -> Value {
        json!({"model": "deepseek-flash", "store": false, "input": items})
    }

    #[test]
    fn orphan_detection() {
        assert!(is_orphan_injected_output(&orphan()));
        assert!(!is_orphan_injected_output(&paired_output()));
        assert!(!is_orphan_injected_output(&plain_message()));
        assert!(!is_orphan_injected_output(&json!("not-a-dict")));
    }

    #[test]
    fn orphan_to_user_message_keeps_text_and_adds_header() {
        let message = orphan_to_user_message(&orphan(), true);
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "user");
        assert!(message["id"].as_str().unwrap().starts_with("msg_"));

        let header = message["content"][0]["text"].as_str().unwrap();
        let body = message["content"][1]["text"].as_str().unwrap();
        assert!(
            header.starts_with("[跨任务消息 · via=send_message_to_thread · namespace=codex_app")
        );
        assert!(header.contains("source_thread=01a0abcd-0000-7000-8000-000000000000"));
        assert!(header.contains("link=codex://threads/01a0abcd-0000-7000-8000-000000000000"));
        assert_eq!(body, DELEGATION_OUTPUT);
        assert_eq!(
            message["internal_chat_message_metadata_passthrough"],
            json!({"turn_id": "01a0cdef"})
        );
    }

    #[test]
    fn orphan_output_as_content_parts_is_preserved() {
        let mut item = orphan();
        item["output"] = json!([
            {"type": "input_text", "text": "hello"},
            {"type": "input_image", "image_url": "data:x"},
        ]);
        let message = orphan_to_user_message(&item, true);
        assert_eq!(message["content"][0]["type"], "input_text");
        assert_eq!(
            message["content"][1],
            json!({"type": "input_text", "text": "hello"})
        );
        assert_eq!(message["content"][2]["type"], "input_image");
    }

    #[test]
    fn reasoning_between_call_and_output_is_moved_after_output() {
        let call = paired_call();
        let reasoning = json!({
            "type": "reasoning",
            "id": "rs_interleaved",
            "summary": [{"type": "summary_text", "text": "thinking"}],
        });
        let output = paired_output();
        let (new_body, report) = rewrite_request_body(
            &body(vec![call.clone(), reasoning.clone(), output.clone()]),
            "note",
            20,
            true,
        );

        assert_eq!(report.tool_rounds_repaired, 1);
        assert_eq!(new_body["input"][0], call);
        assert_eq!(new_body["input"][1], output);
        assert_eq!(new_body["input"][2], reasoning);
    }

    #[test]
    fn orphan_between_call_and_output_is_moved_after_output() {
        let call = paired_call();
        let output = paired_output();
        let (new_body, report) = rewrite_request_body(
            &body(vec![call.clone(), orphan(), output.clone()]),
            "note",
            20,
            true,
        );

        assert_eq!(report.orphan_outputs, 1);
        assert_eq!(report.tool_rounds_repaired, 1);
        assert_eq!(new_body["input"][0], call);
        assert_eq!(new_body["input"][1], output);
        assert_eq!(new_body["input"][2]["role"], "user");
    }

    #[test]
    fn missing_tool_output_gets_synthetic_aborted_result() {
        let call = paired_call();
        let message = plain_message();
        let (new_body, report) =
            rewrite_request_body(&body(vec![call.clone(), message.clone()]), "note", 20, true);

        assert_eq!(report.tool_rounds_repaired, 1);
        assert_eq!(new_body["input"][0], call);
        assert_eq!(new_body["input"][1]["type"], "function_call_output");
        assert_eq!(new_body["input"][1]["call_id"], "call_1");
        assert_eq!(new_body["input"][1]["output"], "aborted");
        assert_eq!(new_body["input"][2], message);
    }

    #[test]
    fn normal_parallel_tool_calls_are_not_reordered() {
        let call_a =
            json!({"type": "function_call", "call_id": "call_a", "name": "a", "arguments": "{}"});
        let call_b =
            json!({"type": "function_call", "call_id": "call_b", "name": "b", "arguments": "{}"});
        let output_a = json!({"type": "function_call_output", "call_id": "call_a", "output": "a"});
        let output_b = json!({"type": "function_call_output", "call_id": "call_b", "output": "b"});
        let items = vec![call_a, call_b, output_a, output_b];
        let (new_body, report) = rewrite_request_body(&body(items.clone()), "note", 20, true);

        assert_eq!(report.tool_rounds_repaired, 0);
        assert_eq!(new_body["input"], json!(items));
    }

    fn empty_assistant_message() -> Value {
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": ""}],
        })
    }

    fn empty_encrypted_reasoning() -> Value {
        json!({
            "type": "reasoning",
            "id": "rs_encrypted",
            "content": [],
            "encrypted_content": "ciphertext-blob",
        })
    }

    #[test]
    fn empty_reasoning_is_dropped() {
        let keep = plain_message();
        let (new_body, report) = rewrite_request_body(
            &body(vec![keep.clone(), empty_encrypted_reasoning()]),
            "note",
            20,
            true,
        );
        assert_eq!(report.empty_reasoning_dropped, 1);
        assert_eq!(new_body["input"], json!([keep]));
    }

    #[test]
    fn reasoning_with_text_is_kept() {
        let rs = json!({
            "type": "reasoning",
            "id": "rs_1",
            "content": [{"type": "reasoning_text", "text": "thinking"}],
        });
        let (new_body, report) = rewrite_request_body(&body(vec![rs.clone()]), "note", 20, true);
        assert_eq!(report.empty_reasoning_dropped, 0);
        assert!(!report.changed());
        assert_eq!(new_body["input"], json!([rs]));
    }

    #[test]
    fn reasoning_with_summary_only_is_kept() {
        let rs = json!({
            "type": "reasoning",
            "id": "rs_2",
            "summary": [{"type": "summary_text", "text": "summary"}],
        });
        let (new_body, report) = rewrite_request_body(&body(vec![rs.clone()]), "note", 20, true);
        assert_eq!(report.empty_reasoning_dropped, 0);
        assert!(!report.changed());
        assert_eq!(new_body["input"], json!([rs]));
    }

    #[test]
    fn model_info_loads_catalog_from_codex_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"model = "glm-5.3"
model_catalog_json = "models.json"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            r#"{"models":[{"slug":"glm-5.3-flash"}]}"#,
        )
        .unwrap();

        let info = ModelInfo::load(dir.path()).unwrap();
        assert_eq!(info.default_model, "glm-5.3");
        assert_eq!(
            info.available_models,
            HashSet::from(["glm-5.3".to_string(), "glm-5.3-flash".to_string()])
        );
    }

    #[test]
    fn model_info_missing_catalog_falls_back_to_default_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"model = "glm-5.3"
model_catalog_json = "missing.json"
"#,
        )
        .unwrap();

        let info = ModelInfo::load(dir.path()).unwrap();
        assert_eq!(info.default_model, "glm-5.3");
        assert_eq!(
            info.available_models,
            HashSet::from(["glm-5.3".to_string()])
        );
    }

    #[test]
    fn model_info_without_catalog_ref_ignores_legacy_catalog_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), r#"model = "glm-5.3""#).unwrap();
        std::fs::write(
            dir.path().join("cc-switch-model-catalog.json"),
            r#"{"models":[{"slug":"glm-5.3-flash"}]}"#,
        )
        .unwrap();

        let info = ModelInfo::load(dir.path()).unwrap();
        assert_eq!(
            info.available_models,
            HashSet::from(["glm-5.3".to_string()]),
            "未显式引用目录时不得隐式绑定 CCS 文件名"
        );
    }

    #[test]
    fn model_info_cache_follows_referenced_catalog_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"model = "glm-5.3"
model_catalog_json = "models.json"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("models.json"),
            r#"{"models":[{"slug":"glm-5.3-flash"}]}"#,
        )
        .unwrap();
        let cache = ModelInfoCache::new(dir.path().to_path_buf());
        assert_eq!(
            cache.get().unwrap().available_models,
            HashSet::from(["glm-5.3".to_string(), "glm-5.3-flash".to_string()])
        );

        std::fs::write(
            dir.path().join("models.json"),
            r#"{"models":[{"slug":"deepseek-v4-pro"}]}"#,
        )
        .unwrap();
        assert_eq!(
            cache.get().unwrap().available_models,
            HashSet::from(["glm-5.3".to_string(), "deepseek-v4-pro".to_string()])
        );
    }
    #[test]
    fn empty_text_message_is_dropped() {
        let keep = plain_message();
        let (new_body, report) = rewrite_request_body(
            &body(vec![keep.clone(), empty_assistant_message()]),
            "note",
            20,
            true,
        );
        assert_eq!(report.empty_messages_dropped, 1);
        assert_eq!(new_body["input"], json!([keep]));
    }

    #[test]
    fn empty_text_user_message_is_dropped_too() {
        let keep = plain_message();
        let empty_user = json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": ""}],
        });
        let (new_body, report) =
            rewrite_request_body(&body(vec![empty_user, keep.clone()]), "note", 20, true);
        assert_eq!(report.empty_messages_dropped, 1);
        assert_eq!(new_body["input"], json!([keep]));
    }

    #[test]
    fn message_with_any_nonempty_text_is_kept() {
        let mixed = json!({
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "output_text", "text": ""},
                {"type": "output_text", "text": "hi"},
            ],
        });
        let (new_body, report) = rewrite_request_body(&body(vec![mixed.clone()]), "note", 20, true);
        assert_eq!(report.empty_messages_dropped, 0);
        assert!(!report.changed());
        assert_eq!(new_body["input"], json!([mixed]));
    }

    #[test]
    fn message_with_non_text_part_is_kept() {
        let with_image = json!({
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": ""},
                {"type": "input_image", "image_url": "data:x"},
            ],
        });
        let (new_body, report) =
            rewrite_request_body(&body(vec![with_image.clone()]), "note", 20, true);
        assert_eq!(report.empty_messages_dropped, 0);
        assert!(!report.changed());
        assert_eq!(new_body["input"], json!([with_image]));
    }

    #[test]
    fn normal_items_untouched_and_order_kept() {
        let items = vec![plain_message(), paired_call(), paired_output(), orphan()];
        let (new_body, report) = rewrite_request_body(&body(items.clone()), "note", 20, true);
        assert_eq!(report.orphan_outputs, 1);
        assert_eq!(new_body["input"][0], items[0]);
        assert_eq!(new_body["input"][1], items[1]);
        assert_eq!(new_body["input"][2], items[2]);
        assert_eq!(new_body["input"][3]["role"], "user");
        // 原始 body 未被就地修改
        assert_eq!(items[3], orphan());
    }

    #[test]
    fn legacy_search_detection_only_for_old_shape() {
        assert!(is_legacy_web_search(&legacy_search()));
        let mut item = legacy_search();
        item["action"] = json!({"type": "search", "queries": ["x"]});
        assert!(!is_legacy_web_search(&item));
        let mut item = legacy_search();
        item["action"] = json!({"type": "search", "query": "x", "queries": ["x"]});
        assert!(!is_legacy_web_search(&item));
        let mut item = legacy_search();
        item["action"] = json!({"type": "open_page", "query": "x"});
        assert!(!is_legacy_web_search(&item));
        assert!(!is_legacy_web_search(&plain_message()));
    }

    #[test]
    fn web_search_note_mode_default() {
        let (new_body, report) = rewrite_request_body(
            &body(vec![plain_message(), legacy_search()]),
            "note",
            20,
            true,
        );
        assert_eq!(report.web_search_noted, 1);
        let note = &new_body["input"][1];
        assert_eq!(note["type"], "message");
        assert_eq!(note["role"], "user");
        assert_eq!(
            note["content"][0]["text"],
            "[历史记录] 本会话早期执行过一次联网搜索：sprite-anim 现状"
        );
    }

    #[test]
    fn web_search_drop_mode() {
        let (new_body, report) = rewrite_request_body(
            &body(vec![plain_message(), legacy_search()]),
            "drop",
            20,
            true,
        );
        assert_eq!(report.web_search_dropped, 1);
        assert_eq!(new_body["input"], json!([plain_message()]));
    }

    #[test]
    fn web_search_normalize_mode_inserts_reasoning() {
        let (new_body, report) = rewrite_request_body(
            &body(vec![plain_message(), legacy_search()]),
            "normalize",
            20,
            true,
        );
        assert_eq!(report.web_search_normalized, 1);
        let reasoning = &new_body["input"][1];
        let call = &new_body["input"][2];
        assert_eq!(reasoning["type"], "reasoning");
        assert_eq!(reasoning["content"][0]["type"], "reasoning_text");
        assert_eq!(
            call["action"],
            json!({"type": "search", "queries": ["sprite-anim 现状"]})
        );
    }

    #[test]
    fn note_max_falls_back_to_drop() {
        let mut items = vec![plain_message()];
        for i in 0..3 {
            let mut s = legacy_search();
            s["id"] = json!(format!("ws_{i}"));
            items.push(s);
        }
        let (new_body, report) = rewrite_request_body(&body(items), "note", 2, true);
        assert_eq!(report.web_search_dropped, 3);
        assert_eq!(report.web_search_noted, 0);
        assert!(!report.notes.is_empty());
        assert_eq!(new_body["input"], json!([plain_message()]));
    }

    #[test]
    fn rewrite_is_idempotent() {
        let b = body(vec![
            plain_message(),
            orphan(),
            paired_call(),
            paired_output(),
            legacy_search(),
        ]);
        let (once, _) = rewrite_request_body(&b, "note", 20, true);
        let (twice, report2) = rewrite_request_body(&once, "note", 20, true);
        assert_eq!(twice, once);
        assert!(!report2.changed());
    }

    #[test]
    fn input_not_a_list_is_untouched() {
        let b = json!({"model": "m", "input": "hello"});
        let (new_body, report) = rewrite_request_body(&b, "note", 20, true);
        assert_eq!(new_body, b);
        assert!(!report.changed());
    }
}
