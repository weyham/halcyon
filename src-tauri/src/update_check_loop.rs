//! 定时自动更新检查：启动 60 秒后首次，之后每 3 小时一次。
//!
//! 行为：静默检查 → 有更新（非手动平台）→ 静默下载校验 → 写红点徽标并广播事件。
//! 安装永远由用户手动触发（关于页「安装并重启」）；徽标在用户装上并重启后、
//! 下一轮检查确认已是最新时自动清除。

use tauri::{AppHandle, Emitter, Manager};

use crate::ShimState;

const FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
const INTERVAL: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);

pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_DELAY);
        loop {
            tick(&app);
            std::thread::sleep(INTERVAL);
        }
    });
}

fn staging_root(_app: &AppHandle) -> std::path::PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(crate::auto_repair_loop::data_dir);
    exe_dir.join("updates").join("staging")
}

fn tick(app: &AppHandle) {
    let state = app.state::<ShimState>();
    let staging = staging_root(app);
    tauri::async_runtime::block_on(async {
        state.update.auto_tick(&staging).await;
        let badge = state.update.badge();
        *state.update_badge.lock().unwrap() = badge.clone();
        let _ = app.emit(
            "update-availability",
            serde_json::json!({
                "version": badge.as_ref().map(|b| b.version.clone()),
                "ready": badge.as_ref().map(|b| b.ready).unwrap_or(false),
                "manualOnly": badge.as_ref().map(|b| b.manual_only).unwrap_or(false),
            }),
        );
    });
    // 右键菜单是点击那一刻的快照：徽标变化后立即重建，让红点条目及时出现/消失。
    crate::tray::refresh_actions_menu(app);
}
