//! 余额/额度轮询：每 5 分钟一次 + 收到刷新信号立即一次；
//! 还在等 key 的阶段（刚启动/刚重启，内存 keyring 是空的）用 15 秒的快节拍补查，
//! 这样第一笔流量进来后很快就能出数，而不是干等一整个 5 分钟周期。
//! key 来自内存 KeyRing（过路流量暂存，绝不落盘）；
//! 结果（结构化档位行 + 原始响应文本）供用量面板展示，不进托盘菜单。

use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use halcyon_core::balance;

use crate::{RouteBalance, ShimState};

pub const POLL_INTERVAL: Duration = Duration::from_secs(300);

/// 还在等 key 时的补查节拍。没有 key 时 `poll_once` 只写一行状态、不发网络请求，
/// 所以这个频率不会增加对供应商 API 的调用量。
const WARMUP_INTERVAL: Duration = Duration::from_secs(15);

/// 启动轮询线程；返回手动刷新信号的发送端（面板"刷新"按钮触发）。
pub fn spawn(app: AppHandle) -> Sender<()> {
    let (tx, rx) = channel::<()>();
    std::thread::spawn(move || poll_loop(app, rx));
    tx
}

fn poll_loop(app: AppHandle, rx: Receiver<()>) {
    loop {
        let waiting_for_key = poll_once(&app);
        // 等 5 分钟（还在等 key 时只等 15 秒），或收到刷新信号立刻再来一轮
        let wait = if waiting_for_key {
            WARMUP_INTERVAL
        } else {
            POLL_INTERVAL
        };
        let _ = rx.recv_timeout(wait);
    }
}

/// 查一轮所有路由；返回是否还有路由在等 key（调用方据此决定下一次的等待时长）。
fn poll_once(app: &AppHandle) -> bool {
    let state = app.state::<ShimState>();
    let routes = state.routes.lock().unwrap().clone();
    if routes.is_empty() {
        return false;
    }
    let keyring = state.server.lock().unwrap().as_ref().map(|h| h.keyring());
    let mut waiting_for_key = false;
    for route in &routes {
        let Some(query) = balance::resolve_query(route) else {
            continue; // 未识别 / template=none：不展示
        };
        let auth = keyring
            .as_ref()
            .and_then(|k| k.lock().unwrap().get(&route.name).cloned());
        let result = match auth {
            None => {
                waiting_for_key = true;
                RouteBalance {
                    rows: Vec::new(),
                    status: "等待首次请求".to_string(),
                    raw: None,
                    fetched_at_ms: None,
                }
            }
            Some(auth) => match balance::fetch_json(&query.url, &auth) {
                Ok(body) => {
                    let raw = serde_json::to_string_pretty(&body).ok();
                    match balance::parse_custom(&query.spec, &body) {
                        Ok(rows) => RouteBalance {
                            rows,
                            status: "ok".to_string(),
                            raw,
                            fetched_at_ms: Some(now_ms()),
                        },
                        Err(e) => RouteBalance {
                            rows: Vec::new(),
                            status: format!("解析失败：{e}"),
                            raw,
                            fetched_at_ms: None,
                        },
                    }
                }
                Err(e) => RouteBalance {
                    rows: Vec::new(),
                    status: format!("查询失败：{e}"),
                    raw: None,
                    fetched_at_ms: None,
                },
            },
        };
        state
            .balance_rows
            .lock()
            .unwrap()
            .insert(route.name.clone(), result);
    }
    waiting_for_key
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
