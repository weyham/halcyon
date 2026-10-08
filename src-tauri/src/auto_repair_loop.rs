//! 模型切换自动修复的后台循环：
//! 监听 config.toml 默认模型变化 → 扫描入队 → Codex 未运行时自动执行。
//! 行为约定见内部 backlog（无开关；队列只放待执行项，执行前可取消）。

use std::path::PathBuf;

use tauri::{AppHandle, Emitter, Manager};

use halcyon_core::appserver;
use halcyon_core::auto_repair;

use crate::ShimState;

pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .map(|h| h.join(".codex"))
}

pub fn data_dir() -> PathBuf {
    halcyon_core::data_dir::resolve(
        halcyon_core::data_dir::parse_cli_data_dir(&std::env::args().collect::<Vec<_>>())
            .as_deref(),
    )
    .path
}

pub fn load_auto_repair() -> auto_repair::AutoRepair {
    auto_repair::AutoRepair::load(&data_dir())
}

/// 后台 tick：每 3 秒一次。
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || loop {
        tick(&app);
        std::thread::sleep(std::time::Duration::from_secs(3));
    });
}

fn tick(app: &AppHandle) {
    let Some(home) = codex_home() else {
        return;
    };
    let state = app.state::<ShimState>();
    let Some(info) = state.model_info.get() else {
        return;
    };
    let sessions = home.join("sessions");

    // 1) 变化检测：短锁，锁内只做状态比对。
    //    纪律：锁内绝不调用会再次取锁或可能阻塞的函数（如 emit / 扫描）。
    let changed = {
        let mut guard = state.auto_repair.lock().unwrap();
        match guard.as_mut() {
            Some(ar) => ar.observe_default_model(&info),
            None => return,
        }
    };

    // 2) 扫描必须在锁外做（30-40 秒），只在合并入队时短暂加锁。
    if changed {
        // 扫描要 30-40 秒：这段时间也要有可见反馈（进度卡片不确定进度 + 托盘蓝点），
        // 否则用户只看到一个没有任何动静的等待。
        *state.auto_repair_progress.lock().unwrap() = Some(crate::RepairProgress::scanning(
            format!("默认模型已切换为 {}", info.default_model),
        ));
        let scanned = halcyon_core::model_repair::scan_model_mismatches(&sessions, &info);
        let scanned_len = scanned.len();
        // 只自动修复「有项目归属」的线程（2026-10-06 用户裁定）：
        // 无项目的散装/临时会话与自动审查线程不进自动修复范围。
        let tasks = auto_repair::filter_for_auto_repair(&sessions, scanned);
        let skipped = scanned_len.saturating_sub(tasks.len());
        let added = {
            let mut guard = state.auto_repair.lock().unwrap();
            match guard.as_mut() {
                Some(ar) => {
                    let added = ar.replace_pending(&info.default_model, tasks);
                    ar.save();
                    added
                }
                None => 0,
            }
        };
        log::info!(
            "检测到默认模型切换为 {}：扫描命中 {} 个，按「仅项目线程」过滤后入队 {}（跳过 {}）",
            info.default_model,
            scanned_len,
            added,
            skipped
        );
        // 扫描结束：Codex 正在运行时本轮只能排队（不执行），卡片必须如实说明；
        // 未运行时交给下面的 try_execute 立刻执行（它会自己写"进行中"进度）。
        *state.auto_repair_progress.lock().unwrap() = None;
        if auto_repair::is_codex_running(&[]) {
            *state.auto_repair_notice.lock().unwrap() = Some(crate::RepairNotice::new(
                "已排队 · 等待修复",
                if added == 0 {
                    "没有需要修复的任务（Codex 正在运行）".to_string()
                } else {
                    format!("Codex 正在运行：退出后自动修复 {added} 个任务")
                },
            ));
        }
        emit_queue(app, &state);
    }

    if let Some((done, failed)) = try_execute(app) {
        log::info!("模型切换自动修复执行完成：成功 {done}，失败 {failed}");
        emit_queue(app, &state);
        if done + failed > 0 {
            use tauri_plugin_notification::NotificationExt;
            let body = if failed == 0 {
                format!("成功修复 {done} 个任务")
            } else {
                format!("成功 {done} 个 · 失败 {failed} 个（面板可查看详情并重试）")
            };
            let _ = app
                .notification()
                .builder()
                .title("Halcyon 自动修复完成")
                .body(&body)
                .show();
        }
    }
}

/// 条件满足时执行队列：Codex 未运行 + 无并发执行 + 有 Pending。
/// 返回 Some((done, failed)) 表示本轮真的执行了。
pub fn try_execute(app: &AppHandle) -> Option<(usize, usize)> {
    let state = app.state::<ShimState>();
    {
        // 统一锁顺序：auto_repair → auto_repair_executing。
        // 反向获取会与 commands::get_auto_repair_queue 形成 AB-BA 死锁。
        let guard = state.auto_repair.lock().unwrap();
        let pending = guard
            .as_ref()
            .map(|ar| ar.queue.pending_count())
            .unwrap_or(0);
        if pending == 0 {
            return None;
        }
        let executing = state.auto_repair_executing.lock().unwrap();
        if *executing {
            return None;
        }
    }
    if auto_repair::is_codex_running(&[]) {
        return None;
    }
    let install_root = appserver::default_install_root()?;
    let exe = appserver::find_codex_exe(&install_root)?;
    {
        let mut executing = state.auto_repair_executing.lock().unwrap();
        if *executing {
            return None;
        }
        *executing = true;
    }
    let work_cwd = data_dir();
    let Some(home) = codex_home() else {
        *state.auto_repair_executing.lock().unwrap() = false;
        return None;
    };
    {
        let guard = state.auto_repair.lock().unwrap();
        let total = guard
            .as_ref()
            .map(|ar| ar.queue.pending_count())
            .unwrap_or(0);
        *state.auto_repair_progress.lock().unwrap() = Some(crate::RepairProgress {
            settled: 0,
            total,
            note: "开始逐项修复".to_string(),
        });
    }
    let result = {
        let mut guard = state.auto_repair.lock().unwrap();
        guard.as_mut().map(|ar| {
            ar.execute_pending(&exe, &work_cwd, &home, &mut |settled, total, tid, ok| {
                let short: String = tid.chars().take(8).collect();
                *state.auto_repair_progress.lock().unwrap() = Some(crate::RepairProgress {
                    settled,
                    total,
                    note: if ok {
                        format!("{short} 已修复")
                    } else {
                        format!("{short} 失败，保留在列表中可重试")
                    },
                });
                // 逐项进度事件：执行期间队列互斥锁被持有，面板不能直接读队列，
                // 用该事件驱动确定进度条与「完成一项消一项」（ok=false 保留）。
                let _ = app.emit(
                    "auto-repair-progress",
                    serde_json::json!({
                        "settled": settled,
                        "total": total,
                        "threadId": tid,
                        "ok": ok,
                    }),
                );
            })
        })
    };
    *state.auto_repair_executing.lock().unwrap() = false;
    *state.auto_repair_progress.lock().unwrap() = None;
    // 收尾提示：本轮结果 + 是否还有剩余（中途启动 Codex 会中止，留待下次）。
    if let Some((done, failed)) = result {
        let remaining = state
            .auto_repair
            .lock()
            .unwrap()
            .as_ref()
            .map(|ar| ar.queue.pending_count())
            .unwrap_or(0);
        let notice = if remaining > 0 {
            crate::RepairNotice::new(
                "本轮已停止 · 仍有待修复",
                format!("本轮完成 {done} 个 · 仍有 {remaining} 个（退出 Codex 后自动继续）"),
            )
        } else if failed > 0 {
            crate::RepairNotice::new(
                "自动修复完成（有失败项）",
                format!("成功 {done} 个 · 失败 {failed} 个（可在面板重试）"),
            )
        } else {
            crate::RepairNotice::new("自动修复完成", format!("成功修复 {done} 个任务"))
        };
        *state.auto_repair_notice.lock().unwrap() = Some(notice);
    }
    result
}

pub fn emit_queue(app: &AppHandle, state: &ShimState) {
    // 锁内只做序列化，emit（IPC 分发）在锁外；调用方也不得在持锁时调用本函数。
    let payload = {
        let guard = state.auto_repair.lock().unwrap();
        guard
            .as_ref()
            .and_then(|ar| serde_json::to_value(&ar.queue).ok())
            .unwrap_or(serde_json::Value::Null)
    };
    let _ = app.emit("auto-repair-changed", payload);
}
