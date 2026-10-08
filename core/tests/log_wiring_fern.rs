//! 最小复现：fern 接线（与 tauri-plugin-log Builder 的 Dispatch target
//! 链结构完全一致）是否把 log:: 宏事件送达到 TrackedRotatingWriter 并落盘。
//! 全局 logger 每进程只能装一次，因此本文件只放这一个测试。

use std::sync::Arc;

use halcyon_core::logging::{LogHealth, TrackedRotatingWriter, LOG_MAX_SIZE};

#[test]
fn log_events_reach_tracked_writer_through_nested_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("halcyon.log");
    let fallback_path = dir.path().join("halcyon.log.fallback");
    let health = Arc::new(LogHealth::new());

    let writer =
        TrackedRotatingWriter::new(&log_path, &fallback_path, health.clone(), LOG_MAX_SIZE);
    // 与 src-tauri/src/lib.rs 相同的用户 dispatch
    let file_dispatch = fern::Dispatch::new().chain(fern::Output::writer(Box::new(writer), "\n"));
    // 与 tauri-plugin-log acquire_logger 相同：每个 target 包一层新 dispatch，
    // 用户 dispatch 作为 Output::Dispatch 链接进去
    let target_dispatch = fern::Dispatch::new().chain(file_dispatch);
    // 根 dispatch：带 format + Info 级别（同 plugin Builder::default().level(Info)）
    let root = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}] {}",
                record.target(),
                record.level(),
                message
            ))
        })
        .level(log::LevelFilter::Info)
        .chain(target_dispatch);
    let (max_level, logger) = root.into_log();
    log::set_boxed_logger(logger).expect("全局 logger 只能装一次");
    log::set_max_level(max_level);

    log::info!("setup 内的启动日志");
    log::warn!("warn 级别也应落盘");
    log::debug!("debug 不应落盘");

    let content = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("日志文件必须存在且可读: {e}"));
    assert!(
        content.contains("setup 内的启动日志"),
        "log::info! 必须落盘，实际内容: {content:?}"
    );
    assert!(
        content.contains("warn 级别也应落盘"),
        "log::warn! 必须落盘，实际内容: {content:?}"
    );
    assert!(
        !content.contains("debug 不应落盘"),
        "debug 必须被 Info 级别过滤: {content:?}"
    );
    let snapshot = health.snapshot_json();
    assert_eq!(snapshot["log_writable"], serde_json::json!(true));
    assert_eq!(snapshot["log_mode"], serde_json::json!("primary"));
}
