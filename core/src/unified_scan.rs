//! 修复页统一扫描：每个任务汇总三类可离线修复的问题。
//!
//! - 坏条目（孤儿注入 / 旧形状搜索 / 空文本 / 空 reasoning / 工具回合）：
//!   复用 rewrite 判定，遍历 rollout 计数；
//! - 模型不匹配：复用 model_repair 的权威模型扫描；
//! - 目录失效（B7）：roots_repair 扫描全局状态的 writable roots。
//!
//! 单线程（深链 / id）扫描 = 全量扫描 + 过滤，保持单一代码路径不发散。

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::model_repair::{load_display_index, scan_model_mismatches};
use crate::repair::{read_history_base, shard_ref_id};
use crate::rewrite::ModelInfo;
use crate::roots_repair::{scan_stale_roots, StaleRoot};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedTask {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 坏条目计数（五类确定性规则合计：孤儿注入 / 旧搜索 / 空文本 / 空 reasoning /
    /// 工具回合）。
    pub bad_entries: usize,
    /// 模型问题：权威模型缺失/非法时的当前值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_mismatch: Option<String>,
    /// 目录失效明细。
    pub stale_roots: Vec<StaleRoot>,
    /// 血缘断链明细（history_base 偏移越过父分片 EOF / 父分片缺失）。
    pub lineage_breaks: Vec<LineageBreak>,
}

/// 一条血缘断链。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineageBreak {
    /// 断链子分片文件名。
    pub shard: String,
    /// 引用的父分片 id。
    pub parent_ref: String,
    /// "offset-past-parent" / "parent-missing"
    pub reason: String,
}

impl UnifiedTask {
    pub fn has_issues(&self) -> bool {
        self.bad_entries > 0
            || self.model_mismatch.is_some()
            || !self.stale_roots.is_empty()
            || !self.lineage_breaks.is_empty()
    }
}

#[derive(Default)]
struct Entry {
    title: Option<String>,
    project: Option<String>,
    bad_entries: usize,
    model_mismatch: Option<String>,
    stale_roots: Vec<StaleRoot>,
    lineage_breaks: Vec<LineageBreak>,
}

/// cwd → 项目名（最后一段目录名）。
///
/// 必须同时认 `\` 与 `/`：rollout 里的 cwd 是**写入端平台**的写法
/// （Windows 会话写 `F:\Projects\X`），用宿主 `Path::file_name()` 解析会在
/// macOS 上把整条路径当成一个文件名（跨平台写入端形态）。
pub fn project_name_from_cwd(cwd: &str) -> Option<String> {
    let trimmed = cwd.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .rsplit(['\\', '/'])
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// 从 rollout 第一行 session_meta 提取 thread id 与项目名（cwd 目录名）。
fn extract_thread_meta(path: &Path) -> Option<(String, Option<String>)> {
    let file = std::fs::File::open(path).ok()?;
    let line = BufReader::new(file).lines().next()?.ok()?;
    let record: Value = serde_json::from_str(&line).ok()?;
    let payload = record.get("payload")?;
    let thread_id = payload
        .get("id")
        .or_else(|| payload.get("session_id"))
        .and_then(Value::as_str)
        .map(str::to_string)?;
    let project = payload
        .get("cwd")
        .and_then(Value::as_str)
        .and_then(project_name_from_cwd);
    Some((thread_id, project))
}

/// 全量统一扫描。
pub fn scan_all(codex_home: &Path, sessions_root: &Path, info: &ModelInfo) -> Vec<UnifiedTask> {
    let display = load_display_index(sessions_root);
    let mut map: HashMap<String, Entry> = HashMap::new();

    // 1) 模型不匹配（复用已验证的聚合扫描）
    for task in scan_model_mismatches(sessions_root, info) {
        let entry = map.entry(task.thread_id.clone()).or_default();
        entry.model_mismatch = Some(task.current_model.clone());
        entry.title = entry.title.take().or(task.title);
        entry.project = entry.project.take().or(task.project);
    }

    // 2) 坏条目计数（遍历全部 rollout 分片），顺带收集血缘校验素材：
    //    ref_id → 文件大小（父分片索引）与每个分片的 history_base 链接。
    let mut shard_sizes: HashMap<String, u64> = HashMap::new();
    let mut links: Vec<(String, String, u64, String)> = Vec::new(); // (thread_id, parent_ref, offset, shard_name)
    let mut stack = vec![sessions_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if !p.extension().is_some_and(|e| e == "jsonl") || p.to_string_lossy().contains(".bak")
            {
                continue;
            }
            if let Some(ref_id) = shard_ref_id(&p) {
                let size = p.metadata().map(|m| m.len()).unwrap_or(0);
                shard_sizes.entry(ref_id).or_insert(size);
            }
            let Some((thread_id, project)) = extract_thread_meta(&p) else {
                continue;
            };
            if let Some((base_ref, _ord, off)) = read_history_base(&p) {
                links.push((
                    thread_id.clone(),
                    base_ref,
                    off,
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                ));
            }
            let scan = crate::repair::scan_file(&p);
            if scan.total() == 0 {
                continue;
            }
            let entry = map.entry(thread_id).or_default();
            if entry.project.is_none() {
                entry.project = project;
            }
            entry.bad_entries += scan.total();
        }
    }

    // 2b) 血缘断链检测（③）：沿 history_base 链接校验「父分片存在且
    // end_byte_offset 不越界」。遍历所有分片即覆盖整条链（每个分片各带
    // 一条指向父的链接）。断链会让 Codex 启动校验拒绝加载整个线程。
    for (thread_id, parent_ref, offset, shard_name) in links {
        match shard_sizes.get(&parent_ref) {
            None => {
                map.entry(thread_id)
                    .or_default()
                    .lineage_breaks
                    .push(LineageBreak {
                        shard: shard_name,
                        parent_ref,
                        reason: "parent-missing".to_string(),
                    });
            }
            Some(&parent_size) if offset > parent_size => {
                map.entry(thread_id)
                    .or_default()
                    .lineage_breaks
                    .push(LineageBreak {
                        shard: shard_name,
                        parent_ref,
                        reason: "offset-past-parent".to_string(),
                    });
            }
            _ => {}
        }
    }

    // 3) 目录失效（全局状态 writable roots）
    for report in scan_stale_roots(codex_home) {
        let entry = map.entry(report.thread_id).or_default();
        entry.stale_roots = report.stale;
    }

    // 合并 + 展示信息 + 过滤无问题任务
    let mut tasks: Vec<UnifiedTask> = map
        .into_iter()
        .map(|(thread_id, entry)| UnifiedTask {
            title: display.titles.get(&thread_id).cloned().or(entry.title),
            project: display.projects.get(&thread_id).cloned().or(entry.project),
            thread_id,
            bad_entries: entry.bad_entries,
            model_mismatch: entry.model_mismatch,
            stale_roots: entry.stale_roots,
            lineage_breaks: entry.lineage_breaks,
        })
        .filter(UnifiedTask::has_issues)
        .collect();
    tasks.sort_by(|a, b| {
        a.project
            .cmp(&b.project)
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.thread_id.cmp(&b.thread_id))
    });
    tasks
}

/// 指定任务（深链 / thread id / rollout 路径）扫描 = 全量 + 过滤。
pub fn scan_one(
    codex_home: &Path,
    sessions_root: &Path,
    info: &ModelInfo,
    input: &str,
) -> Option<UnifiedTask> {
    let thread_id = crate::repair::parse_thread_id(input);
    scan_all(codex_home, sessions_root, info)
        .into_iter()
        .find(|task| task.thread_id == thread_id)
}
