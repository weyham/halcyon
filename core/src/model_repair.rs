//! 扫描 / 修复 rollout 文件中的模型不匹配任务。
//!
//! 扫描全量 `~/.codex/sessions\**\*.jsonl`，按 thread 聚合（同 thread 多分片
//! 合并到同一 task 的 files）。Codex 恢复任务以文件内最后一条
//! `thread_settings_applied` 为权威模型；历史模型字段只用于发现候选。
//! 修复绝不重写历史，只在需要修复的分片末尾追加一条与手动切换同构的权威事件，
//! 并先生成相邻备份。幂等：最后权威模型已合法时不再追加。

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

/// 自动审查（Guardian review）线程的 `thread_source` 标记：不进自动修复范围。
pub const GUARDIAN_REVIEW_SOURCE: &str = "guardian_review";

use crate::rewrite::ModelInfo;

/// 一个文件的扫描/修复结果。
#[derive(Debug, Clone)]
pub struct FileModelReport {
    pub path: PathBuf,
    /// 发现的字段路径（如 `thread_settings.model`、`session_meta.provenance.model`）。
    pub fields: Vec<String>,
    /// 修复后实际改了哪些字段。
    pub changed_fields: Vec<String>,
    /// 修复后的备份路径。
    pub backup_path: Option<PathBuf>,
    /// 文件内时间戳不可解析的 thread_settings_applied 记录数。
    pub unparseable_authority: usize,
    /// 读取/解析错误。
    pub error: Option<String>,
}

/// 一个 thread 的聚合报告。
#[derive(Debug, Clone)]
pub struct TaskModelReport {
    pub thread_id: String,
    /// 任务名（侧边栏标题 → 首条用户消息 → 前端回退短 id）。
    pub title: Option<String>,
    /// 项目名：分片 session_meta.cwd 的目录名。
    pub project: Option<String>,
    /// 线程来源（state DB `thread_source`，如 `guardian_review`）。
    pub thread_source: Option<String>,
    pub current_model: String,
    pub target_model: String,
    pub mismatch_count: usize,
    /// 时间戳不可解析的权威记录总数（各分片合计）。
    pub unparseable_authority: usize,
    pub files: Vec<FileModelReport>,
}

impl TaskModelReport {
    pub fn to_json(&self) -> Value {
        json!({
            "thread_id": self.thread_id,
            "title": self.title,
            "project": self.project,
            "thread_source": self.thread_source,
            "current_model": self.current_model,
            "target_model": self.target_model,
            "mismatch_count": self.mismatch_count,
            "unparseable_authority": self.unparseable_authority,
            "error": Value::Null,
            "files": self.files.iter().map(|f| json!({
                "path": f.path.to_string_lossy(),
                "fields": f.fields,
                "changed_fields": f.changed_fields,
                "backup_path": f.backup_path.as_ref().map(|b| b.to_string_lossy()),
                "unparseable_authority": f.unparseable_authority,
                "error": f.error,
            })).collect::<Vec<_>>(),
        })
    }
}

/// 单个已扫描文件的最小信息。
#[derive(Debug, Clone)]
struct ScannedFile {
    path: PathBuf,
    /// 文件修改时间（线程级判定取 mtime 最大的分片为 Codex 实际恢复来源）。
    modified: Option<SystemTime>,
    fields: Vec<(String, String)>,
    /// 本文件最后一条 thread_settings_applied 的模型；None 表示没有权威事件。
    last_applied_model: Option<String>,
    /// 本文件最后一条 turn_context 的模型（权威记录缺失时的回退：
    /// Codex resume 实际使用每轮持久化的模型绑定）。
    last_turn_context_model: Option<String>,
    /// 时间戳不可解析的 thread_settings_applied 记录数。
    unparseable_authority: usize,
}

/// 扫描期间按 thread 聚合的中间结果。
#[derive(Debug, Default)]
struct ScannedThread {
    project: Option<String>,
    first_user_message: Option<String>,
    files: Vec<ScannedFile>,
}

/// 从 JSONL record 中提取 thread id。
///
/// 真实 rollout 的第一行 `session_meta` 把 id 放在 payload 根部；
/// 后续 `event_msg` 则通过 `thread_id` 引用同一任务（没有嵌套层）。
fn extract_thread_id(payload: &Value) -> Option<&str> {
    payload
        .pointer("/id")
        .or_else(|| payload.pointer("/session_id"))
        .or_else(|| payload.pointer("/thread_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// 从 JSONL 的 payload 中提取模型相关字段路径和当前值。
fn extract_model_fields(record: &Value, payload: &Value) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let record_type = record.get("type").and_then(Value::as_str);

    // thread_settings.model
    if let Some(model) = payload
        .pointer("/thread_settings/model")
        .and_then(Value::as_str)
    {
        fields.push(("thread_settings.model".to_string(), model.to_string()));
    }

    // world_state 是 Codex 恢复任务设置的主来源，同时存在直接 model 与
    // settings.model 两种真实形态。
    if record_type == Some("world_state") {
        for (path, label) in [
            (
                "/state/collaboration_mode/model",
                "world_state.state.collaboration_mode.model",
            ),
            (
                "/state/collaboration_mode/settings/model",
                "world_state.state.collaboration_mode.settings.model",
            ),
        ] {
            if let Some(model) = payload.pointer(path).and_then(Value::as_str) {
                fields.push((label.to_string(), model.to_string()));
            }
        }
    }

    // turn_context 的 payload.model 是每轮请求的持久化模型绑定。
    if record_type == Some("turn_context") {
        if let Some(model) = payload.pointer("/model").and_then(Value::as_str) {
            fields.push(("turn_context.payload.model".to_string(), model.to_string()));
        }
    }

    // 只在 session_meta 行提取 provenance.model；event_msg 中不存在这个字段。
    // payload.model_provider 是 provider 标识（真实数据恒为 custom），不是模型
    // slug，不能拿它和可用模型目录比较，也不参与修复。
    if record_type == Some("session_meta") {
        if let Some(model) = payload
            .pointer("/base_instructions/provenance/model")
            .and_then(Value::as_str)
        {
            fields.push((
                "session_meta.base_instructions.provenance.model".to_string(),
                model.to_string(),
            ));
        }
    }
    fields
}

/// 判断字段值是否在可用模型目录中。
fn is_in_catalog(model: &str, info: &ModelInfo) -> bool {
    info.available_models.contains(model)
}

/// 判断记录是否为权威线程设置事件。
fn is_thread_settings_applied(record: &Value, payload: &Value) -> bool {
    record.get("type").and_then(Value::as_str) == Some("event_msg")
        && payload.get("type").and_then(Value::as_str) == Some("thread_settings_applied")
}

/// 判断权威记录的顶层 timestamp 是否能被 Codex 接受。
///
/// Codex 只识别纯 RFC3339 时间戳（结尾 `Z` 或标准偏移）；带时区名的
/// Zoned 字符串（如 `...+08:00[Asia/Shanghai]`）会被忽略，因此这类记录
/// 不构成权威，扫描与修复都必须跳过。注意 jiff 的 `Timestamp` 解析接受
/// RFC 9557 时区标注，必须额外排除 `[` 注解。
fn has_parseable_rfc3339_timestamp(record: &Value) -> bool {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.contains('[') && value.parse::<jiff::Timestamp>().is_ok())
}

/// 从 Codex global state 读取侧边栏任务标题（thread id → 标题）。
///
/// 只读读取 `sessions_root` 同级（即 `~/.codex`）下的
/// `.codex-global-state.json`；文件缺失、JSON 非法、字段缺失或值不是
/// 非空字符串都静默跳过——标题只是展示优化，绝不成为故障点。
fn load_sidebar_titles(sessions_root: &Path) -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let Some(codex_home) = sessions_root.parent() else {
        return titles;
    };
    let Ok(content) = std::fs::read_to_string(codex_home.join(".codex-global-state.json")) else {
        return titles;
    };
    let Ok(state) = serde_json::from_str::<Value>(content.trim_start_matches('\u{feff}')) else {
        return titles;
    };
    let Some(map) = state
        .pointer("/electron-persisted-atom-state/thread-descriptions-v1")
        .and_then(Value::as_object)
    else {
        return titles;
    };
    for (thread_id, value) in map {
        if let Some(title) = value.as_str().map(str::trim).filter(|t| !t.is_empty()) {
            titles.insert(thread_id.clone(), title.to_string());
        }
    }
    titles
}

/// app-server 状态库（`~/.codex/state_*.sqlite`）的线程展示信息索引。
///
/// 这是 `thread/list` 的 `useStateDbOnly` 模式读取的同一个库：
/// `threads.name` 是 Codex 侧边栏原标题（实测覆盖率远高于 global-state
/// JSON 里的自动摘要），`threads.project_id` 连 `projects` 表可取项目名。
#[derive(Default)]
struct StateDbIndex {
    /// thread id → 任务名（`name` 优先，缺省回退 `title`）。
    titles: HashMap<String, String>,
    /// thread id → 项目名（`threads.project_id` → `projects.name`）。
    projects: HashMap<String, String>,
    /// thread id → thread_source（如 `guardian_review`，用于过滤自动修复范围）。
    sources: HashMap<String, String>,
}

/// 统一扫描使用的展示索引：thread id → 任务名 / 项目名。
pub struct DisplayIndex {
    pub titles: HashMap<String, String>,
    pub projects: HashMap<String, String>,
    pub sources: HashMap<String, String>,
}

/// 加载展示索引：状态库优先，global-state 自动摘要补标题空缺。
///
/// 项目名优先级：global-state 用户归属 > 状态库 project_id > cwd 目录名兜底。
/// cwd 兜底对「先无项目创建、后被归入项目」的线程会显示成
/// `codex-threads-<id>` 这类无意义目录名，所以 global-state 必须最优先
///（实测：状态库 project_id 可能为 NULL，但 global-state
/// 已归入 neowand-arts）。
pub fn load_display_index(sessions_root: &Path) -> DisplayIndex {
    let db = load_state_db_index(sessions_root);
    let sidebar = load_sidebar_titles(sessions_root);
    let mut titles = db.titles;
    for (thread_id, title) in sidebar {
        titles.entry(thread_id).or_insert(title);
    }
    let mut projects = db.projects;
    if let Some(home) = sessions_root.parent() {
        for (thread_id, name) in global_project_names(home) {
            projects.insert(thread_id, name);
        }
    }
    DisplayIndex {
        titles,
        projects,
        sources: db.sources,
    }
}

/// global-state 的项目归属展示名：thread_id → 项目名。
/// 即 Codex 应用侧边栏的分组依据（thread-project-assignments → local-projects）。
pub fn global_project_names(codex_home: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(text) = std::fs::read_to_string(codex_home.join(".codex-global-state.json")) else {
        return names;
    };
    // 容错：某些写入方（PowerShell Set-Content 等）会带 UTF-8 BOM。
    let Ok(state) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else {
        return names;
    };
    let mut id_to_name: HashMap<String, String> = HashMap::new();
    if let Some(local) = state.get("local-projects") {
        let items: Vec<&Value> = match local {
            Value::Array(list) => list.iter().collect(),
            Value::Object(map) => map.values().collect(),
            _ => Vec::new(),
        };
        for project in items {
            let id = project.get("id").and_then(Value::as_str);
            let name = project
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty());
            if let (Some(id), Some(name)) = (id, name) {
                id_to_name.insert(id.to_string(), name.to_string());
            }
        }
    }
    if let Some(assign) = state
        .get("thread-project-assignments")
        .and_then(Value::as_object)
    {
        for (thread_id, value) in assign {
            if let Some(name) = value
                .get("projectId")
                .and_then(Value::as_str)
                .and_then(|pid| id_to_name.get(pid))
            {
                names.insert(thread_id.clone(), name.clone());
            }
        }
    }
    names
}

/// 在 codex_home 下找版本号最大的 `state_<N>.sqlite`。
fn find_state_db(codex_home: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(codex_home).ok()?;
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let version = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u64>()
                .ok()?;
            Some((version, entry.path()))
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
}

/// 只读读取状态库；表/列缺失、文件损坏、锁冲突等一律静默回退为空索引。
///
/// 先尝试 READ_ONLY 打开（Codex 运行中 WAL 模式下读安全）；失败再以
/// 普通模式打开（触发 SQLite 标准 WAL 恢复，只发 SELECT 不写业务数据）。
fn load_state_db_index(sessions_root: &Path) -> StateDbIndex {
    let mut index = StateDbIndex::default();
    let Some(codex_home) = sessions_root.parent() else {
        return index;
    };
    let Some(db_path) = find_state_db(codex_home) else {
        return index;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .or_else(|_| rusqlite::Connection::open(&db_path));
    let Ok(conn) = conn else {
        return index;
    };

    // 列存在性探测：state DB 的 schema 未文档化，缺列时降级而不是报错。
    let columns: HashSet<String> = conn
        .prepare("PRAGMA table_info(threads)")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            Ok(rows.flatten().collect())
        })
        .unwrap_or_default();
    if !columns.contains("id") {
        return index;
    }
    let has_name = columns.contains("name");
    let has_title = columns.contains("title");
    let has_project_id = columns.contains("project_id");
    let has_source = columns.contains("thread_source");

    let project_names: HashMap<String, String> = if has_project_id {
        conn.prepare("SELECT id, name FROM projects")
            .and_then(|mut stmt| {
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                Ok(rows
                    .flatten()
                    .map(|(id, name)| (id, name.trim().to_string()))
                    .filter(|(_, name)| !name.is_empty())
                    .collect())
            })
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    let select_cols = [
        "id",
        if has_name { "name" } else { "NULL" },
        if has_title { "title" } else { "NULL" },
        if has_project_id { "project_id" } else { "NULL" },
        if has_source { "thread_source" } else { "NULL" },
    ]
    .join(", ");
    if let Ok(mut stmt) = conn.prepare(&format!("SELECT {select_cols} FROM threads")) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        }) {
            for row in rows.flatten() {
                let (thread_id, name, title, project_id, source) = row;
                if let Some(source) = source.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    index.sources.insert(thread_id.clone(), source.to_string());
                }
                let best = name
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .or_else(|| title.as_deref().map(str::trim).filter(|t| !t.is_empty()));
                if let Some(best) = best {
                    index.titles.insert(thread_id.clone(), best.to_string());
                }
                if let Some(project_name) =
                    project_id.as_deref().and_then(|pid| project_names.get(pid))
                {
                    index
                        .projects
                        .entry(thread_id)
                        .or_insert_with(|| project_name.clone());
                }
            }
        }
    }
    index
}

/// 扫描全量 rollout 目录，返回所有模型不匹配的 thread 列表。
///
/// 判定是线程级的：以最新分片（mtime 最大）的权威模型为准——Codex 恢复
/// 任务只读最新分片。历史分片里的旧模型
/// 字段不影响判定，只在明细里展示。
pub fn scan_model_mismatches(sessions_root: &Path, info: &ModelInfo) -> Vec<TaskModelReport> {
    let mut thread_map: HashMap<String, ScannedThread> = HashMap::new();

    let mut stack = vec![sessions_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "jsonl")
                && !p.to_string_lossy().contains(".bak")
            {
                // 扫描必须保持只读，且不能把 70MB 级 rollout 整文件读进内存。
                // BufReader::lines 的峰值内存约等于最长一行，而不是整个文件。
                let Ok(file) = File::open(&p) else {
                    continue;
                };
                let mut thread_id = String::new();
                let mut project: Option<String> = None;
                let mut first_user_message: Option<String> = None;
                let mut mismatched: Vec<(String, String)> = Vec::new();
                let mut seen_fields: HashSet<String> = HashSet::new();
                let mut last_applied_model: Option<String> = None;
                let mut last_turn_context_model: Option<String> = None;
                let mut unparseable_authority: usize = 0;

                for line in BufReader::new(file).lines() {
                    let Ok(line) = line else {
                        continue;
                    };
                    let Ok(record) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(payload) = record.get("payload") else {
                        continue;
                    };

                    if thread_id.is_empty() {
                        if let Some(id) = extract_thread_id(payload) {
                            thread_id = id.to_string();
                        }
                    }
                    if is_thread_settings_applied(&record, payload) {
                        if has_parseable_rfc3339_timestamp(&record) {
                            if let Some(model) = payload
                                .pointer("/thread_settings/model")
                                .and_then(Value::as_str)
                            {
                                last_applied_model = Some(model.to_string());
                            }
                        } else {
                            unparseable_authority += 1;
                        }
                    }
                    // 项目名：session_meta.cwd 的目录名（turn_context.cwd 兜底，
                    // 两者指向同一工作目录）。
                    if project.is_none() {
                        if let Some(cwd) = payload.pointer("/cwd").and_then(Value::as_str) {
                            // 必须用共享的跨平台解析：cwd 是写入端平台的写法，
                            // 宿主 Path::file_name() 在 macOS 上会把整条
                            // `F:\\Projects\\X` 当成一个文件名（跨平台写入端的真实形态）。
                            project = crate::unified_scan::project_name_from_cwd(cwd);
                        }
                    }
                    // 标题回退链第二档：首条用户消息前 40 字。
                    if first_user_message.is_none()
                        && record.get("type").and_then(Value::as_str) == Some("response_item")
                        && payload.get("type").and_then(Value::as_str) == Some("message")
                        && payload.get("role").and_then(Value::as_str) == Some("user")
                    {
                        if let Some(text) = payload
                            .pointer("/content/0/text")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                        {
                            first_user_message = Some(text.chars().take(40).collect::<String>());
                        }
                    }
                    if record.get("type").and_then(Value::as_str) == Some("turn_context") {
                        if let Some(model) = payload.pointer("/model").and_then(Value::as_str) {
                            last_turn_context_model = Some(model.to_string());
                        }
                    }
                    for (field, model) in extract_model_fields(&record, payload) {
                        if !is_in_catalog(&model, info) && seen_fields.insert(field.clone()) {
                            mismatched.push((field, model));
                        }
                    }
                }

                // 线程级判定需要看到该线程的所有分片（最新分片可能完全干净，
                // 但它决定 Codex 实际恢复的模型），所以这里按 thread 全量收集，
                // 判定延后到聚合阶段。
                if !thread_id.is_empty() {
                    let entry = thread_map.entry(thread_id).or_default();
                    if entry.project.is_none() {
                        entry.project = project;
                    }
                    if entry.first_user_message.is_none() {
                        entry.first_user_message = first_user_message;
                    }
                    entry.files.push(ScannedFile {
                        path: p.clone(),
                        modified: p.metadata().and_then(|m| m.modified()).ok(),
                        fields: mismatched,
                        last_applied_model,
                        last_turn_context_model,
                        unparseable_authority,
                    });
                }
            }
        }
    }

    // 展示信息第一档：app-server 状态库（threads.name → title；projects.name
    // 连表取项目名）。global-state JSON 的自动摘要、rollout 首条用户消息、
    // cwd 目录名依次作为回退。全部只读、全容错。
    let state_db = load_state_db_index(sessions_root);
    let sidebar_titles = load_sidebar_titles(sessions_root);

    let mut tasks: Vec<TaskModelReport> = thread_map
        .into_iter()
        .filter_map(|(thread_id, scanned)| {
            let mismatch_count: usize = scanned.files.iter().map(|f| f.fields.len()).sum();
            if mismatch_count == 0 {
                return None;
            }
            // 线程级权威判定：Codex 恢复任务只读
            // 最新分片（mtime 最大），权威依次取：
            //   1. 该分片最后一条可解析 thread_settings_applied 的模型；
            //   2. 该分片最后一条 turn_context.model（resume 的实际回退）；
            // 都没有则无法判定，不报（避免修不了还挂着）。历史分片里的旧模型
            // 字段只用于展示，不再参与判定——实证：app-server resume 只往最新
            // 分片追加权威记录，按分片判定会让根分片旧权威把线程永远挂在列表上。
            let latest = scanned.files.iter().max_by_key(|f| f.modified);
            let authority = latest.and_then(|f| {
                f.last_applied_model
                    .clone()
                    .or_else(|| f.last_turn_context_model.clone())
            });
            let current_model = authority?;
            if is_in_catalog(&current_model, info) {
                return None;
            }

            let mut files: Vec<ScannedFile> = scanned
                .files
                .into_iter()
                .filter(|f| !f.fields.is_empty())
                .collect();
            files.sort_by(|a, b| a.path.cmp(&b.path));

            let mut file_reports = Vec::new();
            for file in files {
                file_reports.push(FileModelReport {
                    path: file.path,
                    fields: file.fields.into_iter().map(|(f, _)| f).collect(),
                    changed_fields: Vec::new(),
                    backup_path: None,
                    unparseable_authority: file.unparseable_authority,
                    error: None,
                });
            }

            let unparseable_authority = file_reports.iter().map(|f| f.unparseable_authority).sum();
            // 标题回退链：侧边栏标题 → 首条用户消息；都没有则留 None，
            // 由前端回退到短 thread id。
            let title = state_db
                .titles
                .get(&thread_id)
                .cloned()
                .or_else(|| sidebar_titles.get(&thread_id).cloned())
                .or(scanned.first_user_message);
            let project = state_db
                .projects
                .get(&thread_id)
                .cloned()
                .or(scanned.project);
            let thread_source = state_db.sources.get(&thread_id).cloned();
            Some(TaskModelReport {
                thread_id,
                title,
                project,
                thread_source,
                current_model,
                target_model: info.default_model.clone(),
                mismatch_count,
                unparseable_authority,
                files: file_reports,
            })
        })
        .collect();
    tasks.sort_by(|a, b| {
        b.mismatch_count
            .cmp(&a.mismatch_count)
            .then_with(|| a.thread_id.cmp(&b.thread_id))
    });
    tasks
}
