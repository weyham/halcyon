//! Codex app-server 官方客户端（stdio JSON-RPC）。
//!
//! 用途：离线修复（模型 / cwd 覆盖）——spawn 桌面版捆绑的
//! `codex.exe app-server`，initialize 握手后顺序收发请求，用毕即关。
//! 协议行为以官方 app-server 实际响应为准（匿名读取公开仓 Releases）。

use std::io::{BufRead, BufReader, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

/// 抑制子进程控制台窗口（GUI 主进程拉起控制台子系统进程时，Windows 会分配
/// 一个可见 CMD 窗口；stdio 管道不受该标志影响）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CLIENT_NAME: &str = "halcyon";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 在桌面安装目录下找最新的捆绑 `codex.exe`
///（`bin\<hash>\codex.exe`，按 mtime 取最新）。
pub fn find_codex_exe(install_root: &Path) -> Option<PathBuf> {
    let bin_dir = install_root.join("bin");
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(bin_dir).ok()?.flatten() {
        let exe = entry.path().join("codex.exe");
        if exe.is_file() {
            let mtime = exe
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((mtime, exe));
        }
    }
    candidates.sort_by_key(|(mtime, _)| *mtime);
    candidates.pop().map(|(_, exe)| exe)
}

/// Windows 桌面版的默认安装根。
#[cfg(windows)]
pub fn default_install_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("OpenAI").join("Codex"))
}

#[cfg(not(windows))]
pub fn default_install_root() -> Option<PathBuf> {
    None
}

/// 短生命周期客户端：一个实例只做一轮修复操作。
pub struct AppServerClient {
    child: Child,
    stdin: ChildStdin,
    inbox: Receiver<Value>,
    next_id: u64,
}

impl AppServerClient {
    /// 子进程 pid。修复循环在"Codex 是否在运行"的判定里必须排除自己拉起的
    /// app-server——它同样是 `codex.exe`，否则会被误判成用户在跑 Codex，
    /// 导致每轮只处理一条就中止（表现为"每 3 秒修一个 + 一堆通知"）。
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// spawn `codex.exe app-server --listen stdio://` 并启动读循环。
    pub fn spawn(codex_exe: &Path, cwd: &Path) -> Result<Self, String> {
        let mut cmd = Command::new(codex_exe);
        cmd.args(["app-server", "--listen", "stdio://"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn app-server 失败: {e}"))?;
        let stdin = child.stdin.take().ok_or("无法获取 app-server stdin")?;
        let stdout = child.stdout.take().ok_or("无法获取 app-server stdout")?;
        let (tx, rx) = channel::<Value>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            }
        });
        let mut client = Self {
            child,
            stdin,
            inbox: rx,
            next_id: 0,
        };
        client.initialize()?;
        Ok(client)
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": CLIENT_NAME,
                    "title": "Halcyon",
                    "version": CLIENT_VERSION,
                }
            }),
        )
        .map(|_| ())?;
        self.write(&json!({ "method": "initialized" }))
    }

    fn write(&mut self, msg: &Value) -> Result<(), String> {
        let line = serde_json::to_string(msg).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("写入 app-server 失败: {e}"))
    }

    /// 发送请求并等待匹配 id 的响应；期间的通知按需回调收集。
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({ "method": method, "params": params, "id": id }))?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return Err(format!("app-server 请求超时: {method}"));
            }
            let msg = self
                .inbox
                .recv_timeout(remain)
                .map_err(|_| format!("app-server 请求超时: {method}"))?;
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                // 通知或其他连接的响应：修复场景无需处理，丢弃。
                continue;
            }
            if let Some(error) = msg.get("error") {
                return Err(format!(
                    "app-server {method} 返回错误: {}",
                    error.get("message").and_then(Value::as_str).unwrap_or("?")
                ));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// `thread/resume` 带模型 / cwd 覆盖：Codex 在 rollout 末尾追加官方
    /// `thread_settings_applied`，后续 turn 粘性生效（spike 已验证）。
    pub fn resume_with_overrides(
        &mut self,
        thread_id: &str,
        model: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(), String> {
        let mut params = json!({ "threadId": thread_id });
        if let Some(model) = model {
            params["model"] = Value::String(model.to_string());
        }
        if let Some(cwd) = cwd {
            params["cwd"] = Value::String(cwd.to_string());
        }
        self.request("thread/resume", params).map(|_| ())
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 供测试使用的 JSON-RPC id 匹配逻辑（与传输解耦）。
pub fn response_matches(msg: &Value, id: u64) -> bool {
    msg.get("id").and_then(Value::as_u64) == Some(id)
        && (msg.get("result").is_some() || msg.get("error").is_some())
}
