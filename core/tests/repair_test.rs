//! repair.rs 测试：合成 rollout 文件，验证扫描 / 修复 / 备份 / 幂等。不触碰真实 ~/.codex。

use std::path::{Path, PathBuf};

use halcyon_core::repair::*;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("crs-repair-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rollout_line(payload: &str) -> String {
    format!(r#"{{"timestamp":"2026-09-20T00:00:00Z","type":"response_item","payload":{payload}}}"#)
}

fn write_sample_rollout(dir: &Path) -> PathBuf {
    let path = dir.join("rollout-2026-09-20T00-00-00-deadbeef-0000-7000-8000-000000000000.jsonl");
    let meta = r#"{"timestamp":"2026-09-20T00:00:00Z","type":"session_meta","payload":{"id":"deadbeef-0000-7000-8000-000000000000"}}"#;
    let orphan = rollout_line(
        r#"{"type":"function_call_output","name":"send_message_to_thread","namespace":"codex_app","output":"请回复"}"#,
    );
    let legacy = rollout_line(
        r#"{"type":"web_search_call","action":{"type":"search","query":"rust async"}}"#,
    );
    let normal =
        rollout_line(r#"{"type":"function_call_output","call_id":"call_1","output":"ok"}"#);
    std::fs::write(&path, format!("{meta}\n{orphan}\n{legacy}\n{normal}\n")).unwrap();
    path
}

#[test]
fn parse_thread_id_accepts_deep_link_and_raw() {
    assert_eq!(
        parse_thread_id("codex://threads/01a0abcd-0000-7000-8000-000000000000"),
        "01a0abcd-0000-7000-8000-000000000000"
    );
    assert_eq!(parse_thread_id("codex://threads/01a0abcd/"), "01a0abcd");
    assert_eq!(parse_thread_id("  01a0abcd  "), "01a0abcd");
    assert_eq!(parse_thread_id(r#""01a0abcd""#), "01a0abcd");
}

#[test]
fn scan_finds_both_kinds_with_samples() {
    let dir = temp_dir("scan");
    let path = write_sample_rollout(&dir);
    let scan = scan_file(&path);
    assert_eq!(scan.orphan, 1);
    assert_eq!(scan.web_search, 1);
    assert_eq!(scan.samples.len(), 2);
    assert!(scan.samples[0].contains("孤儿注入"), "{}", scan.samples[0]);
    assert!(scan.samples[1].contains("旧搜索"), "{}", scan.samples[1]);
    std::fs::remove_dir_all(&dir).ok();
}

/// 全规则 fixture：孤儿 + 旧搜索 + 空文本 + 空 reasoning + 真实夹层（call 与
/// output 之间隔一条正常消息，逐项变换后夹层仍在，需序列级移动）。
fn write_all_kinds_rollout(dir: &Path) -> PathBuf {
    let path = dir.join("rollout-2026-09-20T00-00-00-aaaa1111-0000-7000-8000-000000000000.jsonl");
    let meta = r#"{"timestamp":"2026-09-20T00:00:00Z","type":"session_meta","payload":{"id":"aaaa1111-0000-7000-8000-000000000000"}}"#;
    let call = rollout_line(
        r#"{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":"{}"}"#,
    );
    let empty_msg = rollout_line(
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":""}]}"#,
    );
    let mid_msg = rollout_line(
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"中间说明"}]}"#,
    );
    let empty_reasoning = rollout_line(
        r#"{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"xyz"}"#,
    );
    let orphan = rollout_line(
        r#"{"type":"function_call_output","name":"send_message_to_thread","namespace":"codex_app","output":"请回复"}"#,
    );
    let output =
        rollout_line(r#"{"type":"function_call_output","call_id":"call_1","output":"ok"}"#);
    // 顺序：call → 空消息 → 中间消息 → 空 reasoning → 孤儿 → output
    std::fs::write(
        &path,
        format!("{meta}\n{call}\n{empty_msg}\n{mid_msg}\n{empty_reasoning}\n{orphan}\n{output}\n"),
    )
    .unwrap();
    path
}

#[test]
fn scan_counts_all_kinds() {
    let dir = temp_dir("all-scan");
    let path = write_all_kinds_rollout(&dir);
    let scan = scan_file(&path);
    assert_eq!(scan.orphan, 1);
    assert_eq!(scan.empty_message, 1);
    assert_eq!(scan.empty_reasoning, 1);
    assert_eq!(
        scan.tool_rounds, 1,
        "夹层（正常消息隔开 call 与 output）应检出"
    );
    assert_eq!(scan.total(), 4);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn repair_drops_empty_items_and_repairs_tool_round() {
    let dir = temp_dir("all-repair");
    let path = write_all_kinds_rollout(&dir);

    let (changed, dropped, backup) = repair_file(&path, "note", true).unwrap();
    assert!(backup.is_some(), "应有备份");
    assert_eq!(dropped, 2, "空文本 + 空 reasoning 各删一条");
    assert_eq!(changed, 2, "孤儿改写 1 + 工具回合移动 1");

    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let call_pos = lines
        .iter()
        .position(|l| l.contains("\"function_call\""))
        .unwrap();
    let out_pos = lines
        .iter()
        .position(|l| l.contains("\"function_call_output\""))
        .unwrap();
    assert_eq!(out_pos, call_pos + 1, "output 必须紧跟 call");
    assert!(!text.contains("encrypted_content"), "空 reasoning 应被删除");
    assert!(text.contains("跨任务消息"), "孤儿应改写成带来源标注的消息");

    // 幂等：再修一次无改动
    let (changed2, dropped2, _) = repair_file(&path, "note", true).unwrap();
    assert_eq!(changed2, 0);
    assert_eq!(dropped2, 0);
    // 修复后扫描干净
    assert_eq!(scan_file(&path).total(), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn repair_inserts_aborted_for_missing_output() {
    let dir = temp_dir("aborted");
    let path = dir.join("rollout-2026-09-20T00-00-00-bbbb2222-0000-7000-8000-000000000000.jsonl");
    let meta = r#"{"timestamp":"2026-09-20T00:00:00Z","type":"session_meta","payload":{"id":"bbbb2222-0000-7000-8000-000000000000"}}"#;
    let call = rollout_line(
        r#"{"type":"function_call","id":"fc_9","call_id":"call_9","name":"shell","arguments":"{}"}"#,
    );
    let tail = rollout_line(
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}"#,
    );
    std::fs::write(&path, format!("{meta}\n{call}\n{tail}\n")).unwrap();

    let scan = scan_file(&path);
    assert_eq!(scan.tool_rounds, 1, "缺 output 的 call 应被检出");

    let (changed, _, _) = repair_file(&path, "note", true).unwrap();
    assert_eq!(changed, 1);
    let text = std::fs::read_to_string(&path).unwrap();
    let call_pos = text.match_indices("call_9").next().unwrap().0;
    let aborted_pos = text.find("\"aborted\"").unwrap();
    assert!(aborted_pos > call_pos, "aborted 输出应插入 call 之后");
    assert!(text.contains("\"output\":\"aborted\""));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn repair_apply_backs_up_rewrites_and_is_idempotent() {
    let dir = temp_dir("apply");
    let path = write_sample_rollout(&dir);

    let report = repair_thread(&path.to_string_lossy(), "note", true).unwrap();
    assert_eq!(report.total_orphan, 1);
    assert_eq!(report.total_web_search, 1);
    assert_eq!(report.total_changed, 2, "孤儿改写 + 旧搜索 note 改写");
    assert_eq!(report.total_dropped, 0);
    assert_eq!(report.remaining, 0, "修复后复查应为 0");

    let file = &report.files[0];
    let backup = file.backup.as_ref().expect("应有备份");
    assert!(backup.is_file(), "备份文件应存在");
    let backup_text = std::fs::read_to_string(backup).unwrap();
    assert!(
        backup_text.contains("function_call_output"),
        "备份里应保留原始孤儿条目"
    );

    // 修复后的文件：孤儿变 user 消息带来源标注；旧搜索变历史记录说明
    let fixed = std::fs::read_to_string(&path).unwrap();
    assert!(
        fixed.contains("跨任务消息"),
        "孤儿应被改写成带来源标注的 user 消息：{fixed}"
    );
    assert!(
        !fixed.contains(r#""type":"function_call_output","name""#),
        "孤儿原形态不应存在"
    );
    assert!(
        fixed.contains("web search") || fixed.contains("历史"),
        "旧搜索应变说明条目：{fixed}"
    );
    // 正常条目不受影响
    assert!(
        fixed.contains(r#""call_id":"call_1""#),
        "正常条目必须原样保留"
    );

    // 幂等：再修一次 0 改动、不产新备份
    let again = repair_thread(&path.to_string_lossy(), "note", true).unwrap();
    assert_eq!(again.total_orphan, 0);
    assert_eq!(again.total_web_search, 0);
    assert_eq!(again.total_changed, 0);
    assert!(again.files[0].backup.is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn scan_only_mode_does_not_touch_file() {
    let dir = temp_dir("scanonly");
    let path = write_sample_rollout(&dir);
    let before = std::fs::read_to_string(&path).unwrap();
    let report = repair_thread(&path.to_string_lossy(), "note", false).unwrap();
    assert_eq!(report.total_orphan, 1);
    assert_eq!(report.remaining, 2);
    assert!(report.files[0].backup.is_none());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "扫描模式不得改文件"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_mode_removes_legacy_web_search() {
    let dir = temp_dir("drop");
    let path = write_sample_rollout(&dir);
    let report = repair_thread(&path.to_string_lossy(), "drop", true).unwrap();
    assert_eq!(report.total_changed, 1, "只有孤儿改写");
    assert_eq!(report.total_dropped, 1, "旧搜索被删");
    assert_eq!(report.remaining, 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_thread_gives_clear_error() {
    let err = repair_thread("nonexistent-thread-id-xyz", "note", false).unwrap_err();
    assert!(err.contains("找不到"), "{err}");
}

#[test]
fn repair_covers_all_thread_shards_and_chain_walks_to_root() {
    // 1a 实证形态：根分片含坏条目 + 压缩子分片干净。chain() 因子串误匹配
    // 曾断在子分片自身；修复范围也必须 = 扫描判定的文件集合（全部含 tid 分片）。
    let dir = temp_dir("scope");
    let tid = "aaaa2222-0000-7000-8000-000000000000";
    let root = dir.join(format!("rollout-2026-09-20T00-00-00-{tid}.jsonl"));
    let meta = format!(
        r#"{{"timestamp":"2026-09-20T00:00:00Z","type":"session_meta","payload":{{"id":"{tid}"}}}}"#
    );
    let empty_reasoning = rollout_line(
        r#"{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"xyz"}"#,
    );
    std::fs::write(&root, format!("{meta}\n{empty_reasoning}\n")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let child = dir.join(format!(
        "rollout-2026-09-21T00-00-00-{tid}_bbbb3333-0000-7000-8000-000000000000.jsonl"
    ));
    let child_meta = format!(
        r#"{{"timestamp":"2026-09-21T00:00:00Z","type":"session_meta","payload":{{"id":"{tid}","history_base":{{"thread_id":"{tid}","end_ordinal_exclusive":2,"end_byte_offset":10}}}}}}"#
    );
    let normal =
        rollout_line(r#"{"type":"function_call_output","call_id":"call_1","output":"ok"}"#);
    std::fs::write(&child, format!("{child_meta}\n{normal}\n")).unwrap();

    // chain 必须从子分片回溯到根分片（find_by_thread_id 精确匹配，不回配到自身）
    let walked = chain_with_roots(&child, std::slice::from_ref(&dir));
    assert_eq!(walked, vec![root.clone(), child.clone()]);

    let report = repair_thread_with_roots(tid, "note", true, std::slice::from_ref(&dir)).unwrap();
    assert_eq!(report.files.len(), 2, "修复范围必须覆盖全部含 tid 分片");
    assert_eq!(report.total_dropped, 1, "根分片的空 reasoning 必须被删除");
    assert_eq!(report.remaining, 0);
    assert_eq!(scan_file(&root).total(), 0);
    assert_eq!(scan_file(&child).total(), 0);
    // 幂等：再修一次什么也不做
    let again = repair_thread_with_roots(tid, "note", true, std::slice::from_ref(&dir)).unwrap();
    assert_eq!(again.total_changed, 0);
    assert_eq!(again.total_dropped, 0);
    std::fs::remove_dir_all(&dir).ok();
}

// ---------- ② 血缘回填：父分片截短后回填直接子分片 history_base ----------

/// 带 ordinal 的条目行。
fn ordinal_line(ordinal: u64, payload: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-09-20T00:00:00Z","ordinal":{ordinal},"type":"response_item","payload":{payload}}}"#
    )
}

/// 三级链 fixture：根分片（父）ordinal 1..=5，其中 ordinal=2 是空 reasoning
/// （修复会删除 → 父分片截短）；子分片 history_base 指向父（tid），
/// end_ordinal_exclusive=4，end_byte_offset 是旧值（必然过期）；
/// 孙分片 history_base 指向子分片后缀，偏移合法。返回 (dir, tid, parent, child, grandchild)。
fn write_three_level_chain(dir: &Path) -> (String, PathBuf, PathBuf, PathBuf) {
    let tid = "aaaa4444-0000-7000-8000-000000000000";
    let child_suffix = "bbbb5555-0000-7000-8000-000000000000";
    let grand_suffix = "cccc6666-0000-7000-8000-000000000000";

    let parent = dir.join(format!("rollout-2026-09-20T00-00-00-{tid}.jsonl"));
    let parent_meta = format!(
        r#"{{"timestamp":"2026-09-20T00:00:00Z","ordinal":1,"type":"session_meta","payload":{{"id":"{tid}"}}}}"#
    );
    let empty_reasoning = ordinal_line(
        2,
        r#"{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"xyz"}"#,
    );
    let r3 = ordinal_line(
        3,
        r#"{"type":"function_call","id":"fc_1","call_id":"call_1","name":"shell","arguments":"{}"}"#,
    );
    let r4 = ordinal_line(
        4,
        r#"{"type":"function_call_output","call_id":"call_1","output":"ok"}"#,
    );
    let r5 = ordinal_line(
        5,
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"完成"}]}"#,
    );
    std::fs::write(
        &parent,
        format!("{parent_meta}\n{empty_reasoning}\n{r3}\n{r4}\n{r5}\n"),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let child = dir.join(format!(
        "rollout-2026-09-21T00-00-00-{tid}_{child_suffix}.jsonl"
    ));
    let child_meta = format!(
        r#"{{"timestamp":"2026-09-21T00:00:00Z","ordinal":6,"type":"session_meta","payload":{{"id":"{tid}","history_base":{{"thread_id":"{tid}","end_ordinal_exclusive":4,"end_byte_offset":999999}}}}}}"#
    );
    let c7 = ordinal_line(
        7,
        r#"{"type":"function_call_output","call_id":"call_2","output":"ok"}"#,
    );
    std::fs::write(&child, format!("{child_meta}\n{c7}\n")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let grandchild = dir.join(format!(
        "rollout-2026-09-22T00-00-00-{tid}_{grand_suffix}.jsonl"
    ));
    let child_size = std::fs::metadata(&child).unwrap().len();
    let grand_meta = serde_json::json!({
        "timestamp": "2026-09-22T00:00:00Z",
        "ordinal": 8,
        "type": "session_meta",
        "payload": {
            "id": tid,
            "history_base": {
                "thread_id": child_suffix,
                "end_ordinal_exclusive": 7,
                "end_byte_offset": child_size
            }
        }
    })
    .to_string();
    std::fs::write(&grandchild, format!("{grand_meta}\n")).unwrap();

    (tid.to_string(), parent, child, grandchild)
}

#[test]
fn lineage_backfill_updates_direct_child_and_leaves_grandchild() {
    let dir = temp_dir("lineage");
    let (tid, parent, child, grandchild) = write_three_level_chain(&dir);
    let grandchild_before = std::fs::read(&grandchild).unwrap();
    let child_before_len = std::fs::metadata(&child).unwrap().len();

    let report = repair_thread_with_roots(&tid, "note", true, std::slice::from_ref(&dir)).unwrap();

    assert_eq!(report.total_dropped, 1, "父分片的空 reasoning 必须被删除");
    assert_eq!(
        report.lineage_errors.len(),
        0,
        "{:?}",
        report.lineage_errors
    );
    assert_eq!(report.lineage_backfills.len(), 1, "直接子分片必须回填一次");

    // 子分片：首行 end_byte_offset 指向修复后父分片中 ordinal=4 记录的起始字节
    let parent_after = std::fs::read(&parent).unwrap();
    let boundary = {
        let mut pos = 0usize;
        let mut found = None;
        for chunk in parent_after.split(|b| *b == b'\n') {
            if chunk.is_empty() {
                pos += 1;
                continue;
            }
            let v: serde_json::Value = serde_json::from_slice(chunk).unwrap();
            if v.get("ordinal").and_then(serde_json::Value::as_u64) == Some(4) {
                found = Some(pos);
                break;
            }
            pos += chunk.len() + 1;
        }
        found.unwrap()
    };
    let child_first = std::fs::read_to_string(&child)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(
        child_first.contains(&format!("\"end_byte_offset\":{boundary}")),
        "子分片首行必须指向边界记录新位置（{boundary}），实际: {}",
        &child_first[..child_first.len().min(120)]
    );
    // 子分片只改首行：总长度变化 = 新旧偏移数字位数差；内容行原样
    let child_after_len = std::fs::metadata(&child).unwrap().len();
    assert_ne!(
        child_before_len, child_after_len,
        "旧偏移 999999 是 6 位，新偏移 8 位"
    );
    // 子分片备份存在
    let bak_count = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".bak-lineage-"))
        .count();
    assert_eq!(bak_count, 1, "子分片必须有一个血缘备份");

    // 孙分片指向子分片（子分片条目共性内容未被截短），必须原样不动
    assert_eq!(
        std::fs::read(&grandchild).unwrap(),
        grandchild_before,
        "孙分片不得被回填"
    );

    // 幂等：再修一次，无回填无错误
    let again = repair_thread_with_roots(&tid, "note", true, std::slice::from_ref(&dir)).unwrap();
    assert!(again.lineage_backfills.is_empty());
    assert!(again.lineage_errors.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lineage_backfill_falls_back_to_last_record_before_boundary() {
    // 边界 ordinal 在修复后不存在（超过父分片最大 ordinal）→ 退化为最后一条
    // ordinal < excl 记录的结束偏移（= 文件末尾）。
    let dir = temp_dir("lineage-fallback");
    let tid = "aaaa7777-0000-7000-8000-000000000000";
    let parent = dir.join(format!("rollout-2026-09-20T00-00-00-{tid}.jsonl"));
    let meta = format!(
        r#"{{"timestamp":"2026-09-20T00:00:00Z","ordinal":1,"type":"session_meta","payload":{{"id":"{tid}"}}}}"#
    );
    let empty_reasoning = ordinal_line(
        2,
        r#"{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"xyz"}"#,
    );
    let r3 = ordinal_line(
        3,
        r#"{"type":"function_call_output","call_id":"call_1","output":"ok"}"#,
    );
    std::fs::write(&parent, format!("{meta}\n{empty_reasoning}\n{r3}\n")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let child = dir.join(format!(
        "rollout-2026-09-21T00-00-00-{tid}_bbbb8888-0000-7000-8000-000000000000.jsonl"
    ));
    let child_meta = format!(
        r#"{{"timestamp":"2026-09-21T00:00:00Z","ordinal":9,"type":"session_meta","payload":{{"id":"{tid}","history_base":{{"thread_id":"{tid}","end_ordinal_exclusive":99,"end_byte_offset":1}}}}}}"#
    );
    std::fs::write(&child, format!("{child_meta}\n")).unwrap();

    let report = repair_thread_with_roots(tid, "note", true, std::slice::from_ref(&dir)).unwrap();
    assert_eq!(report.lineage_errors.len(), 0);
    assert_eq!(report.lineage_backfills.len(), 1);
    let parent_len = std::fs::metadata(&parent).unwrap().len();
    let child_first = std::fs::read_to_string(&child)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(
        child_first.contains(&format!("\"end_byte_offset\":{parent_len}")),
        "退化值必须是父分片修复后的文件末尾（{parent_len}）"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lineage_backfill_errors_without_boundary_and_never_guesses() {
    // 父分片没有任何 ordinal 记录 → 报错跳过，子分片原样不动（绝不写猜测值）。
    let dir = temp_dir("lineage-error");
    let tid = "aaaa9999-0000-7000-8000-000000000000";
    let parent = dir.join(format!("rollout-2026-09-20T00-00-00-{tid}.jsonl"));
    // 无 ordinal 的条目（旧格式分片形态）
    let meta = format!(
        r#"{{"timestamp":"2026-09-20T00:00:00Z","type":"session_meta","payload":{{"id":"{tid}"}}}}"#
    );
    let empty_reasoning = rollout_line(
        r#"{"type":"reasoning","id":"rs_1","summary":[],"content":[],"encrypted_content":"xyz"}"#,
    );
    std::fs::write(&parent, format!("{meta}\n{empty_reasoning}\n")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let child = dir.join(format!(
        "rollout-2026-09-21T00-00-00-{tid}_bbbb0000-0000-7000-8000-000000000000.jsonl"
    ));
    let child_meta = format!(
        r#"{{"timestamp":"2026-09-21T00:00:00Z","type":"session_meta","payload":{{"id":"{tid}","history_base":{{"thread_id":"{tid}","end_ordinal_exclusive":4,"end_byte_offset":999999}}}}}}"#
    );
    std::fs::write(&child, format!("{child_meta}\n")).unwrap();
    let child_before = std::fs::read(&child).unwrap();

    let report = repair_thread_with_roots(tid, "note", true, std::slice::from_ref(&dir)).unwrap();
    assert_eq!(report.total_dropped, 1, "父分片照常修复");
    assert_eq!(report.lineage_backfills.len(), 0);
    assert_eq!(
        report.lineage_errors.len(),
        1,
        "找不到边界必须报错并面板可见"
    );
    assert!(
        report.lineage_errors[0].contains("ordinal 边界"),
        "{}",
        report.lineage_errors[0]
    );
    assert_eq!(
        std::fs::read(&child).unwrap(),
        child_before,
        "报错时子分片必须原样"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn offline_repair_normalizes_agent_message() {
    // 第 6 类规则：agent_message → assistant message（真实形态见 rewrite.rs 测试）。
    let dir = temp_dir("agent-msg");
    let tid = "aaaa6666-0000-7000-8000-000000000000";
    let path = dir.join(format!("rollout-2026-09-20T00-00-00-{tid}.jsonl"));
    let meta = format!(
        r#"{{"timestamp":"2026-09-20T00:00:00Z","type":"session_meta","payload":{{"id":"{tid}"}}}}"#
    );
    let am = rollout_line(
        r#"{"type":"agent_message","id":"amsg_1","author":"/root/sub","recipient":"/root","content":[{"type":"input_text","text":"Message Type: FINAL_ANSWER\nPayload:\n完成"}]}"#,
    );
    std::fs::write(&path, format!("{meta}\n{am}\n")).unwrap();

    let scan = scan_file(&path);
    assert_eq!(scan.agent_message, 1);
    assert_eq!(scan.total(), 1);

    let report = repair_thread_with_roots(tid, "note", true, std::slice::from_ref(&dir)).unwrap();
    assert_eq!(report.total_changed, 1, "agent_message 必须改写");
    assert_eq!(report.remaining, 0);
    assert_eq!(scan_file(&path).total(), 0, "改写后扫描必须清空");

    // 改写后的条目形态：assistant message，正文逐字保留
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let payload = &lines[1]["payload"];
    assert_eq!(payload["type"], "message");
    assert_eq!(payload["role"], "assistant");
    assert_eq!(
        payload["content"][0]["text"].as_str().unwrap(),
        "Message Type: FINAL_ANSWER\nPayload:\n完成"
    );
    std::fs::remove_dir_all(&dir).ok();
}
