//! 会话工作目录（writable roots）失效扫描与修复。
//!
//! 数据源：`.codex-global-state.json` 的 `thread-writable-roots`（桌面端
//! 自己维护，app-server 协议无接口）。Codex 运行期间会用内存态覆写该
//! 文件，因此**修复只在 Codex 未运行时执行**（备份 + 原子写）。
//!
//! 分类规则（2026-10-05 裁定）：
//! - 线程有项目归属（thread-project-assignments → local-projects）且项目
//!   当前 rootPaths 存在 → 失效根**重定向**到项目 rootPaths；
//! - 无归属的 scratch 根（visualizations 等）→ 默认**移除**。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RootAction {
    /// 重定向到项目当前 rootPaths。
    Remap(Vec<String>),
    /// 移除该失效根（scratch 默认动作）。
    Remove,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleRoot {
    pub path: String,
    pub action: RootAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleRootsReport {
    pub thread_id: String,
    pub stale: Vec<StaleRoot>,
}

fn state_path(codex_home: &Path) -> PathBuf {
    codex_home.join(".codex-global-state.json")
}

fn read_state(codex_home: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(state_path(codex_home)).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

/// thread id → 项目当前可用的 rootPaths（仅当项目归属存在且全部路径存在）。
fn project_roots_map(state: &Value) -> HashMap<String, Vec<String>> {
    let mut projects: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(local) = state.get("local-projects") {
        let iter: Vec<&Value> = match local {
            Value::Array(items) => items.iter().collect(),
            Value::Object(map) => map.values().collect(),
            _ => Vec::new(),
        };
        for project in iter {
            let Some(id) = project.get("id").and_then(Value::as_str) else {
                continue;
            };
            let roots: Vec<String> = project
                .get("rootPaths")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if !roots.is_empty() && roots.iter().all(|r| Path::new(r).is_dir()) {
                projects.insert(id.to_string(), roots);
            }
        }
    }
    let mut map = HashMap::new();
    if let Some(assignments) = state
        .get("thread-project-assignments")
        .and_then(Value::as_object)
    {
        for (thread_id, assignment) in assignments {
            if let Some(pid) = assignment.get("projectId").and_then(Value::as_str) {
                if let Some(roots) = projects.get(pid) {
                    map.insert(thread_id.clone(), roots.clone());
                }
            }
        }
    }
    map
}

/// 扫描全部线程的失效 writable roots。
pub fn scan_stale_roots(codex_home: &Path) -> Vec<StaleRootsReport> {
    let Some(state) = read_state(codex_home) else {
        return Vec::new();
    };
    let project_roots = project_roots_map(&state);
    let mut reports = Vec::new();
    let Some(wr) = state
        .get("thread-writable-roots")
        .and_then(Value::as_object)
    else {
        return reports;
    };
    for (thread_id, roots) in wr {
        let mut stale = Vec::new();
        for root in roots
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if Path::new(root).is_dir() {
                continue;
            }
            let action = match project_roots.get(thread_id) {
                Some(targets) => RootAction::Remap(targets.clone()),
                None => RootAction::Remove,
            };
            stale.push(StaleRoot {
                path: root.to_string(),
                action,
            });
        }
        if !stale.is_empty() {
            reports.push(StaleRootsReport {
                thread_id: thread_id.clone(),
                stale,
            });
        }
    }
    reports
}

/// 对指定线程应用修复。返回 (改动线程数, 备份路径)。
///
/// 调用方必须保证 Codex 未运行（其内存态会覆写该文件）。
pub fn repair_stale_roots(
    codex_home: &Path,
    thread_ids: &[String],
) -> Result<(usize, PathBuf), String> {
    let path = state_path(codex_home);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("读取全局状态失败: {e}"))?;
    let mut state: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("全局状态 JSON 非法: {e}"))?;
    let project_roots = project_roots_map(&state);
    let Some(wr) = state
        .get_mut("thread-writable-roots")
        .and_then(Value::as_object_mut)
    else {
        return Ok((0, PathBuf::new()));
    };

    let stamp = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y%m%d-%H%M%S")
        .to_string();
    let backup = path.with_extension(format!("bak-halcyon-{stamp}"));

    let mut changed = 0;
    for thread_id in thread_ids {
        let Some(roots) = wr.get_mut(thread_id).and_then(Value::as_array_mut) else {
            continue;
        };
        let mut next: Vec<Value> = Vec::new();
        let mut remapped = false;
        let mut touched = false;
        for root in roots.iter().filter_map(Value::as_str) {
            if Path::new(root).is_dir() {
                push_unique(&mut next, root);
                continue;
            }
            touched = true;
            if !remapped {
                if let Some(targets) = project_roots.get(thread_id) {
                    for target in targets {
                        push_unique(&mut next, target);
                    }
                    remapped = true;
                }
            }
            // 无项目归属的失效根：移除（默认动作）
        }
        if touched {
            *roots = next;
            changed += 1;
        }
    }
    if changed == 0 {
        return Ok((0, PathBuf::new()));
    }

    std::fs::copy(&path, &backup).map_err(|e| format!("备份失败: {e}"))?;
    let tmp = path.with_extension("tmp-halcyon");
    let text = serde_json::to_string(&state).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("写入临时文件失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("原子替换失败: {e}"))?;
    Ok((changed, backup))
}

fn push_unique(list: &mut Vec<Value>, value: &str) {
    if !list.iter().any(|v| v.as_str() == Some(value)) {
        list.push(Value::String(value.to_string()));
    }
}
