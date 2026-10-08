//! 模型切换自动修复（模块 `auto_repair`）。
//!
//! 流程：
//! 监听 config.toml 默认模型变化 → 扫描不匹配线程并入队 →
//! Codex 未运行时经 app-server `thread/resume` 官方路径执行。
//! 无开关：检测与排队常开；排队/立即执行由进程状态决定；
//! 队列持久化，Halcyon 重启不丢；执行前用户可在面板取消。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::appserver::AppServerClient;
use crate::model_repair::scan_model_mismatches;
use crate::rewrite::ModelInfo;
use crate::roots_repair::repair_stale_roots;
use crate::unified_scan::UnifiedTask;

/// 有项目归属的 thread id 集合（global-state 的 `thread-project-assignments`
/// → `local-projects`）。自动修复只处理这些线程（
/// 无项目的散装线程不自动改，避免动到临时/一次性会话）。
pub fn project_thread_ids(codex_home: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Ok(text) = std::fs::read_to_string(codex_home.join(".codex-global-state.json")) else {
        return ids;
    };
    // 容错：某些写入方（PowerShell Set-Content 等）会带 UTF-8 BOM。
    let Ok(state) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else {
        return ids;
    };
    let mut projects: HashSet<String> = HashSet::new();
    if let Some(local) = state.get("local-projects") {
        let items: Vec<&Value> = match local {
            Value::Array(list) => list.iter().collect(),
            Value::Object(map) => map.values().collect(),
            _ => Vec::new(),
        };
        for project in items {
            if let Some(id) = project.get("id").and_then(Value::as_str) {
                projects.insert(id.to_string());
            }
        }
    }
    if let Some(assign) = state
        .get("thread-project-assignments")
        .and_then(Value::as_object)
    {
        for (thread_id, value) in assign {
            if let Some(project_id) = value.get("projectId").and_then(Value::as_str) {
                if projects.contains(project_id) {
                    ids.insert(thread_id.clone());
                }
            }
        }
    }
    ids
}

/// 自动修复范围过滤（模型切换触发的批量修复专用）：
/// 只保留「有项目归属」且非自动审查线程的任务。手动修复不受此限制。
pub fn filter_for_auto_repair(
    sessions_root: &Path,
    tasks: Vec<crate::model_repair::TaskModelReport>,
) -> Vec<crate::model_repair::TaskModelReport> {
    let Some(home) = sessions_root.parent() else {
        return Vec::new();
    };
    let owned = project_thread_ids(home);
    tasks
        .into_iter()
        .filter(|task| {
            owned.contains(&task.thread_id)
                && task.thread_source.as_deref()
                    != Some(crate::model_repair::GUARDIAN_REVIEW_SOURCE)
        })
        .collect()
}

const QUEUE_FILE: &str = "auto-repair-queue.json";
/// 修复历史：每次执行一条记录（JSONL 追加），供「记录」页检索。
const HISTORY_FILE: &str = "repair-history.jsonl";

/// 一条修复历史记录：任务 + 原因（kinds）+ 结果 + 明细 + 时间。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepairRecord {
    pub thread_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    pub kinds: RepairKinds,
    /// "done" / "failed"
    pub outcome: String,
    /// 逐类明细（改写条数、备份路径、目标模型、错误信息等）。
    #[serde(default)]
    pub details: Vec<String>,
    /// 本地时间（yyyy-MM-dd HH:mm:ss）。
    pub executed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum QueueStatus {
    Pending,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub thread_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    pub status: QueueStatus,
    /// 该项要执行的修复类型；旧队列文件无此字段时按「仅模型」兼容。
    #[serde(default = "RepairKinds::model_only")]
    pub kinds: RepairKinds,
}

/// 一次修复要执行的类型集合（「一次修复执行所有适用类型」）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RepairKinds {
    pub model: bool,
    pub entries: bool,
    pub roots: bool,
    /// 血缘断链回填（旧队列文件无此字段时按 false 兼容）。
    #[serde(default)]
    pub lineage: bool,
}

impl RepairKinds {
    pub fn model_only() -> Self {
        Self {
            model: true,
            entries: false,
            roots: false,
            lineage: false,
        }
    }

    pub fn all() -> Self {
        Self {
            model: true,
            entries: true,
            roots: true,
            lineage: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.model && !self.entries && !self.roots && !self.lineage
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoRepairQueue {
    /// 本队列的目标模型（触发时的默认模型）。
    #[serde(default)]
    pub target_model: String,
    #[serde(default)]
    pub items: Vec<QueueItem>,
}

impl AutoRepairQueue {
    pub fn pending_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == QueueStatus::Pending)
            .count()
    }
}

pub struct AutoRepair {
    data_dir: PathBuf,
    pub queue: AutoRepairQueue,
    /// 上次见到的默认模型；None 表示尚未建立基线（首次 tick 只记录不触发）。
    pub last_default_model: Option<String>,
}

impl AutoRepair {
    pub fn load(data_dir: &Path) -> Self {
        let mut queue = std::fs::read_to_string(data_dir.join(QUEUE_FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<AutoRepairQueue>(&text).ok())
            .unwrap_or_default();
        // 队列不变量：队列只放待执行项。
        // 执行结果（成功/失败）一律写修复历史，不在队列沉淀；
        // 旧版队列文件里积累的 Done/Failed/Cancelled 在加载时清掉。
        queue.items.retain(|i| i.status == QueueStatus::Pending);
        Self {
            data_dir: data_dir.to_path_buf(),
            queue,
            last_default_model: None,
        }
    }

    pub fn save(&self) {
        let path = self.data_dir.join(QUEUE_FILE);
        if let Ok(text) = serde_json::to_string_pretty(&self.queue) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    /// 追加一条修复历史（JSONL，一行一条，新记录在前端倒序展示）。
    pub fn append_history(&self, record: &RepairRecord) {
        if let Ok(mut line) = serde_json::to_string(record) {
            line.push('\n');
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.data_dir.join(HISTORY_FILE))
            {
                use std::io::Write;
                let _ = file.write_all(line.as_bytes());
            }
        }
    }

    /// 读取修复历史（最新的在最后；读取失败返回空）。
    pub fn load_history(data_dir: &Path) -> Vec<RepairRecord> {
        std::fs::read_to_string(data_dir.join(HISTORY_FILE))
            .map(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str::<RepairRecord>(line).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 删除一条修复历史。`display_index` 是「最新在前」展示顺序里的下标
    ///（前端读到的是 reverse 后的列表，这里同样反转再删，删完写回文件顺序）。
    pub fn delete_history_at(data_dir: &Path, display_index: usize) -> Result<(), String> {
        let mut records = Self::load_history(data_dir);
        records.reverse();
        if display_index >= records.len() {
            return Err(format!("记录下标越界: {display_index}"));
        }
        records.remove(display_index);
        records.reverse();
        Self::write_history(data_dir, &records)
    }

    /// 清空全部修复历史，返回删除的条数。
    pub fn clear_history(data_dir: &Path) -> Result<usize, String> {
        let n = Self::load_history(data_dir).len();
        Self::write_history(data_dir, &[])?;
        Ok(n)
    }

    /// 整文件重写历史（临时文件 + rename，避免半写状态）。
    fn write_history(data_dir: &Path, records: &[RepairRecord]) -> Result<(), String> {
        let path = data_dir.join(HISTORY_FILE);
        let mut text = String::new();
        for record in records {
            let line = serde_json::to_string(record).map_err(|e| e.to_string())?;
            text.push_str(&line);
            text.push('\n');
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("写历史临时文件失败: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("替换历史文件失败: {e}"))?;
        Ok(())
    }

    /// 每个 tick 调用：返回 true 表示默认模型发生了变化（首次只建基线）。
    pub fn observe_default_model(&mut self, info: &ModelInfo) -> bool {
        if info.default_model.is_empty() {
            return false;
        }
        match &self.last_default_model {
            None => {
                self.last_default_model = Some(info.default_model.clone());
                false
            }
            Some(last) if *last == info.default_model => false,
            Some(_) => {
                self.last_default_model = Some(info.default_model.clone());
                true
            }
        }
    }

    /// 用已扫描好的结果替换 Pending 队列（旧 Pending 丢弃，
    /// Done / Failed / Cancelled 历史保留）。返回新入队数量。
    ///
    /// 扫描由调用方在**锁外**完成——全量扫描需 30-40 秒，持锁期间会让
    /// 面板轮询与托盘读取全部阻塞。
    pub fn replace_pending(
        &mut self,
        target_model: &str,
        tasks: Vec<crate::model_repair::TaskModelReport>,
    ) -> usize {
        self.queue.target_model = target_model.to_string();
        self.queue
            .items
            .retain(|i| i.status != QueueStatus::Pending);
        let mut added = 0;
        for task in tasks {
            // 同一线程若已有非 Pending 条目则复活/复用，而不是再压一行——
            // 否则失败条目会和新的 Pending 同时出现在队列卡里。
            if let Some(existing) = self
                .queue
                .items
                .iter_mut()
                .find(|i| i.thread_id == task.thread_id)
            {
                match existing.status {
                    // 用户明确取消过的，模型再切换也不自动复活
                    QueueStatus::Cancelled => {}
                    // 失败重试 / 上次修完后模型又变了
                    QueueStatus::Failed(_) | QueueStatus::Done => {
                        existing.status = QueueStatus::Pending;
                        if task.title.is_some() {
                            existing.title = task.title.clone();
                        }
                        if task.project.is_some() {
                            existing.project = task.project.clone();
                        }
                        added += 1;
                    }
                    QueueStatus::Pending => {}
                }
                continue;
            }
            self.queue.items.push(QueueItem {
                thread_id: task.thread_id,
                title: task.title,
                project: task.project,
                status: QueueStatus::Pending,
                kinds: RepairKinds::model_only(),
            });
            added += 1;
        }
        added
    }

    /// 扫描并入队（内部完成扫描；供测试与一次性调用方使用）。
    /// 生产路径用 [`Self::replace_pending`]：扫描必须在锁外做。
    pub fn plan(&mut self, sessions_root: &Path, info: &ModelInfo) -> usize {
        let tasks = scan_model_mismatches(sessions_root, info);
        let tasks = filter_for_auto_repair(sessions_root, tasks);
        self.replace_pending(&info.default_model, tasks)
    }

    /// 修复页手动修复：按统一扫描结果入队（全类型）。
    /// 已在 Pending 的线程做类型并集；Failed 的线程复活为 Pending（重试）；
    /// 不重复入队。返回新增/合并的项数。
    pub fn enqueue_unified(&mut self, target_model: &str, tasks: &[UnifiedTask]) -> usize {
        if !target_model.is_empty() {
            self.queue.target_model = target_model.to_string();
        }
        let mut touched = 0;
        for task in tasks {
            // 方针：坏条目（对话内容形态）只走请求出口实时改写，
            // 不做静态修复——rollout 是 Codex 的事实源，破坏性改写会引发
            // 血缘偏移失效、投影库不一致。所以 entries 不再因扫描入队；
            // 仅保留旧队列文件里已有 entries 项的兼容展示。
            let kinds = RepairKinds {
                model: task.model_mismatch.is_some(),
                entries: false,
                roots: !task.stale_roots.is_empty(),
                lineage: !task.lineage_breaks.is_empty(),
            };
            if kinds.is_empty() {
                continue;
            }
            if let Some(existing) = self.queue.items.iter_mut().find(|i| {
                i.thread_id == task.thread_id
                    && matches!(i.status, QueueStatus::Pending | QueueStatus::Failed(_))
            }) {
                existing.kinds.model |= kinds.model;
                existing.kinds.entries |= kinds.entries;
                existing.kinds.roots |= kinds.roots;
                existing.kinds.lineage |= kinds.lineage;
                existing.status = QueueStatus::Pending;
            } else {
                self.queue.items.push(QueueItem {
                    thread_id: task.thread_id.clone(),
                    title: task.title.clone(),
                    project: task.project.clone(),
                    status: QueueStatus::Pending,
                    kinds,
                });
            }
            touched += 1;
        }
        touched
    }

    /// 取消一个待执行项 = 直接从队列移除（「跳过本轮」语义）。
    /// 不再保留 Cancelled 状态：队列只含待执行项，结果看修复历史；
    /// 线程若仍有问题，下次模型切换的自动扫描会重新入队。
    pub fn cancel(&mut self, thread_id: &str) -> bool {
        let before = self.queue.items.len();
        self.queue
            .items
            .retain(|i| !(i.thread_id == thread_id && i.status == QueueStatus::Pending));
        self.queue.items.len() != before
    }

    /// 落盘前维持队列不变量：只留待执行项，已结算的（Done/Failed）移除。
    /// 结果在结算时已逐条写入修复历史（append_history），移除不丢信息。
    fn prune_and_save(&mut self) {
        self.queue
            .items
            .retain(|i| i.status == QueueStatus::Pending);
        self.save();
    }

    /// 执行全部 Pending 项（调用方须保证 Codex 未运行）。
    /// 逐项处理：每项按其 kinds 执行所有适用修复（目录失效 → 坏条目 → 模型），
    /// 全部成功才记 Done，任一失败记 Failed（各修复均幂等，可整体重试）。
    /// 每项开始前重新检测 Codex 进程：中途启动即中止，剩余项保持 Pending。
    /// `on_progress(settled, total, thread_id, ok)` 在每项结算后回调
    /// （托盘进度 + 面板逐项消项：ok=false 的项留在列表）。
    /// 返回 (done, failed)；逐项记录结果并落盘。
    pub fn execute_pending(
        &mut self,
        codex_exe: &Path,
        work_cwd: &Path,
        codex_home: &Path,
        on_progress: &mut dyn FnMut(usize, usize, &str, bool),
    ) -> (usize, usize) {
        let total = self.queue.pending_count();
        if total == 0 {
            return (0, 0);
        }
        // 队列项里的项目名/标题是入队那一刻写下的；用户可能之后改了项目归属
        // 或修了展示名解析。写历史前用展示索引刷新一次，避免记录里留下
        // `codex-threads-<id>` 这类无意义旧名。
        let display = crate::model_repair::load_display_index(&codex_home.join("sessions"));
        let mut client: Option<AppServerClient> = None;
        let mut settled = 0;
        let mut done = 0;
        let mut failed = 0;
        for i in 0..self.queue.items.len() {
            if self.queue.items[i].status != QueueStatus::Pending {
                continue;
            }
            // 修复中途用户打开了 Codex：中止剩余项，保持 Pending 留待下次。
            // 必须排除本函数自己拉起的 app-server 子进程：它也是 codex.exe，
            // 不排除的话第一项之后每一轮都会被误判为"Codex 已启动"而中止，
            // 结果变成每 3 秒只修一条、并重复弹通知。
            let spawned: Vec<u32> = client.as_ref().map(|c| vec![c.pid()]).unwrap_or_default();
            if is_codex_running(&spawned) {
                self.prune_and_save();
                return (done, failed);
            }
            let thread_id = self.queue.items[i].thread_id.clone();
            let kinds = self.queue.items[i].kinds;
            let mut errors: Vec<String> = Vec::new();
            let mut details: Vec<String> = Vec::new();

            if kinds.roots {
                match repair_stale_roots(codex_home, std::slice::from_ref(&thread_id)) {
                    Ok((0, _)) => details.push("目录失效：扫描已无需修复".to_string()),
                    Ok((n, backup)) => details.push(format!(
                        "目录失效：已处理 {n} 项（备份 {}）",
                        backup.file_name().unwrap_or_default().to_string_lossy()
                    )),
                    Err(e) => errors.push(format!("目录失效修复失败: {e}")),
                }
            }
            if kinds.entries && errors.is_empty() {
                // 旧队列里带入的 entries 项：不再离线改写（内容形态问题由出口
                // 实时改写覆盖，见 design.md 2.4.2），只记录说明。
                details.push("坏条目：实时保护中（出口改写覆盖，无需离线修复）".to_string());
            }
            if kinds.lineage && errors.is_empty() {
                let roots = [
                    codex_home.join("sessions"),
                    codex_home.join("archived_sessions"),
                ];
                let (fixes, lineage_errors) =
                    crate::repair::repair_lineage_breaks(&thread_id, &roots);
                if !fixes.is_empty() {
                    details.push(format!("血缘断链：回填 {} 处", fixes.len()));
                    details.extend(fixes);
                }
                errors.extend(lineage_errors);
            }
            if kinds.model && errors.is_empty() {
                if self.queue.target_model.is_empty() {
                    errors.push("目标模型为空，无法执行模型修复".to_string());
                } else {
                    if client.is_none() {
                        match AppServerClient::spawn(codex_exe, work_cwd) {
                            Ok(c) => client = Some(c),
                            Err(e) => {
                                for item in self.queue.items[i..].iter_mut() {
                                    if item.status == QueueStatus::Pending && item.kinds.model {
                                        item.status = QueueStatus::Failed(format!(
                                            "app-server 启动失败: {e}"
                                        ));
                                        failed += 1;
                                        settled += 1;
                                        let tid = item.thread_id.clone();
                                        on_progress(settled, total, &tid, false);
                                    }
                                }
                                self.prune_and_save();
                                return (done, failed);
                            }
                        }
                    }
                    if let Some(c) = client.as_mut() {
                        let target = self.queue.target_model.clone();
                        match c.resume_with_overrides(&thread_id, Some(&target), None) {
                            Ok(()) => details.push(format!("模型：已切换为 {target}")),
                            Err(e) => errors.push(format!("模型修复失败: {e}")),
                        }
                    }
                }
            }

            let outcome;
            if errors.is_empty() {
                self.queue.items[i].status = QueueStatus::Done;
                done += 1;
                outcome = "done".to_string();
            } else {
                let message = errors.join("；");
                details.push(format!("错误：{message}"));
                self.queue.items[i].status = QueueStatus::Failed(message);
                failed += 1;
                outcome = "failed".to_string();
            }
            let ok = outcome == "done";
            let display_title = display
                .titles
                .get(&thread_id)
                .cloned()
                .or_else(|| self.queue.items[i].title.clone());
            let display_project = display
                .projects
                .get(&thread_id)
                .cloned()
                .or_else(|| self.queue.items[i].project.clone());
            self.append_history(&RepairRecord {
                thread_id: thread_id.clone(),
                title: display_title,
                project: display_project,
                kinds: self.queue.items[i].kinds,
                outcome,
                details,
                executed_at: jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S").to_string(),
            });
            settled += 1;
            on_progress(settled, total, &thread_id, ok);
        }
        self.prune_and_save();
        (done, failed)
    }
}

/// Codex 进程分类：交互实例（桌面 / CLI，会持有线程状态）与
/// 后台辅助（沙箱服务、computer-use 等，不持有线程状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexProcessKind {
    Interactive,
    Helper,
    Other,
}

/// 按可执行文件名与路径分类（纯函数，便于测试）。
pub fn classify_codex_process(exe_path: &str) -> CodexProcessKind {
    let lower = exe_path.to_lowercase().replace('/', "\\");
    let file = lower.rsplit('\\').next().unwrap_or("");
    if file.starts_with("codex-windows-sandbox-service")
        || file.starts_with("codex-computer-use")
        || file.starts_with("halcyon")
    {
        return CodexProcessKind::Helper;
    }
    let is_codex = file == "codex.exe" || file == "codex" || file.starts_with("codex-");
    if !is_codex {
        return CodexProcessKind::Other;
    }
    if lower.contains("openai\\codex") || lower.contains("openai/codex") {
        return CodexProcessKind::Interactive;
    }
    CodexProcessKind::Other
}

/// 当前是否有交互态 Codex 进程在运行（排除指定 pid，如自身 spawn 的实例）。
pub fn is_codex_running(exclude_pids: &[u32]) -> bool {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    system.processes().iter().any(|(pid, proc_)| {
        if exclude_pids.contains(&pid.as_u32()) {
            return false;
        }
        proc_
            .exe()
            .and_then(|p| p.to_str())
            .map(|exe| classify_codex_process(exe) == CodexProcessKind::Interactive)
            .unwrap_or(false)
    })
}
