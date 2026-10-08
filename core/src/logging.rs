//! 文件日志目标与可用性状态。
//!
//! tauri-plugin-log 的内建文件轮转器在 Windows 上会带着打开句柄删除当前日志；
//! 轮转失败后 fern 不把写入错误暴露给调用方，后续日志会静默停写。这里改用
//! 可测试的自研 writer：先关闭再重命名，主路径失败写相邻 fallback，并把结果
//! 暴露给 `/health`。状态只记录路径与 IO 错误，不包含日志正文。

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub const LOG_MAX_SIZE: u64 = 10 * 1024 * 1024;
pub const LOG_BUFFER_LIMIT: usize = 64 * 1024;

#[derive(Debug, Default)]
struct LogHealthInner {
    primary_path: Option<PathBuf>,
    fallback_path: Option<PathBuf>,
    writable: bool,
    fallback_mode: bool,
    write_failures: u64,
    last_error: Option<String>,
    /// 最近一次成功落盘的时间（epoch 毫秒）；None = 本进程从未写成功。
    /// 用于区分"writable 是很久以前的成功"与"正在持续写入"——排查中
    /// 曾出现 writable=true 但文件零写入的假健康状态。
    last_write_ms: Option<u64>,
}

/// 文件日志目标的运行期健康状态。
#[derive(Debug, Default)]
pub struct LogHealth {
    inner: Mutex<LogHealthInner>,
}

impl LogHealth {
    pub fn new() -> Self {
        Self::default()
    }

    fn initialize(&self, primary: PathBuf, fallback: PathBuf) {
        let mut inner = self.inner.lock().unwrap();
        inner.primary_path = Some(primary);
        inner.fallback_path = Some(fallback);
        inner.writable = false;
        inner.fallback_mode = false;
        inner.write_failures = 0;
        inner.last_error = None;
        inner.last_write_ms = None;
    }

    fn record_primary_success(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.writable = true;
        inner.fallback_mode = false;
        inner.last_error = None;
        inner.last_write_ms = Some(now_epoch_ms());
    }

    fn record_fallback_success(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.writable = true;
        inner.fallback_mode = true;
        inner.last_write_ms = Some(now_epoch_ms());
    }

    fn record_failure(&self, error: &io::Error) {
        let mut inner = self.inner.lock().unwrap();
        inner.writable = false;
        inner.fallback_mode = false;
        inner.write_failures += 1;
        inner.last_error = Some(error.to_string());
    }

    /// 供 `/health` 输出；不包含任何日志正文。
    pub fn snapshot_json(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({
            "log_writable": inner.writable,
            "log_mode": if inner.fallback_mode { "fallback" } else { "primary" },
            "log_path": inner.primary_path.as_ref().map(|p| p.to_string_lossy()),
            "log_fallback_path": inner
                .fallback_path
                .as_ref()
                .map(|p| p.to_string_lossy()),
            "log_write_failures": inner.write_failures,
            "log_last_error": inner.last_error,
            "log_last_write_ms": inner.last_write_ms,
        })
    }
}

/// 带健康状态与 fallback 的轮转文件 writer。
pub struct TrackedRotatingWriter {
    primary_path: PathBuf,
    fallback_path: PathBuf,
    primary_file: Option<File>,
    fallback_file: Option<File>,
    primary_size: u64,
    fallback_size: u64,
    buffer: Vec<u8>,
    max_size: u64,
    health: Arc<LogHealth>,
}

impl TrackedRotatingWriter {
    pub fn new(
        primary_path: impl Into<PathBuf>,
        fallback_path: impl Into<PathBuf>,
        health: Arc<LogHealth>,
        max_size: u64,
    ) -> Self {
        let primary_path = primary_path.into();
        let fallback_path = fallback_path.into();
        if let Some(parent) = primary_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Some(parent) = fallback_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        health.initialize(primary_path.clone(), fallback_path.clone());
        Self {
            primary_path,
            fallback_path,
            primary_file: None,
            fallback_file: None,
            primary_size: 0,
            fallback_size: 0,
            buffer: Vec::new(),
            max_size: max_size.max(1),
            health,
        }
    }

    fn append_with_rotation(
        path: &Path,
        mut file: Option<File>,
        size: &mut u64,
        bytes: &[u8],
        max_size: u64,
    ) -> io::Result<Option<File>> {
        if let Some(mut handle) = file.take() {
            let _ = handle.flush();
            drop(handle);
        }
        if file.is_none() {
            file = Some(OpenOptions::new().create(true).append(true).open(path)?);
            *size = file
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .map_or(0, |m| m.len());
        }

        if *size != 0 && *size + bytes.len() as u64 > max_size {
            if let Some(mut handle) = file.take() {
                let _ = handle.flush();
                // 显式 drop：Windows 不能在句柄仍打开时安全 rename/delete。
                drop(handle);
            }
            let rotated = rotated_path(path);
            if let Err(error) = std::fs::rename(path, &rotated) {
                // 重命名失败时继续追加旧 active 文件，宁可超限也不丢日志。
                file = Some(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .map_err(|reopen_error| {
                            io::Error::other(format!(
                                "rotate failed: {error}; reopen failed: {reopen_error}"
                            ))
                        })?,
                );
            } else {
                file = Some(OpenOptions::new().create(true).append(true).open(path)?);
                *size = 0;
            }
        }

        if let Some(handle) = file.as_mut() {
            handle.write_all(bytes)?;
            handle.flush()?;
            *size += bytes.len() as u64;
        }
        Ok(file)
    }

    fn truncate_failed_buffer(&mut self) {
        if self.buffer.len() > LOG_BUFFER_LIMIT {
            let start = self.buffer.len() - LOG_BUFFER_LIMIT;
            self.buffer.drain(..start);
        }
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn rotated_path(path: &Path) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let mut candidate = path.with_file_name(format!(
        "{}.{millis}.rotated",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("halcyon.log")
    ));
    for index in 1..1000 {
        if !candidate.exists() {
            return candidate;
        }
        candidate = path.with_file_name(format!(
            "{}.{millis}-{index}.rotated",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("halcyon.log")
        ));
    }
    candidate
}

impl Write for TrackedRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        match Self::append_with_rotation(
            &self.primary_path,
            self.primary_file.take(),
            &mut self.primary_size,
            &self.buffer,
            self.max_size,
        ) {
            Ok(file) => {
                self.primary_file = file;
                if let Some(mut fallback) = self.fallback_file.take() {
                    let _ = fallback.flush();
                }
                self.buffer.clear();
                self.health.record_primary_success();
                Ok(())
            }
            Err(primary_error) => {
                let fallback_result = Self::append_with_rotation(
                    &self.fallback_path,
                    self.fallback_file.take(),
                    &mut self.fallback_size,
                    &self.buffer,
                    self.max_size,
                );
                match fallback_result {
                    Ok(file) => {
                        self.fallback_file = file;
                        self.buffer.clear();
                        self.health.record_fallback_success();
                        self.health.record_primary_error_for_health(&primary_error);
                        Ok(())
                    }
                    Err(fallback_error) => {
                        self.truncate_failed_buffer();
                        self.health.record_failure(&io::Error::other(format!(
                            "primary: {primary_error}; fallback: {fallback_error}"
                        )));
                        Err(fallback_error)
                    }
                }
            }
        }
    }
}

impl LogHealth {
    fn record_primary_error_for_health(&self, error: &io::Error) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_error = Some(format!("primary: {error}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_primary_and_reports_health() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("halcyon.log");
        let fallback = dir.path().join("halcyon.log.fallback");
        let health = Arc::new(LogHealth::new());
        let mut writer =
            TrackedRotatingWriter::new(&primary, &fallback, health.clone(), LOG_MAX_SIZE);
        writer.write_all(b"first\n").unwrap();
        writer.flush().unwrap();

        assert_eq!(std::fs::read(&primary).unwrap(), b"first\n");
        let snapshot = health.snapshot_json();
        assert_eq!(snapshot["log_writable"], json!(true));
        assert_eq!(snapshot["log_mode"], json!("primary"));
        assert_eq!(snapshot["log_write_failures"], json!(0));
        assert!(
            snapshot["log_last_write_ms"].as_u64().is_some(),
            "成功写入后必须记录最近落盘时间"
        );
    }

    #[test]
    fn fresh_health_has_no_last_write() {
        let dir = tempfile::tempdir().unwrap();
        let health = Arc::new(LogHealth::new());
        let _writer = TrackedRotatingWriter::new(
            dir.path().join("halcyon.log"),
            dir.path().join("halcyon.log.fallback"),
            health.clone(),
            LOG_MAX_SIZE,
        );
        let snapshot = health.snapshot_json();
        assert_eq!(
            snapshot["log_writable"],
            json!(false),
            "初始化后未写入应为不可写"
        );
        assert!(
            snapshot["log_last_write_ms"].is_null(),
            "从未写入时 last_write_ms 必须为 null"
        );
    }

    #[test]
    fn primary_failure_falls_back_and_remains_visible() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("blocked");
        std::fs::create_dir(&primary).unwrap();
        let fallback = dir.path().join("halcyon.log.fallback");
        let health = Arc::new(LogHealth::new());
        let mut writer =
            TrackedRotatingWriter::new(&primary, &fallback, health.clone(), LOG_MAX_SIZE);
        writer.write_all(b"fallback\n").unwrap();
        writer.flush().unwrap();

        assert_eq!(std::fs::read(&fallback).unwrap(), b"fallback\n");
        let snapshot = health.snapshot_json();
        assert_eq!(snapshot["log_writable"], json!(true));
        assert_eq!(snapshot["log_mode"], json!("fallback"));
        assert!(snapshot["log_last_error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("primary:"));
    }

    #[test]
    fn both_failures_make_health_false_and_bound_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("blocked-primary");
        let fallback = dir.path().join("blocked-fallback");
        std::fs::create_dir(&primary).unwrap();
        std::fs::create_dir(&fallback).unwrap();
        let health = Arc::new(LogHealth::new());
        let mut writer =
            TrackedRotatingWriter::new(&primary, &fallback, health.clone(), LOG_MAX_SIZE);
        let big = vec![b'x'; LOG_BUFFER_LIMIT + 128];
        writer.write_all(&big).unwrap();
        assert!(writer.flush().is_err());
        assert!(writer.buffer.len() <= LOG_BUFFER_LIMIT);

        let snapshot = health.snapshot_json();
        assert_eq!(snapshot["log_writable"], json!(false));
        assert_eq!(snapshot["log_write_failures"], json!(1));
    }

    #[test]
    fn rotation_closes_file_before_renaming() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("halcyon.log");
        let fallback = dir.path().join("halcyon.log.fallback");
        std::fs::write(&primary, vec![b'a'; 32]).unwrap();
        let health = Arc::new(LogHealth::new());
        let mut writer = TrackedRotatingWriter::new(&primary, &fallback, health.clone(), 32);
        writer.write_all(b"second\n").unwrap();
        writer.flush().unwrap();

        let rotated = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".rotated"))
            .expect("should rotate after closing the old handle");
        assert_eq!(std::fs::read(rotated.path()).unwrap(), vec![b'a'; 32]);
        assert_eq!(std::fs::read(&primary).unwrap(), b"second\n");
        assert_eq!(health.snapshot_json()["log_writable"], json!(true));
    }

    #[test]
    fn externally_moved_active_file_is_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("halcyon.log");
        let moved = dir.path().join("halcyon.log.moved");
        let fallback = dir.path().join("halcyon.log.fallback");
        let health = Arc::new(LogHealth::new());
        let mut writer =
            TrackedRotatingWriter::new(&primary, &fallback, health.clone(), LOG_MAX_SIZE);
        writer.write_all(b"first\n").unwrap();
        writer.flush().unwrap();
        std::fs::rename(&primary, &moved).unwrap();

        writer.write_all(b"second\n").unwrap();
        writer.flush().unwrap();

        assert_eq!(std::fs::read(&moved).unwrap(), b"first\n");
        assert_eq!(std::fs::read(&primary).unwrap(), b"second\n");
        let snapshot = health.snapshot_json();
        assert_eq!(snapshot["log_writable"], json!(true));
        assert_eq!(snapshot["log_mode"], json!("primary"));
    }
}
