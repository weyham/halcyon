//! 救援被"坏条目"打挂的历史：扫描 / 修复 rollout 文件里的两类条目。1:1 移植 deploy/repair_rollout.py。
//!
//! 代理只在**请求边界**改写；本模块处理"历史文件里已经躺着坏条目"的情况
//! （例如代理还没上线时被打挂的线程）。判定与改写规则直接复用 rewrite.rs，
//! 保证与代理行为一致：
//!
//!   1. 孤儿注入条目（`function_call_output`，缺 `call_id`，有 `name`/`namespace`）
//!      → 改写成带来源标注的 user 消息；
//!   2. 旧形状 `web_search_call`（`action.query`，无 `queries`）
//!      → 改写成一条"历史记录"说明（默认），或 drop 直接删。
//!   3. 空文本消息 / 空 reasoning（跨 provider 加密残留）→ 整条丢弃；
//!   4. 工具回合夹层 / 缺 output → output 移到 call 后 / 补 `aborted`
//!      （检测规则复用 rewrite::detect_tool_round_issues，与请求出口一致）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::rewrite::{
    detect_tool_round_issues, is_empty_reasoning, is_empty_text_message, is_legacy_web_search,
    is_orphan_injected_output, orphan_to_user_message, web_search_note_message,
};

/// 会话文件根目录（`~/.codex`）。
fn codex_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .map(|h| h.join(".codex"))
}

fn sessions_roots() -> Vec<PathBuf> {
    let Some(home) = codex_home() else {
        return Vec::new();
    };
    vec![home.join("sessions"), home.join("archived_sessions")]
}

/// 从用户输入里提取 thread id：接受 `codex://threads/<id>`、裸 id、或 rollout 文件路径。
pub fn parse_thread_id(input: &str) -> String {
    let t = input.trim().trim_matches(['"', '\'']);
    if let Some(rest) = t.strip_prefix("codex://threads/") {
        return rest.trim_matches('/').to_string();
    }
    t.to_string()
}

fn is_rollout_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl") && !path.to_string_lossy().contains(".bak")
}

/// 在 sessions / archived_sessions 里找文件名包含 thread id 的 rollout（排除 .bak），
/// 按修改时间取最新一个。
pub fn find_rollout(target: &str) -> Result<PathBuf, String> {
    let as_path = Path::new(target);
    if as_path.is_file() {
        return Ok(as_path.to_path_buf());
    }
    let mut matches: Vec<PathBuf> = Vec::new();
    for root in sessions_roots() {
        if !root.is_dir() {
            continue;
        }
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if is_rollout_jsonl(&p)
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().contains(target))
                {
                    matches.push(p);
                }
            }
        }
    }
    if matches.is_empty() {
        return Err(format!("找不到 {target} 对应的 rollout 文件"));
    }
    matches.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
    Ok(matches.pop().unwrap())
}

/// 按 history_base 记录的分片 id 精确定位文件。
///
/// history_base.thread_id 有两种形态：主 tid（目标是根分片，文件名以
/// `-<tid>` 结尾、无后缀）或分片后缀 id（文件名以 `_<id>` 结尾）。
/// 不能用 contains：后缀分片的文件名同样包含主 tid，遍历顺序不定时
/// 会匹配到自己或兄弟分片，chain() 因此断在第一个文件（实证：
/// 修复只覆盖最新分片，旧分片的坏条目永远修不到）。
fn find_by_thread_id_in(thread_id: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    let root_suffix = format!("-{thread_id}");
    let shard_suffix = format!("_{thread_id}");
    let mut matches: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if is_rollout_jsonl(&p)
                    && p.file_name().is_some_and(|n| {
                        let name = n.to_string_lossy();
                        let Some(stem) = name.strip_suffix(".jsonl") else {
                            return false;
                        };
                        stem.ends_with(&root_suffix) || stem.ends_with(&shard_suffix)
                    })
                {
                    matches.push(p);
                }
            }
        }
    }
    matches.into_iter().next()
}

/// 沿 `history_base.thread_id` 往上找回所有分片（旧→新）。
pub fn chain(path: &Path) -> Vec<PathBuf> {
    chain_with_roots(path, &sessions_roots())
}

/// 同 [`chain`]，但会话根目录可注入（测试与内部用）。
pub fn chain_with_roots(path: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut current: Option<PathBuf> = Some(path.to_path_buf());
    while let Some(cur) = current {
        if files.contains(&cur) {
            break;
        }
        files.push(cur.clone());
        let Ok(first) =
            std::fs::read_to_string(&cur).map(|s| s.lines().next().unwrap_or("").to_string())
        else {
            break;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&first) else {
            break;
        };
        let Some(thread_id) = meta
            .get("payload")
            .and_then(|p| p.get("history_base"))
            .and_then(|b| b.get("thread_id"))
            .and_then(Value::as_str)
        else {
            break;
        };
        current = find_by_thread_id_in(thread_id, roots);
    }
    files.reverse();
    files
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub orphan: usize,
    pub web_search: usize,
    pub empty_message: usize,
    pub empty_reasoning: usize,
    pub tool_rounds: usize,
    /// 多智能体 agent_message（严格上游拒绝，归一为 assistant 消息）。
    pub agent_message: usize,
    /// 超大图片输出条目（function_call_output 含 base64 input_image，且条目
    /// 序列化体积 > 64 KB）。可离线瘦身（占位替换），不计入 total()——
    /// total() 驱动「实时保护」chip，图片瘦身是独立的可离线修复类。
    pub image_outputs: usize,
    /// 前几条样本（行号 + id/name 或 query），供结果展示。
    pub samples: Vec<String>,
}

impl ScanResult {
    /// 全部坏条目合计（五类确定性规则）。
    pub fn total(&self) -> usize {
        self.orphan
            + self.web_search
            + self.empty_message
            + self.empty_reasoning
            + self.tool_rounds
            + self.agent_message
    }
}

fn payload_of(line: &str) -> Option<Value> {
    if !line.contains("\"response_item\"") {
        return None;
    }
    let record = serde_json::from_str::<Value>(line).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    record.get("payload").cloned()
}

/// 扫描一个文件里的坏条目。
pub fn scan_file(path: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return result;
    };
    let mut payloads: Vec<Value> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let Some(payload) = payload_of(line) else {
            continue;
        };
        if is_orphan_injected_output(&payload) {
            result.orphan += 1;
            if result.samples.len() < 5 {
                result.samples.push(format!(
                    "行 {}: 孤儿注入 id={} name={}",
                    number + 1,
                    payload.get("id").and_then(Value::as_str).unwrap_or("?"),
                    payload.get("name").and_then(Value::as_str).unwrap_or("?"),
                ));
            }
        } else if is_legacy_web_search(&payload) {
            result.web_search += 1;
            if result.samples.len() < 5 {
                let q = payload
                    .get("action")
                    .and_then(|a| a.get("query"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                result
                    .samples
                    .push(format!("行 {}: 旧搜索 query={:?}", number + 1, q));
            }
        } else if is_empty_text_message(&payload) {
            result.empty_message += 1;
            if result.samples.len() < 5 {
                result
                    .samples
                    .push(format!("行 {}: 空文本消息", number + 1));
            }
        } else if crate::rewrite::is_agent_message(&payload) {
            result.agent_message += 1;
            if result.samples.len() < 5 {
                result.samples.push(format!(
                    "行 {}: agent_message author={}",
                    number + 1,
                    payload.get("author").and_then(Value::as_str).unwrap_or("?"),
                ));
            }
        } else if is_empty_reasoning(&payload) {
            result.empty_reasoning += 1;
            if result.samples.len() < 5 {
                result
                    .samples
                    .push(format!("行 {}: 空 reasoning", number + 1));
            }
        }
        if crate::rewrite::is_image_output(&payload) && line.len() > 64 * 1024 {
            result.image_outputs += 1;
            if result.samples.len() < 5 {
                let cid = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                result.samples.push(format!(
                    "行 {}: 图片输出 call_id={cid}（{:.1} MB）",
                    number + 1,
                    line.len() as f64 / 1024.0 / 1024.0
                ));
            }
        }
        payloads.push(payload);
    }
    result.tool_rounds = detect_tool_round_issues(&payloads).len();
    if result.tool_rounds > 0 && result.samples.len() < 5 {
        result
            .samples
            .push(format!("工具回合需修补 {} 处", result.tool_rounds));
    }
    result
}

/// 改写单个条目；`None` 表示删除。五类确定性规则（与请求出口同源）：
/// 孤儿注入 → user 消息；旧搜索 → note/drop；空文本消息 / 空 reasoning → 删除。
fn rewrite_item(item: &Value, web_search: &str) -> Option<Value> {
    if crate::rewrite::is_agent_message(item) {
        return Some(crate::rewrite::agent_message_to_message(item));
    }
    if is_orphan_injected_output(item) {
        return Some(orphan_to_user_message(item, true));
    }
    if is_legacy_web_search(item) {
        return if web_search == "drop" {
            None
        } else {
            Some(web_search_note_message(item))
        };
    }
    if is_empty_text_message(item) || is_empty_reasoning(item) {
        return None;
    }
    Some(item.clone())
}

/// 修复一个文件；`apply=false` 只统计不落盘。返回 (改了几条, 删了几条, 备份路径)。
///
/// 逐项变换（孤儿/旧搜索/空文本/空 reasoning）之后，再做一次**序列级**的
/// 工具回合修补（检测复用 rewrite::detect_tool_round_issues）：夹层 output
/// 移到 call 正后方，缺 output 补 `aborted`——移动与新增都以新行写入，
/// 未改动的行保持原始字节不动。
pub fn repair_file(
    path: &Path,
    web_search: &str,
    apply: bool,
) -> Result<(usize, usize, Option<PathBuf>), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读取失败 {}: {e}", path.display()))?;
    let lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();

    // 收集 response_item 槽位：行号 + 信封 + payload
    let mut slot_lines: Vec<usize> = Vec::new();
    let mut envelopes: Vec<Value> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(payload) = payload_of(line) else {
            continue;
        };
        let record = serde_json::from_str::<Value>(line.trim_end()).map_err(|e| e.to_string())?;
        slot_lines.push(i);
        envelopes.push(record);
        items.push(payload);
    }

    // 1) 逐项变换
    let mut changed = 0usize;
    let mut dropped = 0usize;
    let mut slot_dropped = vec![false; items.len()];
    let mut slot_changed = vec![false; items.len()];
    for (i, payload) in items.iter_mut().enumerate() {
        let before = payload.clone();
        match rewrite_item(&before, web_search) {
            None => {
                slot_dropped[i] = true;
                dropped += 1;
            }
            Some(new_payload) => {
                if new_payload != before {
                    *payload = new_payload;
                    slot_changed[i] = true;
                    changed += 1;
                }
            }
        }
    }

    // 2) 工具回合序列修补（在存活条目上检测）
    let surviving: Vec<usize> = (0..items.len()).filter(|i| !slot_dropped[*i]).collect();
    let surviving_payloads: Vec<&Value> = surviving.iter().map(|i| &items[*i]).collect();
    let fixes = detect_tool_round_issues(
        &surviving_payloads
            .iter()
            .map(|v| (*v).clone())
            .collect::<Vec<_>>(),
    );
    let mut extras_after: std::collections::HashMap<usize, Vec<Value>> =
        std::collections::HashMap::new();
    for fix in fixes {
        let call_slot = surviving[fix.call_idx];
        match fix.output_idx {
            Some(output_pos) => {
                let output_slot = surviving[output_pos];
                slot_dropped[output_slot] = true;
                extras_after
                    .entry(call_slot)
                    .or_default()
                    .push(items[output_slot].clone());
            }
            None => {
                extras_after
                    .entry(call_slot)
                    .or_default()
                    .push(crate::rewrite::synthetic_aborted_output(&items[call_slot]));
            }
        }
        changed += 1;
    }

    let mut backup = None;
    if apply && (changed > 0 || dropped > 0) {
        // 重建：非条目行原样；未改动槽位原样；改动槽位重序列化；
        // 移动/新增的条目作为新行跟在对应 call 槽位之后。
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        let mut slot_of_line: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        for (slot, line_idx) in slot_lines.iter().enumerate() {
            slot_of_line.insert(*line_idx, slot);
        }
        for (i, line) in lines.iter().enumerate() {
            let Some(&slot) = slot_of_line.get(&i) else {
                out.push(line.clone());
                continue;
            };
            if slot_dropped[slot] {
                continue;
            }
            if slot_changed[slot] {
                let mut record = envelopes[slot].clone();
                record["payload"] = items[slot].clone();
                out.push(serde_json::to_string(&record).map_err(|e| e.to_string())? + "\n");
            } else {
                out.push(line.clone());
            }
            if let Some(extras) = extras_after.get(&slot) {
                for extra in extras {
                    let mut record = envelopes[slot].clone();
                    record["payload"] = extra.clone();
                    out.push(serde_json::to_string(&record).map_err(|e| e.to_string())? + "\n");
                }
            }
        }
        let stamp = chrono_stamp();
        let bak = path.with_file_name(format!(
            "{}.bak-{stamp}",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::copy(path, &bak).map_err(|e| format!("备份失败 {}: {e}", bak.display()))?;
        std::fs::write(path, out.concat())
            .map_err(|e| format!("写入失败 {}: {e}", path.display()))?;
        backup = Some(bak);
    }
    Ok((changed, dropped, backup))
}

/// 图片瘦身（离线）结果。
#[derive(Debug, Default)]
pub struct SlimImagesReport {
    /// 被改写的分片。
    pub files: Vec<PathBuf>,
    /// 替换为占位文本的图片条数。
    pub slimmed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub backups: Vec<PathBuf>,
    /// 血缘回填明细（瘦身改变分片字节长度后必须回填子分片偏移）。
    pub lineage_backfills: Vec<String>,
    pub lineage_errors: Vec<String>,
}

/// 离线图片瘦身：把线程 rollout 里的 `input_image`（`view_image` 的 base64
/// 结果）按时间从旧到新替换为占位文本，保留最近 `keep_recent` 张。
///
/// 与请求出口的实时瘦身（rewrite::slim_oversized_images）互补：出口只救当下
/// 这一次请求，rollout 本体不变，下一次请求要重做一遍；离线修复直接改写
/// rollout（备份 + 血缘回填），一劳永逸。仅处理 Codex 未运行时（同其它
/// 静态修复的窗口约束）。
pub fn slim_thread_images(
    thread_id: &str,
    roots: &[PathBuf],
    keep_recent: usize,
) -> Result<SlimImagesReport, String> {
    let mut report = SlimImagesReport::default();
    let files = collect_thread_files(roots, thread_id);
    if files.is_empty() {
        return Err(format!("找不到 {thread_id} 对应的 rollout 文件"));
    }

    // 第一遍：按全局顺序（父→子、行序）收集图片条目，确定保留集。
    // 保留集 = 全局最后 keep_recent 个（file_idx, line_idx）。
    struct ImgEntry {
        file: usize,
        line: usize,
    }
    let mut entries: Vec<ImgEntry> = Vec::new();
    let mut file_lines: Vec<Vec<String>> = Vec::with_capacity(files.len());
    for path in &files {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取失败 {}: {e}", path.display()))?;
        let lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(payload) = payload_of(line) else {
                continue;
            };
            if crate::rewrite::is_image_output(&payload) {
                entries.push(ImgEntry {
                    file: file_lines.len(),
                    line: i,
                });
            }
        }
        file_lines.push(lines);
    }
    let slimmable = entries.len().saturating_sub(keep_recent);
    if slimmable == 0 {
        return Ok(report);
    }
    let keep: std::collections::HashSet<(usize, usize)> = entries[slimmable..]
        .iter()
        .map(|e| (e.file, e.line))
        .collect();

    // 第二遍：逐分片改写（只有命中的分片才动盘），随后做血缘回填。
    for (file_idx, path) in files.iter().enumerate() {
        let lines = &file_lines[file_idx];
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        let mut changed = 0usize;
        let before: u64 = lines.iter().map(|l| l.len() as u64).sum();
        for (i, line) in lines.iter().enumerate() {
            let Some(payload) = payload_of(line) else {
                out.push(line.clone());
                continue;
            };
            let is_img =
                crate::rewrite::is_image_output(&payload) && !keep.contains(&(file_idx, i));
            if !is_img {
                out.push(line.clone());
                continue;
            }
            let mut record: Value =
                serde_json::from_str(line.trim_end()).map_err(|e| e.to_string())?;
            record["payload"] = crate::rewrite::slim_image_output(&payload);
            out.push(serde_json::to_string(&record).map_err(|e| e.to_string())? + "\n");
            changed += 1;
        }
        if changed == 0 {
            continue;
        }
        let after: u64 = out.iter().map(|l| l.len() as u64).sum();
        report.bytes_before += before;
        report.bytes_after += after;
        report.slimmed += changed;
        report.files.push(path.clone());
        let stamp = chrono_stamp();
        let bak = path.with_file_name(format!(
            "{}.bak-slim-{stamp}",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::copy(path, &bak).map_err(|e| format!("备份失败 {}: {e}", bak.display()))?;
        std::fs::write(path, out.concat())
            .map_err(|e| format!("写入失败 {}: {e}", path.display()))?;
        report.backups.push(bak);

        // 血缘回填：本分片变短后，直接子分片的 end_byte_offset 必须重算。
        if let Some(ref_id) = shard_ref_id(path) {
            for child in files.iter().skip(file_idx + 1) {
                let Some((base_ref, ord_excl, old_off)) = read_history_base(child) else {
                    continue;
                };
                if base_ref != ref_id {
                    continue;
                }
                match compute_base_offset(path, ord_excl) {
                    Ok(new_off) => match backfill_history_base(child, new_off) {
                        Ok((bak, _, _)) => report.lineage_backfills.push(format!(
                            "血缘回填 {}: end_byte_offset {old_off} -> {new_off}（备份 {}）",
                            child.display(),
                            bak.display()
                        )),
                        Err(e) if e == "offsets-equal" => {}
                        Err(e) => report
                            .lineage_errors
                            .push(format!("血缘回填失败 {}: {e}", child.display())),
                    },
                    Err(e) => report.lineage_errors.push(format!(
                        "血缘回填失败 {}: 父分片 {} 中{e}",
                        child.display(),
                        path.display()
                    )),
                }
            }
        }
    }
    Ok(report)
}
/// 本地时间戳（yyyyMMdd-HHmmss），用于备份文件名。
fn chrono_stamp() -> String {
    jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string()
}

/// 分片文件名中被 history_base 引用的 id：根分片（`rollout-<ts>-<tid>.jsonl`）
/// 用主 tid；压缩分片（`..._<suffix>.jsonl`）用后缀 id。
pub fn shard_ref_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("rollout-")?;
    // 结构：rollout-<yyyy-MM-dd>T<HH-MM-SS>-<tid>[_<suffix>]
    // 注意时间戳自身含 '-'，要跳过 HH-MM-SS 三段再取 id。
    let after_date = rest.split_once('T')?.1;
    let id_part = after_date.splitn(4, '-').nth(3)?;
    Some(match id_part.split_once('_') {
        Some((_, suffix)) => suffix.to_string(),
        None => id_part.to_string(),
    })
}

/// 读取分片首行 session_meta 的 history_base。
/// 返回 (引用的分片 id, end_ordinal_exclusive, end_byte_offset)。
pub fn read_history_base(path: &Path) -> Option<(String, u64, u64)> {
    let file = std::fs::File::open(path).ok()?;
    use std::io::BufRead;
    let first = std::io::BufReader::new(file).lines().next()?.ok()?;
    let record = serde_json::from_str::<Value>(&first).ok()?;
    let base = record.get("payload")?.get("history_base")?;
    Some((
        base.get("thread_id")?.as_str()?.to_string(),
        base.get("end_ordinal_exclusive")?.as_u64()?,
        base.get("end_byte_offset")?.as_u64()?,
    ))
}

/// 计算 history_base 在修复后父分片中的新字节偏移：
/// 首条 ordinal >= end_ordinal_exclusive 记录的起始字节；找不到则退化为
/// 最后一条 ordinal < end_ordinal_exclusive 记录的结束偏移；都没有则报错
/// （绝不写猜测值——找不到边界就报错跳过）。
pub fn compute_base_offset(parent: &Path, end_ordinal_exclusive: u64) -> Result<u64, String> {
    let data = std::fs::read(parent).map_err(|e| format!("读取失败 {}: {e}", parent.display()))?;
    let mut pos = 0usize;
    let mut last_before_end: Option<usize> = None;
    for chunk in data.split(|b| *b == b'\n') {
        let start = pos;
        pos += chunk.len() + 1; // 含换行
        if chunk.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(chunk) else {
            continue;
        };
        let Some(ordinal) = record.get("ordinal").and_then(Value::as_u64) else {
            continue;
        };
        if ordinal >= end_ordinal_exclusive {
            return Ok(start as u64);
        }
        last_before_end = Some(pos.min(data.len()));
    }
    last_before_end
        .map(|end| end as u64)
        .ok_or_else(|| format!("找不到 ordinal 边界 {end_ordinal_exclusive}"))
}

/// 回填子分片首行 history_base.end_byte_offset（只改首行，先备份）。
/// 返回 (备份路径, 旧值, 新值)。
pub fn backfill_history_base(child: &Path, new_offset: u64) -> Result<(PathBuf, u64, u64), String> {
    let data = std::fs::read(child).map_err(|e| format!("读取失败 {}: {e}", child.display()))?;
    let newline = data
        .iter()
        .position(|b| *b == b'\n')
        .ok_or_else(|| format!("{} 不是多行 JSONL", child.display()))?;
    let mut meta: Value = serde_json::from_slice(&data[..newline])
        .map_err(|e| format!("{} 首行解析失败: {e}", child.display()))?;
    let base = meta
        .get_mut("payload")
        .and_then(|p| p.get_mut("history_base"))
        .ok_or_else(|| format!("{} 没有 history_base", child.display()))?;
    let old_offset = base
        .get("end_byte_offset")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{} 的 history_base 缺 end_byte_offset", child.display()))?;
    if old_offset == new_offset {
        return Err("offsets-equal".to_string()); // 调用方视为 no-op
    }
    base["end_byte_offset"] = Value::from(new_offset);
    let stamp = chrono_stamp();
    let bak = child.with_file_name(format!(
        "{}.bak-lineage-{stamp}",
        child.file_name().unwrap().to_string_lossy()
    ));
    std::fs::copy(child, &bak).map_err(|e| format!("备份失败 {}: {e}", bak.display()))?;
    let mut out = serde_json::to_string(&meta)
        .map_err(|e| e.to_string())?
        .into_bytes();
    out.push(b'\n');
    out.extend_from_slice(&data[newline + 1..]);
    std::fs::write(child, &out).map_err(|e| format!("写入失败 {}: {e}", child.display()))?;
    Ok((bak, old_offset, new_offset))
}

/// 修复一个线程的血缘断链（③）：对每个分片的 history_base 链接校验
/// 「父分片存在且 end_byte_offset 不越界」，断链处按 ② 的回填逻辑修复。
/// 返回 (回填明细, 错误明细)；错误包括父分片缺失等无法修复的情形
/// （面板可见，绝不写猜测值）。
pub fn repair_lineage_breaks(thread_id: &str, roots: &[PathBuf]) -> (Vec<String>, Vec<String>) {
    let files = collect_thread_files(roots, thread_id);
    let mut fixed = Vec::new();
    let mut errors = Vec::new();
    for child in &files {
        let Some((base_ref, ord_excl, old_off)) = read_history_base(child) else {
            continue;
        };
        let Some(parent) = files
            .iter()
            .find(|p| shard_ref_id(p).as_deref() == Some(base_ref.as_str()))
        else {
            errors.push(format!(
                "血缘断链 {}: 父分片 {base_ref} 缺失，无法回填",
                child.display()
            ));
            continue;
        };
        let parent_size = parent.metadata().map(|m| m.len()).unwrap_or(0);
        if old_off <= parent_size {
            continue; // 链完好
        }
        match compute_base_offset(parent, ord_excl) {
            Ok(new_off) => match backfill_history_base(child, new_off) {
                Ok((bak, _, _)) => fixed.push(format!(
                    "血缘回填 {}: end_byte_offset {old_off} -> {new_off}（备份 {}）",
                    child.display(),
                    bak.display()
                )),
                Err(e) if e == "offsets-equal" => {}
                Err(e) => errors.push(format!("血缘回填失败 {}: {e}", child.display())),
            },
            Err(e) => errors.push(format!(
                "血缘回填失败 {}: 父分片 {} 中{e}",
                child.display(),
                parent.display()
            )),
        }
    }
    (fixed, errors)
}
#[derive(Debug, Clone)]
pub struct FileReport {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub orphan: usize,
    pub web_search: usize,
    pub samples: Vec<String>,
    pub changed: usize,
    pub dropped: usize,
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RepairReport {
    pub files: Vec<FileReport>,
    pub total_orphan: usize,
    pub total_web_search: usize,
    pub total_changed: usize,
    pub total_dropped: usize,
    pub remaining: usize,
    pub applied: bool,
    /// 血缘回填记录（父分片改写后子分片 history_base.end_byte_offset 的新值）。
    pub lineage_backfills: Vec<String>,
    /// 血缘回填失败（找不到边界记录等）：跳过并在面板可见，绝不写猜测值。
    pub lineage_errors: Vec<String>,
}

impl RepairReport {
    pub fn to_json(&self) -> Value {
        json!({
            "applied": self.applied,
            "total_orphan": self.total_orphan,
            "total_web_search": self.total_web_search,
            "total_changed": self.total_changed,
            "total_dropped": self.total_dropped,
            "remaining": self.remaining,
            "lineage_backfills": self.lineage_backfills,
            "lineage_errors": self.lineage_errors,
            "files": self.files.iter().map(|f| json!({
                "path": f.path.to_string_lossy(),
                "size_bytes": f.size_bytes,
                "orphan": f.orphan,
                "web_search": f.web_search,
                "samples": f.samples,
                "changed": f.changed,
                "dropped": f.dropped,
                "backup": f.backup.as_ref().map(|b| b.to_string_lossy()),
            })).collect::<Vec<_>>(),
        })
    }
}

/// 收集 roots 下所有文件名包含 tid 的 rollout 分片，按 mtime 升序
///（父分片先修：子分片的 history_base 偏移回填依赖父分片已修复）。
pub fn collect_thread_files(roots: &[PathBuf], thread_id: &str) -> Vec<PathBuf> {
    let mut matches: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if is_rollout_jsonl(&p)
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().contains(thread_id))
                {
                    matches.push(p);
                }
            }
        }
    }
    matches.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
    matches
}

/// 扫描 / 修复一个线程（深链 / id / 路径）的全部 rollout 分片。
///
/// 修复范围与统一扫描的判定集合保持一致（所有文件名含 tid 的分片），
/// 不再只修血缘链：链外/废弃分片里的坏条目同样会被分页历史读到，
/// 且「修复后扫描应清空」要求两侧同集合。
pub fn repair_thread(input: &str, web_search: &str, apply: bool) -> Result<RepairReport, String> {
    repair_thread_with_roots(input, web_search, apply, &sessions_roots())
}

/// 同 [`repair_thread`]，但会话根目录可注入（测试与内部用）。
pub fn repair_thread_with_roots(
    input: &str,
    web_search: &str,
    apply: bool,
    roots: &[PathBuf],
) -> Result<RepairReport, String> {
    let target = parse_thread_id(input);
    let as_path = Path::new(&target);
    let files = if as_path.is_file() {
        // 显式路径输入：只修这一个文件（常用于测试与定点处理）
        vec![as_path.to_path_buf()]
    } else {
        let files = collect_thread_files(roots, &target);
        if files.is_empty() {
            return Err(format!("找不到 {target} 对应的 rollout 文件"));
        }
        files
    };
    let mut report = RepairReport {
        files: Vec::new(),
        total_orphan: 0,
        total_web_search: 0,
        total_changed: 0,
        total_dropped: 0,
        remaining: 0,
        applied: apply,
        lineage_backfills: Vec::new(),
        lineage_errors: Vec::new(),
    };
    for (index, path) in files.iter().enumerate() {
        let scan = scan_file(path);
        let (changed, dropped, backup) = repair_file(path, web_search, apply)?;
        // ② 血缘回填：本分片被截短/改写后，直接子分片首行的
        // history_base.end_byte_offset 必须指到边界记录的新位置，
        // 否则 Codex 启动校验报「cutoff byte offset is past the source
        // rollout」并拒绝加载线程。只回填直接子分片：
        // 孙分片指向子分片，子分片未被改写则孙不动。
        if apply && (changed > 0 || dropped > 0) {
            if let Some(ref_id) = shard_ref_id(path) {
                for child in files.iter().skip(index + 1) {
                    let Some((base_ref, ord_excl, old_off)) = read_history_base(child) else {
                        continue;
                    };
                    if base_ref != ref_id {
                        continue;
                    }
                    match compute_base_offset(path, ord_excl) {
                        Ok(new_off) => match backfill_history_base(child, new_off) {
                            Ok((bak, _, _)) => report.lineage_backfills.push(format!(
                                "血缘回填 {}: end_byte_offset {old_off} -> {new_off}（备份 {}）",
                                child.display(),
                                bak.display()
                            )),
                            Err(e) if e == "offsets-equal" => {}
                            Err(e) => report
                                .lineage_errors
                                .push(format!("血缘回填失败 {}: {e}", child.display())),
                        },
                        Err(e) => report.lineage_errors.push(format!(
                            "血缘回填失败 {}: 父分片 {} 中{e}",
                            child.display(),
                            path.display()
                        )),
                    }
                }
            }
        }
        report.total_orphan += scan.orphan;
        report.total_web_search += scan.web_search;
        report.total_changed += changed;
        report.total_dropped += dropped;
        report.files.push(FileReport {
            path: path.clone(),
            size_bytes: path.metadata().map(|m| m.len()).unwrap_or(0),
            orphan: scan.orphan,
            web_search: scan.web_search,
            samples: scan.samples,
            changed,
            dropped,
            backup,
        });
    }
    if apply {
        report.remaining = files
            .iter()
            .map(|p| {
                let s = scan_file(p);
                s.orphan + s.web_search
            })
            .sum();
    } else {
        report.remaining = report.total_orphan + report.total_web_search;
    }
    Ok(report)
}
