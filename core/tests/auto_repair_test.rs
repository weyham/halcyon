//! 模型切换自动修复：队列持久化、触发与计划、进程分类。不触碰真实 `~/.codex`。

use std::collections::HashSet;
use std::io::Write;

use halcyon_core::auto_repair::{
    classify_codex_process, filter_for_auto_repair, project_thread_ids, AutoRepair,
    CodexProcessKind, QueueStatus, RepairKinds, RepairRecord,
};
use halcyon_core::model_repair::TaskModelReport;
use halcyon_core::rewrite::ModelInfo;
use serde_json::{json, Value};
use tempfile::TempDir;

fn info(model: &str) -> ModelInfo {
    ModelInfo {
        default_model: model.to_string(),
        available_models: HashSet::from([model.to_string()]),
    }
}

/// 写一个权威模型与目录不匹配的 rollout，返回 sessions_root。
fn mismatching_sessions(dir: &TempDir, thread_id: &str) -> std::path::PathBuf {
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join(format!("rollout-{thread_id}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();
    let meta = json!({
        "timestamp": "2026-10-04T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": thread_id,
            "model_provider": "custom",
            "cwd": "F:\\Projects\\Weyham\\codex-responses-shim",
            "base_instructions": { "provenance": { "model": "deepseek-flash" } }
        }
    });
    let event = json!({
        "timestamp": "2026-10-04T00:00:01Z",
        "type": "event_msg",
        "payload": {
            "type": "thread_settings_applied",
            "thread_id": thread_id,
            "thread_settings": { "model": "deepseek-flash" }
        }
    });
    writeln!(file, "{}", serde_json::to_string(&meta).unwrap()).unwrap();
    writeln!(file, "{}", serde_json::to_string(&event).unwrap()).unwrap();
    // 自动修复只处理「有项目归属」的线程：
    // fixture 必须带项目归属，否则 plan() 会按设计把它们过滤掉。
    let mut assignments = serde_json::Map::new();
    assignments.insert(
        thread_id.to_string(),
        json!({ "projectKind": "local", "projectId": "p-test" }),
    );
    std::fs::write(
        dir.path().join(".codex-global-state.json"),
        json!({
            "local-projects": [ { "id": "p-test", "name": "proj-test" } ],
            "thread-project-assignments": Value::Object(assignments)
        })
        .to_string(),
    )
    .unwrap();
    sessions
}

#[test]
fn observe_only_triggers_on_real_change() {
    let dir = TempDir::new().unwrap();
    let mut ar = AutoRepair::load(dir.path());
    assert!(!ar.observe_default_model(&info("glm-5.3")), "首次只建基线");
    assert!(!ar.observe_default_model(&info("glm-5.3")), "同模型不触发");
    assert!(ar.observe_default_model(&info("k3")), "模型变化触发");
}

#[test]
fn plan_enqueues_and_retargets_on_new_default() {
    let dir = TempDir::new().unwrap();
    let sessions = mismatching_sessions(&dir, "01queue-test-id");
    let mut ar = AutoRepair::load(dir.path());

    let added = ar.plan(&sessions, &info("glm-5.3"));
    assert_eq!(added, 1);
    assert_eq!(ar.queue.target_model, "glm-5.3");
    assert_eq!(ar.queue.pending_count(), 1);

    // 目标模型再次变化：旧 Pending 被替换为新目标的扫描结果
    let added = ar.plan(&sessions, &info("k3"));
    assert_eq!(added, 1);
    assert_eq!(ar.queue.target_model, "k3");
    assert_eq!(ar.queue.pending_count(), 1);
    assert_eq!(ar.queue.items.len(), 1, "旧 Pending 不应残留");
}

#[test]
fn cancel_and_persistence_roundtrip() {
    let dir = TempDir::new().unwrap();
    let sessions = mismatching_sessions(&dir, "01queue-persist-id");
    {
        let mut ar = AutoRepair::load(dir.path());
        ar.plan(&sessions, &info("glm-5.3"));
        assert!(ar.cancel("01queue-persist-id"));
        assert!(!ar.cancel("01queue-persist-id"), "已移除的不能重复取消");
        ar.save();
    }
    // 取消 = 从队列移除（跳过本轮）；队列不沉淀任何已完结状态
    let ar = AutoRepair::load(dir.path());
    assert_eq!(ar.queue.items.len(), 0);
    assert_eq!(ar.queue.pending_count(), 0);
}

/// 生产路径用「锁外扫描 + replace_pending」的回归：
/// 该函数必须只替换 Pending、保留已完成历史，且不依赖扫描。
#[test]
fn replace_pending_swaps_only_pending_and_keeps_history() {
    let dir = TempDir::new().unwrap();
    let sessions = mismatching_sessions(&dir, "01replace-id");
    let mut ar = AutoRepair::load(dir.path());

    // 先跑一轮：入队 → 标记一条 Done（模拟已执行）
    ar.plan(&sessions, &info("glm-5.3"));
    assert_eq!(ar.queue.pending_count(), 1);
    ar.queue.items[0].status = QueueStatus::Done;

    // 模型再次变化：替换 Pending（此处没有 Pending，纯保留历史）
    let tasks = halcyon_core::model_repair::scan_model_mismatches(&sessions, &info("k3"));
    let added = ar.replace_pending("k3", tasks);
    assert_eq!(added, 1);
    assert_eq!(ar.queue.target_model, "k3");
    // 同一线程不再重复入队，已完结的 Done 条目原地复活为 Pending
    //（否则队列卡里会同时出现「失败/已完成」残留行和新待执行行）。
    assert_eq!(
        ar.queue.items.len(),
        1,
        "同一线程只保留一行，Done 复活为 Pending"
    );
    let fresh = &ar.queue.items[0];
    assert_eq!(fresh.status, QueueStatus::Pending);
    // 复活后的条目只带模型修复类型
    assert!(fresh.kinds.model && !fresh.kinds.entries && !fresh.kinds.roots);
}

/// 2026-10-07：取消 = 从队列移除（跳过本轮），下一轮模型切换会重新入队；
/// Failed 条目（仅执行期的瞬态）可以复活重试。
#[test]
fn replace_pending_revives_failed_but_not_cancelled() {
    let dir = TempDir::new().unwrap();
    let sessions = mismatching_sessions(&dir, "01revive-id");
    let mut ar = AutoRepair::load(dir.path());

    // 先入队再取消：条目被移除；模型再切换时作为新条目重新入队
    ar.plan(&sessions, &info("glm-5.3"));
    assert!(ar.cancel("01revive-id"));
    assert_eq!(ar.queue.items.len(), 0, "取消即移除，队列不沉淀");
    let tasks = halcyon_core::model_repair::scan_model_mismatches(&sessions, &info("k3"));
    let added = ar.replace_pending("k3", tasks.clone());
    assert_eq!(added, 1, "取消过的线程在新一轮切换中重新入队");
    assert_eq!(ar.queue.items.len(), 1);
    assert_eq!(ar.queue.items[0].status, QueueStatus::Pending);

    // 标为 Failed：下次切换应复活为 Pending 重试
    ar.queue.items[0].status = QueueStatus::Failed("模拟失败".to_string());
    let added = ar.replace_pending("k3", tasks);
    assert_eq!(added, 1, "Failed 复活重试");
    assert_eq!(ar.queue.items.len(), 1, "同一行复活，不新增");
    assert_eq!(ar.queue.items[0].status, QueueStatus::Pending);
}

/// 2026-10-06 用户裁定：模型切换只自动修复「有项目归属」的线程，
/// 散装会话与自动审查线程不进自动修复范围。
#[test]
fn auto_repair_filter_keeps_project_threads_only() {
    let dir = TempDir::new().unwrap();
    let codex_home = dir.path();
    let sessions = codex_home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        codex_home.join(".codex-global-state.json"),
        json!({
            "local-projects": [ {"id": "p1", "name": "proj"} ],
            "thread-project-assignments": {
                "t-in":    { "projectId": "p1" },
                "t-gone":  { "projectId": "p9" }
            }
        })
        .to_string(),
    )
    .unwrap();

    let owned = project_thread_ids(codex_home);
    assert_eq!(owned, HashSet::from(["t-in".to_string()]));

    let mk = |id: &str, src: Option<&str>| TaskModelReport {
        thread_id: id.to_string(),
        title: None,
        project: None,
        thread_source: src.map(str::to_string),
        current_model: "old".into(),
        target_model: "new".into(),
        mismatch_count: 1,
        unparseable_authority: 0,
        files: Vec::new(),
    };
    let kept = filter_for_auto_repair(
        &sessions,
        vec![
            mk("t-in", None),                    // 项目线程 -> 保留
            mk("t-loose", None),                 // 无项目 -> 过滤
            mk("t-in", Some("guardian_review")), // 自动审查 -> 过滤
        ],
    );
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].thread_id, "t-in");

    // 没有 global-state 时（读不到项目）不应误修：全部过滤掉
    std::fs::remove_file(codex_home.join(".codex-global-state.json")).unwrap();
    assert!(filter_for_auto_repair(&sessions, vec![mk("t-in", None)]).is_empty());
}

#[test]
fn classify_process_kinds() {
    let desktop = "C:\\Users\\u\\AppData\\Local\\OpenAI\\Codex\\codex.exe";
    let cli = "C:\\Users\\u\\AppData\\Local\\OpenAI\\Codex\\bin\\abc123\\codex.exe";
    let sandbox = "C:\\Users\\u\\AppData\\Local\\OpenAI\\Codex\\codex-windows-sandbox-service.exe";
    let cua =
        "C:\\Users\\u\\AppData\\Local\\OpenAI\\Codex\\runtimes\\x\\codex-computer-use-swift.exe";
    let halcyon = "F:\\Apps\\Halcyon\\halcyon.exe";
    let other = "C:\\Windows\\System32\\notepad.exe";
    assert_eq!(
        classify_codex_process(desktop),
        CodexProcessKind::Interactive
    );
    assert_eq!(classify_codex_process(cli), CodexProcessKind::Interactive);
    assert_eq!(classify_codex_process(sandbox), CodexProcessKind::Helper);
    assert_eq!(classify_codex_process(cua), CodexProcessKind::Helper);
    assert_eq!(classify_codex_process(halcyon), CodexProcessKind::Helper);
    assert_eq!(classify_codex_process(other), CodexProcessKind::Other);
}

#[test]
fn find_codex_exe_picks_newest_hash_dir() {
    let dir = TempDir::new().unwrap();
    for (name, sleep_ms) in [("aaa", 0u64), ("bbb", 30u64)] {
        let exe_dir = dir.path().join("bin").join(name);
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::write(exe_dir.join("codex.exe"), b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
    }
    let found = halcyon_core::appserver::find_codex_exe(dir.path()).unwrap();
    // 按路径分量比较：写死 "bin\\bbb\\codex.exe" 只在 Windows 成立，
    // macOS 上分隔符不同会误判（2026-10-07 macOS CI 实测）。
    assert_eq!(
        found.file_name().and_then(|n| n.to_str()),
        Some("codex.exe"),
        "应选中 codex.exe: {found:?}"
    );
    assert_eq!(
        found
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str()),
        Some("bbb"),
        "应取 mtime 最新: {found:?}"
    );
}

#[test]
fn repair_history_appends_and_loads_in_order() {
    let dir = TempDir::new().unwrap();
    let ar = AutoRepair::load(dir.path());
    for (i, outcome) in ["done", "failed"].iter().enumerate() {
        ar.append_history(&RepairRecord {
            thread_id: format!("t-{i}"),
            title: Some(format!("任务{i}")),
            project: Some("proj".to_string()),
            kinds: RepairKinds::all(),
            outcome: outcome.to_string(),
            details: vec![format!("明细{i}")],
            executed_at: "2026-10-06 01:00:00".to_string(),
        });
    }
    let records = AutoRepair::load_history(dir.path());
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].thread_id, "t-0");
    assert_eq!(records[1].outcome, "failed");
    assert_eq!(records[1].details[0], "明细1");
    assert!(records[1].kinds.entries && records[1].kinds.model && records[1].kinds.roots);
    // 损坏行容错：整文件不是 JSONL 时回退为空，不报错
    std::fs::write(dir.path().join("repair-history.jsonl"), "not json\n").ok();
    let records2 = AutoRepair::load_history(dir.path());
    assert!(records2.is_empty(), "坏文件整体回退为空，不报错");
}

#[test]
fn enqueue_unified_never_queues_realtime_protected_entries() {
    // 2026-10-07 方针：仅含坏条目（实时保护类）的任务不得入队；
    // 含元数据类问题的任务入队时 entries 也必须为 false。
    use halcyon_core::unified_scan::UnifiedTask;
    let dir = TempDir::new().unwrap();
    let mut ar = AutoRepair::load(dir.path());
    let task = |tid: &str, bad: usize, model: Option<String>| UnifiedTask {
        thread_id: tid.to_string(),
        title: None,
        project: None,
        bad_entries: bad,
        model_mismatch: model,
        stale_roots: vec![],
        lineage_breaks: vec![],
    };
    let touched = ar.enqueue_unified(
        "k3",
        &[
            task("t-entries-only", 5, None),
            task("t-with-model", 3, Some("deepseek-flash".to_string())),
        ],
    );
    assert_eq!(touched, 1, "仅坏条目的任务不得入队");
    let item = &ar.queue.items[0];
    assert_eq!(item.thread_id, "t-with-model");
    assert!(!item.kinds.entries, "入队项的 entries 必须为 false");
    assert!(item.kinds.model);
}
