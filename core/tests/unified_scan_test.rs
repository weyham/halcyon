//! 统一扫描与 writable-roots 修复测试。fixture 均为临时目录，不碰真实 `~/.codex`。

use std::collections::HashSet;
use std::io::Write;

use halcyon_core::rewrite::ModelInfo;
use halcyon_core::roots_repair::{repair_stale_roots, scan_stale_roots, RootAction};
use halcyon_core::unified_scan::scan_all;
use serde_json::json;
use tempfile::TempDir;

fn info(model: &str) -> ModelInfo {
    ModelInfo {
        default_model: model.to_string(),
        available_models: HashSet::from([model.to_string()]),
    }
}

/// 搭一个 codex_home：sessions/ + 全局状态 JSON + 项目目录。
fn fake_home(dir: &TempDir) -> std::path::PathBuf {
    let home = dir.path().join("codex-home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    home
}

fn write_global_state(home: &std::path::Path, state: serde_json::Value) {
    std::fs::write(
        home.join(".codex-global-state.json"),
        serde_json::to_string(&state).unwrap(),
    )
    .unwrap();
}

fn write_rollout(home: &std::path::Path, name: &str, lines: &[serde_json::Value]) {
    let path = home.join("sessions").join(format!("rollout-{name}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();
    for line in lines {
        writeln!(file, "{}", serde_json::to_string(line).unwrap()).unwrap();
    }
}

fn session_meta(thread_id: &str, model: &str) -> serde_json::Value {
    json!({
        "timestamp": "2026-10-04T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": thread_id,
            "model_provider": "custom",
            "cwd": "F:\\Projects\\Weyham\\codex-responses-shim",
            "base_instructions": { "provenance": { "model": model } }
        }
    })
}

fn orphan_item() -> serde_json::Value {
    json!({
        "timestamp": "2026-10-04T00:00:05Z",
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "name": "send_message_to_thread",
            "namespace": "codex_app",
            "output": "跨任务消息正文"
        }
    })
}

fn image_item(index: usize) -> serde_json::Value {
    json!({
        "timestamp": "2026-10-04T00:00:05Z",
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": format!("view_image_{index}"),
            "output": [{
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{}", "x".repeat(70 * 1024))
            }]
        }
    })
}

#[test]
fn stale_roots_scan_classifies_remap_and_remove() {
    let dir = TempDir::new().unwrap();
    let home = fake_home(&dir);
    let project_dir = dir.path().join("real-project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let existing = dir.path().join("still-there");
    std::fs::create_dir_all(&existing).unwrap();

    write_global_state(
        &home,
        json!({
            "local-projects": [
                { "id": "p1", "name": "real-project", "rootPaths": [project_dir.to_string_lossy()] }
            ],
            "thread-project-assignments": {
                "t-moved": { "projectKind": "local", "projectId": "p1" }
            },
            "thread-writable-roots": {
                "t-moved": ["F:\\gone\\old-project", existing.to_string_lossy()],
                "t-scratch": ["C:\\Users\\u\\.codex\\visualizations\\gone"]
            }
        }),
    );

    let reports = scan_stale_roots(&home);
    assert_eq!(reports.len(), 2);
    let moved = reports.iter().find(|r| r.thread_id == "t-moved").unwrap();
    assert_eq!(moved.stale.len(), 1, "存在的根不应报失效");
    assert_eq!(
        moved.stale[0].action,
        RootAction::Remap(vec![project_dir.to_string_lossy().to_string()])
    );
    let scratch = reports.iter().find(|r| r.thread_id == "t-scratch").unwrap();
    assert_eq!(scratch.stale[0].action, RootAction::Remove);
}

#[test]
fn stale_roots_repair_remaps_removes_and_preserves_valid() {
    let dir = TempDir::new().unwrap();
    let home = fake_home(&dir);
    let project_dir = dir.path().join("real-project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let existing = dir.path().join("still-there");
    std::fs::create_dir_all(&existing).unwrap();

    write_global_state(
        &home,
        json!({
            "local-projects": [
                { "id": "p1", "name": "real-project", "rootPaths": [project_dir.to_string_lossy()] }
            ],
            "thread-project-assignments": {
                "t-moved": { "projectKind": "local", "projectId": "p1" }
            },
            "thread-writable-roots": {
                "t-moved": ["F:\\gone\\old-project", existing.to_string_lossy()],
                "t-scratch": ["C:\\Users\\u\\.codex\\visualizations\\gone"]
            }
        }),
    );

    let (changed, backup) =
        repair_stale_roots(&home, &["t-moved".into(), "t-scratch".into()]).unwrap();
    assert_eq!(changed, 2);
    assert!(backup.is_file(), "应生成备份");

    let reports = scan_stale_roots(&home);
    assert!(reports.is_empty(), "修复后不应再有失效根: {reports:?}");

    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".codex-global-state.json")).unwrap(),
    )
    .unwrap();
    let moved_roots = state["thread-writable-roots"]["t-moved"]
        .as_array()
        .unwrap();
    assert!(moved_roots
        .iter()
        .any(|r| r.as_str() == Some(project_dir.to_string_lossy().as_ref())));
    assert!(moved_roots
        .iter()
        .any(|r| r.as_str() == Some(existing.to_string_lossy().as_ref())));
    let scratch_roots = state["thread-writable-roots"]["t-scratch"]
        .as_array()
        .unwrap();
    assert!(scratch_roots.is_empty(), "scratch 失效根应被移除");
}

#[test]
fn unified_scan_includes_only_offline_issue_kinds() {
    let dir = TempDir::new().unwrap();
    let home = fake_home(&dir);
    // 线程 A：模型不匹配 + 坏条目
    write_rollout(
        &home,
        "a",
        &[
            session_meta("t-a", "deepseek-flash"),
            json!({
                "timestamp": "2026-10-04T00:00:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "thread_settings_applied",
                    "thread_id": "t-a",
                    "thread_settings": { "model": "deepseek-flash" }
                }
            }),
            orphan_item(),
            orphan_item(),
        ],
    );
    // 线程 B：只有目录失效
    write_rollout(&home, "b", &[session_meta("t-b", "glm-5.3")]);
    // 线程 C：只有实时出口保护覆盖的内容形态问题，不应进入离线扫描。
    write_rollout(&home, "c", &[session_meta("t-c", "glm-5.3"), orphan_item()]);
    // 线程 D：只有图片瘦身问题，仍应进入离线扫描（实时与离线是两条独立路径）。
    let mut image_lines = vec![session_meta("t-images", "glm-5.3")];
    image_lines.extend((0..6).map(image_item));
    write_rollout(&home, "images", &image_lines);
    // Guardian review 是 Codex 自动审查线程，即使模型字段过期也不应列入普通修复。
    let mut guardian = session_meta("t-guardian", "deepseek-flash");
    guardian["payload"]["thread_source"] = json!("guardian_review");
    write_rollout(
        &home,
        "guardian",
        &[
            guardian,
            json!({
                "timestamp": "2026-10-04T00:00:01Z",
                "type": "event_msg",
                "payload": {
                    "type": "thread_settings_applied",
                    "thread_id": "t-guardian",
                    "thread_settings": { "model": "deepseek-flash" }
                }
            }),
        ],
    );
    write_global_state(
        &home,
        json!({
            "thread-writable-roots": { "t-b": ["F:\\gone\\somewhere"] }
        }),
    );

    let tasks = scan_all(&home, &home.join("sessions"), &info("glm-5.3"));
    let a = tasks
        .iter()
        .find(|t| t.thread_id == "t-a")
        .expect("t-a 应在结果中");
    assert_eq!(a.model_mismatch.as_deref(), Some("deepseek-flash"));
    assert!(a.stale_roots.is_empty());
    let b = tasks
        .iter()
        .find(|t| t.thread_id == "t-b")
        .expect("t-b 应在结果中");
    assert_eq!(b.stale_roots.len(), 1);
    assert!(b.model_mismatch.is_none());
    let images = tasks
        .iter()
        .find(|t| t.thread_id == "t-images")
        .expect("纯图片瘦身任务也应在结果中");
    assert_eq!(images.oversized_images, 1);
    assert_eq!(tasks.len(), 3, "无问题线程不应出现");
    assert!(!tasks.iter().any(|t| t.thread_id == "t-c"));
    assert!(!tasks.iter().any(|t| t.thread_id == "t-guardian"));
}

/// ③ 血缘断链：子分片 history_base.end_byte_offset 越过父分片 EOF 或父分片缺失。
#[test]
fn lineage_break_detected_and_repair_clears_it() {
    let dir = TempDir::new().unwrap();
    let home = fake_home(&dir);
    let tid = "aaaa1111-0000-7000-8000-000000000000";
    let child_suffix = "bbbb2222-0000-7000-8000-000000000000";
    // 父分片（根）：两条小记录
    let parent_meta = json!({
        "timestamp": "2026-10-04T00:00:00Z",
        "ordinal": 1,
        "type": "session_meta",
        "payload": { "id": tid, "model_provider": "custom",
            "base_instructions": { "provenance": { "model": "m1" } } }
    });
    let r2 = json!({
        "timestamp": "2026-10-04T00:00:01Z",
        "ordinal": 2,
        "type": "response_item",
        "payload": { "type": "function_call_output", "call_id": "c1", "output": "ok" }
    });
    write_rollout(
        &home,
        &format!("2026-10-04T00-00-00-{tid}"),
        &[parent_meta, r2],
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    // 子分片：偏移远超父分片大小（断链）
    let child_meta = json!({
        "timestamp": "2026-10-04T01:00:00Z",
        "ordinal": 3,
        "type": "session_meta",
        "payload": { "id": tid, "model_provider": "custom",
            "history_base": { "thread_id": tid, "end_ordinal_exclusive": 2, "end_byte_offset": 999999 },
            "base_instructions": { "provenance": { "model": "m1" } } }
    });
    write_rollout(
        &home,
        &format!("2026-10-04T01-00-00-{tid}_{child_suffix}"),
        &[child_meta],
    );

    let sessions = home.join("sessions");
    let tasks = scan_all(&home, &sessions, &info("m1"));
    assert_eq!(tasks.len(), 1, "断链必须作为一个问题上报");
    assert_eq!(tasks[0].lineage_breaks.len(), 1);
    assert_eq!(tasks[0].lineage_breaks[0].reason, "offset-past-parent");

    // 修复后复扫清空
    let (fixed, errors) = halcyon_core::repair::repair_lineage_breaks(
        tid,
        &[sessions.clone(), home.join("archived_sessions")],
    );
    assert_eq!(errors.len(), 0, "{errors:?}");
    assert_eq!(fixed.len(), 1);
    let tasks = scan_all(&home, &sessions, &info("m1"));
    assert!(tasks.is_empty(), "回填后断链必须消失: {tasks:?}");
}

#[test]
fn lineage_parent_missing_is_not_actionable() {
    let dir = TempDir::new().unwrap();
    let home = fake_home(&dir);
    let tid = "aaaa3333-0000-7000-8000-000000000000";
    // 子分片指向不存在的父分片 id
    let child_meta = json!({
        "timestamp": "2026-10-04T01:00:00Z",
        "ordinal": 3,
        "type": "session_meta",
        "payload": { "id": tid, "model_provider": "custom",
            "history_base": { "thread_id": "00000000-0000-7000-8000-000000000000", "end_ordinal_exclusive": 2, "end_byte_offset": 10 },
            "base_instructions": { "provenance": { "model": "m1" } } }
    });
    write_rollout(
        &home,
        &format!("2026-10-04T01-00-00-{tid}_bbbb4444-0000-7000-8000-000000000000"),
        &[child_meta],
    );

    let sessions = home.join("sessions");
    let tasks = scan_all(&home, &sessions, &info("m1"));
    assert!(
        tasks.is_empty(),
        "父分片缺失没有可靠修复依据，不应挂为待修复"
    );
    // 直接调用底层修复仍然拒绝猜测偏移。
    let (fixed, errors) =
        halcyon_core::repair::repair_lineage_breaks(tid, std::slice::from_ref(&sessions));
    assert!(fixed.is_empty());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("父分片"), "{errors:?}");
}

#[test]
fn intact_lineage_not_flagged() {
    let dir = TempDir::new().unwrap();
    let home = fake_home(&dir);
    let tid = "aaaa5555-0000-7000-8000-000000000000";
    let meta1 = json!({
        "timestamp": "2026-10-04T00:00:00Z", "ordinal": 1, "type": "session_meta",
        "payload": { "id": tid, "model_provider": "custom",
            "base_instructions": { "provenance": { "model": "m1" } } }
    });
    write_rollout(&home, &format!("2026-10-04T00-00-00-{tid}"), &[meta1]);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let parent_size = std::fs::metadata(
        home.join("sessions")
            .join(format!("rollout-2026-10-04T00-00-00-{tid}.jsonl")),
    )
    .unwrap()
    .len();
    let child_meta = json!({
        "timestamp": "2026-10-04T01:00:00Z", "ordinal": 2, "type": "session_meta",
        "payload": { "id": tid, "model_provider": "custom",
            "history_base": { "thread_id": tid, "end_ordinal_exclusive": 1, "end_byte_offset": parent_size },
            "base_instructions": { "provenance": { "model": "m1" } } }
    });
    write_rollout(
        &home,
        &format!("2026-10-04T01-00-00-{tid}_bbbb6666-0000-7000-8000-000000000000"),
        &[child_meta],
    );

    let sessions = home.join("sessions");
    let tasks = scan_all(&home, &sessions, &info("m1"));
    assert!(tasks.is_empty(), "完好链路不得上报: {tasks:?}");
}

/// cwd → 项目名必须同时认 Windows 与 POSIX 分隔符：
/// rollout 里的 cwd 是写入端平台的写法，跨平台读取时不能只按宿主语义切分
/// （2026-10-07 macOS CI 实测：把整条 `F:\\Projects\\X` 当成了项目名）。
#[test]
fn project_name_from_cwd_handles_both_separators() {
    use halcyon_core::unified_scan::project_name_from_cwd;
    assert_eq!(
        project_name_from_cwd("F:\\Projects\\Weyham\\codex-responses-shim").as_deref(),
        Some("codex-responses-shim")
    );
    assert_eq!(
        project_name_from_cwd("/Users/me/projects/halcyon").as_deref(),
        Some("halcyon")
    );
    // 尾部分隔符不应产生空名
    assert_eq!(
        project_name_from_cwd("F:\\Projects\\X\\").as_deref(),
        Some("X")
    );
    assert_eq!(project_name_from_cwd(""), None);
}
