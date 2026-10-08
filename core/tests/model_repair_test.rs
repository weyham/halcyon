//! 模型不匹配扫描测试。fixture 使用真实 rollout 的 JSONL 形状，
//! 不触碰真实 `~/.codex`。修复动作已收敛到 app-server 官方路径（auto_repair），
//! 手工追加权威记录的实现已移除：模型修复只走官方 resume/turn 路径。

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use halcyon_core::model_repair::scan_model_mismatches;
use halcyon_core::rewrite::ModelInfo;
use serde_json::{json, Value};
use tempfile::TempDir;

fn model_info() -> ModelInfo {
    ModelInfo {
        default_model: "glm-5.3".to_string(),
        available_models: HashSet::from(["glm-5.3".to_string(), "glm-5.3-flash".to_string()]),
    }
}

fn write_real_shape_rollout(
    dir: &TempDir,
    thread_id: &str,
    model: &str,
    include_id: bool,
) -> PathBuf {
    let path = dir.path().join(format!("rollout-real-{thread_id}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();

    let mut meta_payload = json!({
        "session_id": thread_id,
        "model_provider": "custom",
        "cwd": "F:\\Projects\\Weyham\\codex-responses-shim",
        "base_instructions": {
            "provenance": { "model": model }
        }
    });
    if include_id {
        meta_payload["id"] = Value::String(thread_id.to_string());
    }
    let meta = json!({
        "timestamp": "2026-10-04T00:00:00Z",
        "type": "session_meta",
        "payload": meta_payload,
    });
    let event = json!({
        "timestamp": "2026-10-04T00:00:01Z",
        "ordinal": 5,
        "type": "event_msg",
        "payload": {
            "type": "thread_settings_applied",
            "thread_id": thread_id,
            "thread_settings": {
                "model": model,
                "model_provider_id": "custom",
                "approval_policy": "untrusted",
                "collaboration_mode": {
                    "settings": {
                        "developer_instructions": "真实标题\n后续说明"
                    }
                }
            }
        }
    });
    let world_state_direct = json!({
        "timestamp": "2026-10-04T00:00:03Z",
        "type": "world_state",
        "payload": {
            "state": {
                "collaboration_mode": {
                    "mode": "default",
                    "model": model,
                    "instructions": "保持语义"
                }
            }
        }
    });
    let world_state_settings = json!({
        "timestamp": "2026-10-04T00:00:04Z",
        "type": "world_state",
        "payload": {
            "state": {
                "collaboration_mode": {
                    "mode": "default",
                    "settings": {
                        "model": model,
                        "developer_instructions": "保持语义"
                    }
                }
            }
        }
    });
    let turn_context = json!({
        "timestamp": "2026-10-04T00:00:05Z",
        "type": "turn_context",
        "payload": {
            "model": model,
            "cwd": "F:\\Projects\\Weyham\\codex-responses-shim"
        }
    });
    let message = json!({
        "timestamp": "2026-10-04T00:00:06Z",
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "正文必须保持不动" }]
        }
    });

    writeln!(file, "{}", serde_json::to_string(&meta).unwrap()).unwrap();
    writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::to_string(&world_state_direct).unwrap()
    )
    .unwrap();
    writeln!(
        file,
        "{}",
        serde_json::to_string(&world_state_settings).unwrap()
    )
    .unwrap();
    writeln!(file, "{}", serde_json::to_string(&turn_context).unwrap()).unwrap();
    writeln!(file, "{}", serde_json::to_string(&message).unwrap()).unwrap();
    path
}

#[test]
fn scan_reports_real_rollout_model_mismatch() {
    let dir = TempDir::new().unwrap();
    let path = write_real_shape_rollout(&dir, "01real-thread-id", "deepseek-flash", true);
    let before = std::fs::read(&path).unwrap();
    let tasks = scan_model_mismatches(dir.path(), &model_info());

    assert_eq!(tasks.len(), 1, "真实形状必须能进入结果集");
    assert_eq!(tasks[0].thread_id, "01real-thread-id");
    assert_eq!(tasks[0].current_model, "deepseek-flash");
    assert_eq!(tasks[0].target_model, "glm-5.3");
    assert_eq!(
        tasks[0].title.as_deref(),
        Some("正文必须保持不动"),
        "无侧边栏标题时回退到首条用户消息"
    );
    assert_eq!(
        tasks[0].project.as_deref(),
        Some("codex-responses-shim"),
        "项目名取 session_meta.cwd 的目录名"
    );
    assert_eq!(tasks[0].files.len(), 1);
    assert_eq!(tasks[0].files[0].path, path);
    let report = tasks[0].to_json();
    assert!(
        report.get("error").is_some_and(Value::is_null),
        "任务级 JSON 必须显式携带 error=null 契约"
    );
    assert!(tasks[0].fields_contain("thread_settings.model"));
    assert!(tasks[0].fields_contain("session_meta.base_instructions.provenance.model"));
    assert!(tasks[0].fields_contain("world_state.state.collaboration_mode.model"));
    assert!(tasks[0].fields_contain("world_state.state.collaboration_mode.settings.model"));
    assert!(tasks[0].fields_contain("turn_context.payload.model"));
    assert_eq!(std::fs::read(&path).unwrap(), before, "扫描必须保持只读");
}
trait FieldsContain {
    fn fields_contain(&self, field: &str) -> bool;
}

impl FieldsContain for halcyon_core::model_repair::TaskModelReport {
    fn fields_contain(&self, field: &str) -> bool {
        self.files
            .iter()
            .flat_map(|file| file.fields.iter())
            .any(|actual| actual == field)
    }
}

#[test]
fn scan_falls_back_to_session_id_and_event_thread_id() {
    let dir = TempDir::new().unwrap();
    write_real_shape_rollout(&dir, "01fallback-thread-id", "k3", false);
    let tasks = scan_model_mismatches(dir.path(), &model_info());

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].thread_id, "01fallback-thread-id");
    assert_eq!(tasks[0].current_model, "k3");
}

#[test]
fn scan_ignores_provider_and_catalog_models() {
    let dir = TempDir::new().unwrap();
    write_real_shape_rollout(&dir, "01current-thread-id", "glm-5.3-flash", true);
    let tasks = scan_model_mismatches(dir.path(), &model_info());
    assert!(
        tasks.is_empty(),
        "provider=custom 或目录内模型不得判为不匹配"
    );
}

fn write_rollout_with_zoned_authority(
    dir: &TempDir,
    thread_id: &str,
    old_model: &str,
    target_model: &str,
) -> PathBuf {
    let path = write_real_shape_rollout(dir, thread_id, old_model, true);
    let bad = json!({
        "timestamp": "2026-10-04T22:18:40.2818596+08:00[Asia/Shanghai]",
        "ordinal": 100,
        "type": "event_msg",
        "payload": {
            "type": "thread_settings_applied",
            "thread_id": thread_id,
            "thread_settings": {
                "model": target_model,
                "model_provider_id": "custom",
                "approval_policy": "untrusted"
            }
        }
    });
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{}", serde_json::to_string(&bad).unwrap()).unwrap();
    path
}

#[test]
fn scan_ignores_unparseable_zoned_authority_timestamp() {
    let dir = TempDir::new().unwrap();
    write_rollout_with_zoned_authority(&dir, "01zoned-thread-id", "deepseek-flash", "glm-5.3");
    let before = std::fs::read(dir.path().join("rollout-real-01zoned-thread-id.jsonl")).unwrap();

    let tasks = scan_model_mismatches(dir.path(), &model_info());

    assert_eq!(
        tasks.len(),
        1,
        "坏时间戳记录不构成权威，任务必须仍进入待修复列表"
    );
    assert_eq!(
        tasks[0].current_model, "deepseek-flash",
        "权威模型必须取最后一条可解析记录，而不是坏时间戳记录里的目标模型"
    );
    assert_eq!(
        tasks[0].unparseable_authority, 1,
        "任务级必须统计坏时间戳记录"
    );
    assert_eq!(
        tasks[0].files[0].unparseable_authority, 1,
        "文件级必须统计坏时间戳记录"
    );
    let report = tasks[0].to_json();
    assert_eq!(report["unparseable_authority"], 1);
    assert_eq!(report["files"][0]["unparseable_authority"], 1);
    assert_eq!(
        std::fs::read(dir.path().join("rollout-real-01zoned-thread-id.jsonl")).unwrap(),
        before,
        "扫描必须保持只读"
    );
}

fn fake_codex_home(dir: &TempDir) -> (PathBuf, PathBuf) {
    let home = dir.path().join("codex-home");
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    (sessions, home)
}

fn write_global_state(home: &Path, descriptions: Value) {
    let state = json!({
        "electron-persisted-atom-state": {
            "thread-descriptions-v1": descriptions
        }
    });
    std::fs::write(
        home.join(".codex-global-state.json"),
        serde_json::to_string(&state).unwrap(),
    )
    .unwrap();
}

/// 在 codex_home 下写出 `state_<version>.sqlite`，含 threads / projects 表。
fn write_state_db(home: &Path, version: u64) {
    let conn = rusqlite::Connection::open(home.join(format!("state_{version}.sqlite"))).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads (id TEXT PRIMARY KEY, name TEXT, title TEXT, project_id TEXT);
         CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT);",
    )
    .unwrap();
}

#[test]
fn title_and_project_prefer_state_db() {
    let dir = TempDir::new().unwrap();
    let (sessions, home) = fake_codex_home(&dir);
    let sub = sessions.join("2026").join("10");
    std::fs::create_dir_all(&sub).unwrap();
    let shard_dir = TempDir::new_in(&sub).unwrap();
    write_real_shape_rollout(&shard_dir, "01title-statedb-id", "deepseek-flash", true);
    // 状态库给出侧边栏标题与项目名；global-state 的自动摘要在场但应靠后。
    write_state_db(&home, 5);
    let conn = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    conn.execute(
        "INSERT INTO projects (id, name) VALUES (?1, ?2)",
        ("proj-1", "项目分组名"),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads (id, name, title, project_id) VALUES (?1, ?2, ?3, ?4)",
        ("01title-statedb-id", "侧边栏原标题", "自动摘要", "proj-1"),
    )
    .unwrap();
    drop(conn);
    write_global_state(&home, json!({ "01title-statedb-id": "全局状态里的描述" }));

    let tasks = scan_model_mismatches(&sessions, &model_info());

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title.as_deref(), Some("侧边栏原标题"));
    assert_eq!(
        tasks[0].project.as_deref(),
        Some("项目分组名"),
        "项目名应来自 projects 连表，而不是 cwd 目录名"
    );
}

#[test]
fn title_falls_back_to_state_db_title_then_globally() {
    let dir = TempDir::new().unwrap();
    let (sessions, home) = fake_codex_home(&dir);
    let shard_dir = TempDir::new_in(&sessions).unwrap();
    write_real_shape_rollout(&shard_dir, "01title-dbtitle-id", "deepseek-flash", true);
    write_state_db(&home, 5);
    let conn = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    conn.execute(
        "INSERT INTO threads (id, name, title, project_id) VALUES (?1, ?2, ?3, NULL)",
        ("01title-dbtitle-id", "", "状态库自动摘要"),
    )
    .unwrap();
    drop(conn);

    let tasks = scan_model_mismatches(&sessions, &model_info());

    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].title.as_deref(),
        Some("状态库自动摘要"),
        "name 为空时回退 threads.title"
    );
    assert_eq!(
        tasks[0].project.as_deref(),
        Some("codex-responses-shim"),
        "project_id 缺失时回退 cwd 目录名"
    );
}

#[test]
fn state_db_picks_highest_version_and_tolerates_missing_columns() {
    let dir = TempDir::new().unwrap();
    let (sessions, home) = fake_codex_home(&dir);
    let shard_dir = TempDir::new_in(&sessions).unwrap();
    write_real_shape_rollout(&shard_dir, "01title-version-id", "deepseek-flash", true);
    // 旧版本库放干扰标题；新版本库缺 project_id 列，验证列探测与版本选择。
    write_state_db(&home, 3);
    let conn3 = rusqlite::Connection::open(home.join("state_3.sqlite")).unwrap();
    conn3
        .execute(
            "INSERT INTO threads (id, name, title, project_id) VALUES (?1, ?2, NULL, NULL)",
            ("01title-version-id", "旧版本库的标题"),
        )
        .unwrap();
    drop(conn3);
    let conn5 = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    conn5
        .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, name TEXT);")
        .unwrap();
    conn5
        .execute(
            "INSERT INTO threads (id, name) VALUES (?1, ?2)",
            ("01title-version-id", "新版本库的标题"),
        )
        .unwrap();
    drop(conn5);

    let tasks = scan_model_mismatches(&sessions, &model_info());

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title.as_deref(), Some("新版本库的标题"));
    assert_eq!(tasks[0].project.as_deref(), Some("codex-responses-shim"));
}

#[test]
fn title_prefers_sidebar_global_state() {
    let dir = TempDir::new().unwrap();
    let (sessions, home) = fake_codex_home(&dir);
    let sub = sessions.join("2026").join("10");
    std::fs::create_dir_all(&sub).unwrap();
    let shard_dir = TempDir::new_in(&sub).unwrap();
    write_real_shape_rollout(&shard_dir, "01title-sidebar-id", "deepseek-flash", true);
    write_global_state(&home, json!({ "01title-sidebar-id": "侧边栏里的任务名" }));

    let tasks = scan_model_mismatches(&sessions, &model_info());

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title.as_deref(), Some("侧边栏里的任务名"));
    assert_eq!(tasks[0].project.as_deref(), Some("codex-responses-shim"));
    let report = tasks[0].to_json();
    assert_eq!(report["title"], "侧边栏里的任务名");
    assert_eq!(report["project"], "codex-responses-shim");
}

#[test]
fn title_falls_back_to_first_user_message_when_sidebar_missing() {
    let dir = TempDir::new().unwrap();
    let (sessions, home) = fake_codex_home(&dir);
    let shard_dir = TempDir::new_in(&sessions).unwrap();
    write_real_shape_rollout(&shard_dir, "01title-message-id", "deepseek-flash", true);
    // global-state 存在但没有该 thread 的条目 → 回退首条用户消息
    write_global_state(&home, json!({ "01another-thread": "别的任务" }));

    let tasks = scan_model_mismatches(&sessions, &model_info());

    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].title.as_deref(),
        Some("正文必须保持不动"),
        "侧边栏没有该任务时回退到首条用户消息"
    );
}

#[test]
fn title_first_user_message_truncates_to_40_chars() {
    let dir = TempDir::new().unwrap();
    let (sessions, _home) = fake_codex_home(&dir);
    let shard_dir = TempDir::new_in(&sessions).unwrap();
    let thread_id = "01title-long-id";
    let path = shard_dir.path().join(format!("rollout-{thread_id}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();
    let meta = json!({
        "timestamp": "2026-10-04T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": thread_id,
            "model_provider": "custom",
            "base_instructions": { "provenance": { "model": "deepseek-flash" } }
        }
    });
    // 线程级判定：无权威记录时回退最后一条 turn_context.model；
    // 补一条让分片「可判定」（否则按「无法判定则不报」不进结果）。
    let turn_context = json!({
        "timestamp": "2026-10-04T00:00:07Z",
        "type": "turn_context",
        "payload": { "model": "deepseek-flash" }
    });
    let long_text = "一".repeat(80);
    let message = json!({
        "timestamp": "2026-10-04T00:00:06Z",
        "type": "response_item",
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": long_text }]
        }
    });
    writeln!(file, "{}", serde_json::to_string(&meta).unwrap()).unwrap();
    writeln!(file, "{}", serde_json::to_string(&turn_context).unwrap()).unwrap();
    writeln!(file, "{}", serde_json::to_string(&message).unwrap()).unwrap();
    drop(file);

    let tasks = scan_model_mismatches(&sessions, &model_info());

    assert_eq!(tasks.len(), 1);
    let title = tasks[0].title.as_deref().unwrap();
    assert_eq!(title.chars().count(), 40, "首条用户消息必须截断到 40 字");
}

#[test]
fn title_silently_falls_back_when_global_state_missing_or_invalid() {
    // 情况一：global-state 文件不存在，且 rollout 里没有用户消息 → title 为 None，
    // 由前端回退到短 thread id。
    let dir = TempDir::new().unwrap();
    let (sessions, _home) = fake_codex_home(&dir);
    let shard_dir = TempDir::new_in(&sessions).unwrap();
    let thread_id = "01title-none-id";
    let path = shard_dir.path().join(format!("rollout-{thread_id}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();
    let meta = json!({
        "timestamp": "2026-10-04T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": thread_id,
            "model_provider": "custom",
            "base_instructions": { "provenance": { "model": "deepseek-flash" } }
        }
    });
    // 线程级判定：无权威记录时回退最后一条 turn_context.model；
    // 补一条让分片「可判定」（否则按「无法判定则不报」不进结果）。
    let turn_context = json!({
        "timestamp": "2026-10-04T00:00:07Z",
        "type": "turn_context",
        "payload": { "model": "deepseek-flash" }
    });
    writeln!(file, "{}", serde_json::to_string(&meta).unwrap()).unwrap();
    writeln!(file, "{}", serde_json::to_string(&turn_context).unwrap()).unwrap();
    drop(file);

    let tasks = scan_model_mismatches(&sessions, &model_info());
    assert_eq!(tasks.len(), 1, "global-state 缺失不得影响扫描");
    assert_eq!(tasks[0].title, None, "两级标题都缺失时留 None 由前端回退");

    // 情况二：global-state 是非法 JSON → 静默回退，不得报错。
    let dir2 = TempDir::new().unwrap();
    let (sessions2, home2) = fake_codex_home(&dir2);
    let shard_dir2 = TempDir::new_in(&sessions2).unwrap();
    write_real_shape_rollout(&shard_dir2, "01title-badjson-id", "deepseek-flash", true);
    std::fs::write(home2.join(".codex-global-state.json"), "{ not json").unwrap();

    let tasks2 = scan_model_mismatches(&sessions2, &model_info());
    assert_eq!(tasks2.len(), 1, "global-state 非法不得影响扫描");
    assert_eq!(
        tasks2[0].title.as_deref(),
        Some("正文必须保持不动"),
        "global-state 非法时回退到首条用户消息"
    );
}

#[test]
fn project_missing_when_no_cwd() {
    let dir = TempDir::new().unwrap();
    let (sessions, _home) = fake_codex_home(&dir);
    let shard_dir = TempDir::new_in(&sessions).unwrap();
    let thread_id = "01project-none-id";
    let path = shard_dir.path().join(format!("rollout-{thread_id}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();
    let meta = json!({
        "timestamp": "2026-10-04T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": thread_id,
            "model_provider": "custom",
            "base_instructions": { "provenance": { "model": "deepseek-flash" } }
        }
    });
    // 线程级判定：无权威记录时回退最后一条 turn_context.model；
    // 补一条让分片「可判定」（否则按「无法判定则不报」不进结果）。
    let turn_context = json!({
        "timestamp": "2026-10-04T00:00:07Z",
        "type": "turn_context",
        "payload": { "model": "deepseek-flash" }
    });
    writeln!(file, "{}", serde_json::to_string(&meta).unwrap()).unwrap();
    writeln!(file, "{}", serde_json::to_string(&turn_context).unwrap()).unwrap();
    drop(file);

    let tasks = scan_model_mismatches(&sessions, &model_info());
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].project, None, "无 cwd 时项目名留空");
}

/// 线程级判定 fixture：写一个最小分片（session_meta 带旧 provenance 模型 +
/// 可选权威记录 + 可选 turn_context）。
fn write_model_shard(
    dir: &Path,
    file_name: &str,
    thread_id: &str,
    applied: Option<&str>,
    turn_ctx: Option<&str>,
) -> PathBuf {
    let path = dir.join(file_name);
    let mut file = std::fs::File::create(&path).unwrap();
    let meta = json!({
        "timestamp": "2026-10-04T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": thread_id,
            "model_provider": "custom",
            "base_instructions": { "provenance": { "model": "deepseek-flash" } }
        }
    });
    writeln!(file, "{}", serde_json::to_string(&meta).unwrap()).unwrap();
    if let Some(model) = applied {
        let event = json!({
            "timestamp": "2026-10-04T00:00:01Z",
            "ordinal": 2,
            "type": "event_msg",
            "payload": {
                "type": "thread_settings_applied",
                "thread_id": thread_id,
                "thread_settings": { "model": model, "model_provider_id": "custom" }
            }
        });
        writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    }
    if let Some(model) = turn_ctx {
        let tc = json!({
            "timestamp": "2026-10-04T00:00:05Z",
            "type": "turn_context",
            "payload": { "model": model }
        });
        writeln!(file, "{}", serde_json::to_string(&tc).unwrap()).unwrap();
    }
    drop(file);
    path
}

/// 线程级判定（1c）：Codex 只按最新分片恢复模型，所以权威只看最新分片。
/// 实证形态 = 「对系统的操控」：根分片旧权威 glm-5.3 时代的值，
/// 最新分片已被 app-server resume 追加 k3 → 线程不应再报。
#[test]
fn thread_level_verdict_ignores_stale_authority_in_old_shards() {
    let dir = TempDir::new().unwrap();
    let tid = "01thread-level-old";
    write_model_shard(
        dir.path(),
        &format!("rollout-2026-09-01T00-00-00-{tid}.jsonl"),
        tid,
        Some("deepseek-flash"),
        None,
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_model_shard(
        dir.path(),
        &format!("rollout-2026-10-01T00-00-00-{tid}_01child-0000-7000-8000-000000000000.jsonl"),
        tid,
        Some("glm-5.3"),
        None,
    );

    let tasks = scan_model_mismatches(dir.path(), &model_info());
    assert!(
        tasks.is_empty(),
        "最新分片权威合法时，旧分片的过期权威不得再报: {tasks:?}"
    );
}

/// 最新分片权威非法 → 报（最新分片优先，哪怕旧分片权威合法）。
#[test]
fn thread_level_verdict_flags_when_latest_shard_authority_stale() {
    let dir = TempDir::new().unwrap();
    let tid = "01thread-level-stale";
    write_model_shard(
        dir.path(),
        &format!("rollout-2026-09-01T00-00-00-{tid}.jsonl"),
        tid,
        Some("glm-5.3"),
        None,
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_model_shard(
        dir.path(),
        &format!("rollout-2026-10-01T00-00-00-{tid}_01child-0000-7000-8000-000000000000.jsonl"),
        tid,
        Some("deepseek-flash"),
        None,
    );

    let tasks = scan_model_mismatches(dir.path(), &model_info());
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].current_model, "deepseek-flash");
}

/// 最新分片无权威记录 → 回退该分片最后一条 turn_context.model。
#[test]
fn thread_level_verdict_falls_back_to_turn_context() {
    let dir = TempDir::new().unwrap();
    let tid = "01thread-level-tc";
    write_model_shard(
        dir.path(),
        &format!("rollout-2026-10-01T00-00-00-{tid}.jsonl"),
        tid,
        None,
        Some("deepseek-flash"),
    );
    let tasks = scan_model_mismatches(dir.path(), &model_info());
    assert_eq!(tasks.len(), 1, "turn_context 旧模型必须可判定并报出");
    assert_eq!(tasks[0].current_model, "deepseek-flash");

    // turn_context 合法 → 不报
    let dir2 = TempDir::new().unwrap();
    write_model_shard(
        dir2.path(),
        &format!("rollout-2026-10-01T00-00-00-{tid}.jsonl"),
        tid,
        None,
        Some("glm-5.3-flash"),
    );
    assert!(scan_model_mismatches(dir2.path(), &model_info()).is_empty());
}

/// 最新分片既无权威记录也无 turn_context → 无法判定则不报
///（历史分片里的旧字段只展示，不构成判定依据）。
#[test]
fn thread_level_verdict_skips_undecidable_threads() {
    let dir = TempDir::new().unwrap();
    let tid = "01thread-level-none";
    write_model_shard(
        dir.path(),
        &format!("rollout-2026-09-01T00-00-00-{tid}.jsonl"),
        tid,
        Some("deepseek-flash"),
        None,
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    // 最新分片：只有 session_meta（无任何模型证据）
    write_model_shard(
        dir.path(),
        &format!("rollout-2026-10-01T00-00-00-{tid}_01child-0000-7000-8000-000000000000.jsonl"),
        tid,
        None,
        None,
    );
    let tasks = scan_model_mismatches(dir.path(), &model_info());
    assert!(
        tasks.is_empty(),
        "最新分片无权威且无 turn_context 时不得报（无法判定则不报）"
    );
}

/// 只读真实验收探针。设置 CRS_MODEL_REPAIR_PROBE_HOME 后运行：
/// `cargo test -p halcyon-core real_sessions -- --nocapture`。
/// 未设置环境变量时跳过，不读取用户数据。
#[test]
fn optional_real_sessions_read_only_probe() {
    let Ok(codex_home) = std::env::var("CRS_MODEL_REPAIR_PROBE_HOME") else {
        return;
    };
    let codex_home = PathBuf::from(codex_home);
    let info = ModelInfo::load(&codex_home).expect("读取当前模型配置失败");
    let sessions = codex_home.join("sessions");
    assert!(sessions.is_dir(), "sessions 目录不存在：{sessions:?}");
    let tasks = scan_model_mismatches(&sessions, &info);

    let mut distribution = BTreeMap::new();
    let mut files = 0usize;
    for task in &tasks {
        files += task.files.len();
        *distribution
            .entry(task.current_model.clone())
            .or_insert(0usize) += 1;
    }
    println!(
        "model_repair_probe default_model={} tasks={} files={} distribution={distribution:?}",
        info.default_model,
        tasks.len(),
        files
    );
}

/// global-state 项目归属优先于 cwd 目录名：thread-project-assignments
/// 里的 projectId 映射到 local-projects 的 name（数组与对象两种形态都认）。
#[test]
fn global_project_names_maps_assignment_to_name() {
    let dir = TempDir::new().unwrap();
    let state = json!({
        "local-projects": [
            {"id": "proj-1", "name": "codex-responses-shim"},
        ],
        "thread-project-assignments": {
            "thread-a": {"projectId": "proj-1", "projectKind": "local"},
            "thread-b": {"projectId": "proj-missing", "projectKind": "local"}
        }
    });
    std::fs::write(
        dir.path().join(".codex-global-state.json"),
        serde_json::to_string(&state).unwrap(),
    )
    .unwrap();

    let names = halcyon_core::model_repair::global_project_names(dir.path());
    assert_eq!(
        names.get("thread-a").map(String::as_str),
        Some("codex-responses-shim")
    );
    // projectId 指向不存在的项目 → 不出名字（调用方会回落到 cwd 兜底）
    assert!(!names.contains_key("thread-b"));
}
