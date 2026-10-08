//! 设置面板用的 Tauri 命令：读配置、保存并重启、查状态。

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use halcyon_core::config::{self, Config};
use halcyon_core::update::journal::UpdateJournal;

use crate::update_runtime::{UpdatePhase, UpdateStateView};
use crate::{restart_server, ShimState};

const HALCYON_REPOSITORY: &str = "weyham/halcyon";

#[tauri::command]
pub fn open_external_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("只允许打开 HTTP/HTTPS 链接".into());
    }
    if url.chars().any(char::is_control) {
        return Err("链接包含非法控制字符".into());
    }

    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", &url])
        .spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&url).spawn();

    result
        .map(|_| ())
        .map_err(|error| format!("打开浏览器失败：{error}"))
}

fn app_info_from_update(view: &UpdateStateView) -> Value {
    let (status, note) = match view.phase {
        UpdatePhase::Idle => ("未检查", "尚未检查GitHub Releases（公开仓）。"),
        UpdatePhase::Checking => ("检查中", "正在检查GitHub Releases（公开仓）。"),
        UpdatePhase::UpToDate => ("已是最新", "当前版本已是最新。"),
        UpdatePhase::UpdateAvailable => ("有更新", "发现已通过签名校验的可用更新。"),
        UpdatePhase::ManualDownload => ("需手动下载", "当前平台需要从 Release 页面手动下载。"),
        UpdatePhase::Downloading => ("下载中", "正在下载并校验更新。"),
        UpdatePhase::ReadyToInstall => ("待安装", "更新已下载并通过校验，等待安装。"),
        UpdatePhase::Installing => ("安装中", "更新正在安装。"),
        UpdatePhase::Completed => ("已完成", "更新已完成。"),
        UpdatePhase::Error => ("检查失败", "更新链路未完成或校验失败，已安全停止。"),
    };
    // 失败时必须能看到原因（F2）：把具体 message 拼进备注；原通用文案保留为前缀。
    let detail = if matches!(view.phase, UpdatePhase::Error) {
        view.error.as_ref().map(|error| {
            format!(
                "更新链路未完成或校验失败，已安全停止。原因：{}",
                error.message
            )
        })
    } else {
        None
    };
    json!({
        "version": view.current_version,
        "product": "Halcyon",
        "repository": HALCYON_REPOSITORY,
        "update_source": "GitHub Releases（公开仓，匿名读取）",
        "update_status": status,
        "update_note": detail
            .or_else(|| view.notes.clone())
            .unwrap_or_else(|| note.to_string()),
        "available_version": view.available_version,
        "release_url": view.release_url,
    })
}

/// 返回关于页需要的应用元数据。
#[tauri::command]
pub async fn get_app_info(app: AppHandle) -> Value {
    app_info_from_update(&app.state::<ShimState>().update.view().await)
}

/// 安全的更新检查占位：没有授权令牌或签名公钥时 fail closed，不访问私有 Release，也不执行下载。
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Value {
    let state = app.state::<ShimState>();
    // 安装版：Velopack 更新通道（E2）。portable/非 Velopack 走自研链路。
    if state.update.install_kind().is_installed() {
        let _ = state.update.check_velopack().await;
        return app_info_from_update(&state.update.view().await);
    }
    match state.update.check().await {
        Ok(view) => app_info_from_update(&view),
        Err(error) => {
            state.update.set_error(error);
            app_info_from_update(&state.update.view().await)
        }
    }
}

#[tauri::command]
pub async fn update_download(app: AppHandle) -> Result<Value, String> {
    // 安装版：Velopack download。portable 走自研 staging。
    {
        let state = app.state::<ShimState>();
        if state.update.install_kind().is_installed() {
            return match state.update.download_velopack().await {
                Ok(()) => Ok(app_info_from_update(&state.update.view().await)),
                Err(error) => Err(error.to_string()),
            };
        }
    }
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let app_dir = exe
        .parent()
        .ok_or_else(|| "无法定位 app 目录".to_string())?;
    let staging_root = app_dir.join("updates").join("staging");
    let state = app.state::<ShimState>();
    match state.update.download(&staging_root).await {
        Ok(view) => Ok(app_info_from_update(&view)),
        Err(error) => {
            state.update.set_error(error.clone());
            Err(error.to_string())
        }
    }
}

#[cfg(target_os = "windows")]
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<(), String> {
    let state = app.state::<ShimState>();
    // 安装版：Velopack apply + restart。portable 走自研 helper/journal。
    if state.update.install_kind().is_installed() {
        return state
            .update
            .apply_velopack()
            .await
            .map_err(|error| error.to_string());
    }
    let staged = state
        .update
        .staged()
        .ok_or_else(|| "没有已下载并验证的更新".to_string())?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let app_dir = exe
        .parent()
        .ok_or_else(|| "无法定位 app 目录".to_string())?
        .to_path_buf();
    let updates_dir = app_dir.join("updates");
    let rollback_dir = updates_dir.join("rollback").join(env!("CARGO_PKG_VERSION"));
    std::fs::create_dir_all(&rollback_dir).map_err(|error| error.to_string())?;
    let journal_path = updates_dir.join("halcyon-update.journal.json");
    let readiness_token = uuid::Uuid::new_v4().to_string();
    let journal = UpdateJournal::new(
        env!("CARGO_PKG_VERSION"),
        staged.version.clone(),
        &app_dir,
        &staged.staging_dir,
        &rollback_dir,
        &staged.helper_path,
        std::process::id(),
        &readiness_token,
    );
    journal
        .save(&journal_path)
        .map_err(|error| error.to_string())?;
    let launch = app_dir.join("halcyon.exe");
    let mut helper = std::process::Command::new(&staged.helper_path);
    helper
        .arg("--protocol")
        .arg("1")
        .arg("--journal")
        .arg(&journal_path)
        .arg("--target-app")
        .arg(&app_dir)
        .arg("--launch")
        .arg(&launch)
        .arg("--expected-version")
        .arg(&staged.version)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--readiness-token")
        .arg(&readiness_token)
        .current_dir(&app_dir);
    // updater 助手是控制台子系统进程，GUI 主进程拉起时必须抑制窗口，
    // 否则更新安装期间会弹出一个 CMD 窗口。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        helper.creation_flags(0x0800_0000);
    }
    helper.spawn().map_err(|error| error.to_string())?;
    state.update.mark_installing();
    app.exit(0);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
pub async fn update_install(_app: AppHandle) -> Result<(), String> {
    Err("当前平台只支持手动下载".to_string())
}

/// 当前生效的配置（含来源说明：exe 同目录 config.json / 旧 toml 导入 / 内置默认）。
#[tauri::command]
pub fn get_config() -> Value {
    let (raw, source) = config::load_default();
    json!({ "config": raw, "source": source })
}

/// 校验并保存配置到平台约定位置（Windows：exe 同目录；macOS：Application Support），
/// 然后重启代理服务。
#[tauri::command]
pub fn save_config(app: AppHandle, config: Value) -> Result<Value, String> {
    let raw: Config = serde_json::from_value(config).map_err(|e| format!("配置格式错误：{e}"))?;
    let resolved = config::ResolvedConfig::try_from(raw.clone()).map_err(|e| e.to_string())?;
    if resolved.routes.is_empty() {
        return Err("至少需要一条路由".to_string());
    }
    let path = config::default_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(&raw).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    log::info!("配置已保存到 {}，正在重启代理", path.display());
    restart_server(&app);
    Ok(json!({ "path": path.display().to_string() }))
}

/// 服务运行状态：是否在跑、监听地址、计数快照、配置来源。
#[tauri::command]
pub fn get_status(app: AppHandle) -> Value {
    let state = app.state::<ShimState>();
    let source = state.config_source.lock().unwrap().clone();
    let guard = state.server.lock().unwrap();
    match guard.as_ref() {
        Some(h) => json!({
            "running": true,
            "addr": h.addr().to_string(),
            "stats": h.stats().snapshot(),
            "config_source": source,
        }),
        None => json!({ "running": false, "config_source": source }),
    }
}

/// 各路由余额/额度查询结果（设置面板用量卡用）。
#[tauri::command]
pub fn get_balances(app: AppHandle) -> Value {
    let state = app.state::<ShimState>();
    let routes = state.routes.lock().unwrap().clone();
    let balances = state.balance_rows.lock().unwrap().clone();
    json!({
        "routes": routes.iter().map(|r| {
            let entry = balances.get(&r.name);
            json!({
                "name": r.name,
                "status": entry.map(|e| e.status.clone()),
                "raw": entry.and_then(|e| e.raw.clone()),
                "rows": entry.map(|e| e.rows.iter().map(|q| json!({
                    "label": q.label,
                    "pct": q.pct,
                    "reset": q.reset,
                    "reset_at_ms": q.reset_at_ms,
                    "detail": q.detail,
                    "text": q.menu_text(),
                })).collect::<Vec<_>>()),
            })
        }).collect::<Vec<_>>()
    })
}

/// 试跑一次用量查询（设置页"用量查询"对话框的测试按钮）：
/// 不保存配置，用内存 KeyRing 里该路由暂存的 Authorization 跑一次 取数→解析。
/// 对话框不提供粘贴 key 入口；无暂存凭据时返回"等待首次请求"。
#[tauri::command]
pub fn test_balance_query(
    app: AppHandle,
    name: String,
    upstream: String,
    balance: Option<config::BalanceCfg>,
) -> Result<Value, String> {
    let route = config::Route {
        name,
        upstream: upstream.trim_end_matches('/').to_string(),
        web_search: "note".to_string(),
        web_search_note_max: 20,
        aliases: Vec::new(),
        balance,
    };
    let query = halcyon_core::balance::resolve_query(&route)
        .ok_or_else(|| "未识别供应商，且未配置 custom 规则（或 custom 缺 url/规则）".to_string())?;
    if let Err(e) = halcyon_core::balance::validate_custom(&query.spec) {
        return Ok(json!({ "url": query.url, "status": format!("规则不合法：{e}") }));
    }
    let state = app.state::<ShimState>();
    let keyring = state.server.lock().unwrap().as_ref().map(|h| h.keyring());
    let auth = keyring.and_then(|k| k.lock().unwrap().get(&route.name).cloned());
    let Some(auth) = auth else {
        return Ok(
            json!({ "url": query.url, "status": "等待首次请求", "hint": "该路由还没有过流量，内存里暂存不到 Authorization；先用它发一次请求再来测试" }),
        );
    };
    match halcyon_core::balance::fetch_json(&query.url, &auth) {
        Err(e) => Ok(json!({ "url": query.url, "status": format!("查询失败：{e}") })),
        Ok(body) => {
            let raw = serde_json::to_string_pretty(&body).ok();
            match halcyon_core::balance::parse_custom(&query.spec, &body) {
                Ok(rows) => Ok(json!({
                    "url": query.url,
                    "status": "ok",
                    "rows": rows.iter().map(|q| json!({
                        "label": q.label,
                        "pct": q.pct,
                        "reset": q.reset,
                        "detail": q.detail,
                        "text": q.menu_text(),
                    })).collect::<Vec<_>>(),
                    "raw": raw,
                })),
                Err(e) => {
                    Ok(json!({ "url": query.url, "status": format!("解析失败：{e}"), "raw": raw }))
                }
            }
        }
    }
}

/// 手动触发一次余额/额度刷新。
#[tauri::command]
pub fn refresh_balances(app: AppHandle) {
    let state = app.state::<ShimState>();
    let tx = state.balance_refresh.lock().unwrap().clone();
    if let Some(tx) = tx {
        let _ = tx.send(());
    }
}

fn load_model_info() -> Result<halcyon_core::rewrite::ModelInfo, String> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .ok_or("找不到用户主目录")?;
    halcyon_core::rewrite::ModelInfo::load(&home.join(".codex"))
        .ok_or_else(|| "无法读取模型配置（config.toml 缺失、无 model 或解析失败）".to_string())
}

fn codex_sessions_root() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .ok_or("找不到用户主目录")?;
    let root = home.join(".codex").join("sessions");
    if !root.is_dir() {
        return Err(format!("sessions 目录不存在：{}", root.display()));
    }
    Ok(root)
}

/// 返回运行时数据目录信息（模式 / 路径 / 配置路径）。
#[tauri::command]
pub fn get_runtime_info() -> Value {
    let result = halcyon_core::data_dir::resolve(
        halcyon_core::data_dir::parse_cli_data_dir(&std::env::args().collect::<Vec<_>>())
            .as_deref(),
    );
    json!({
        "mode": if result.is_portable { "portable" } else { "installed" },
        "data_dir": result.path.to_string_lossy(),
        "config_path": result.path.join("config.json").to_string_lossy(),
    })
}

/// 模型切换自动修复：队列现状 + Codex 是否运行（决定按钮是「修复」还是「排队」）。
///
/// 异步命令：Tauri 的同步命令会在**主线程**执行，一旦阻塞会冻住整个 App
///（同步命令在主线程执行，阻塞会冻住整个 App）。所有可能阻塞的命令一律 async。
#[tauri::command]
pub async fn get_auto_repair_queue(state: tauri::State<'_, ShimState>) -> Result<Value, String> {
    // 锁内只读取（顺序：auto_repair → auto_repair_executing）；
    // 进程枚举在锁外，避免长时间占锁。
    let (queue, executing) = {
        let guard = state.auto_repair.lock().unwrap();
        let q = guard
            .as_ref()
            .and_then(|ar| serde_json::to_value(&ar.queue).ok())
            .unwrap_or(Value::Null);
        let e = *state.auto_repair_executing.lock().unwrap();
        (q, e)
    };
    // 队列文件只存身份（thread id）与修复类型；项目名/标题在入队时会被写死，
    // 之后用户改了项目归属或修了展示名解析，旧队列里永远是旧名字
    //（旧队列里固化的展示名曾显示成 cwd 兜底目录名 codex-threads-<id>）。
    // 所以读取时用展示索引实时覆盖。索引加载（SQLite + global-state）放阻塞线程池。
    let queue = tauri::async_runtime::spawn_blocking(move || {
        let mut queue = queue;
        if let Some(items) = queue.get_mut("items").and_then(Value::as_array_mut) {
            if let Some(home) = crate::auto_repair_loop::codex_home() {
                let display =
                    halcyon_core::model_repair::load_display_index(&home.join("sessions"));
                for item in items.iter_mut() {
                    let Some(tid) = item
                        .get("threadId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    if let Some(project) = display.projects.get(&tid) {
                        item["project"] = json!(project);
                    }
                    if let Some(title) = display.titles.get(&tid) {
                        item["title"] = json!(title);
                    }
                }
            }
        }
        queue
    })
    .await
    .map_err(|e| format!("读取展示索引失败：{e}"))?;
    let codex_running = halcyon_core::auto_repair::is_codex_running(&[]);
    Ok(json!({
        "queue": queue,
        "codex_running": codex_running,
        "executing": executing,
    }))
}

/// 执行前从队列取消一个任务。
#[tauri::command]
pub async fn cancel_auto_repair_item(app: AppHandle, thread_id: String) -> Result<Value, String> {
    let cancelled = {
        let state = app.state::<ShimState>();
        let mut guard = state.auto_repair.lock().unwrap();
        let Some(ar) = guard.as_mut() else {
            return Err("自动修复未初始化".into());
        };
        let cancelled = ar.cancel(&thread_id);
        if cancelled {
            ar.save();
        }
        cancelled
    };
    let state = app.state::<ShimState>();
    crate::auto_repair_loop::emit_queue(&app, &state);
    Ok(json!({ "cancelled": cancelled }))
}

/// 立即执行队列（Codex 运行中拒绝，由前端引导）。
#[tauri::command]
pub async fn execute_auto_repair_now(app: AppHandle) -> Result<Value, String> {
    if halcyon_core::auto_repair::is_codex_running(&[]) {
        return Err("Codex 正在运行：修复将保持排队，退出 Codex 后自动执行".into());
    }
    // 执行可能持续数十秒（逐项改写 + app-server 往返）：放到阻塞线程池，
    // 既不占用主线程也不占用 tokio 工作线程。
    let handle = app.clone();
    let result =
        tauri::async_runtime::spawn_blocking(move || crate::auto_repair_loop::try_execute(&handle))
            .await
            .map_err(|e| format!("执行失败：{e}"))?;
    match result {
        Some((done, failed)) => {
            let state = app.state::<ShimState>();
            crate::auto_repair_loop::emit_queue(&app, &state);
            Ok(json!({ "done": done, "failed": failed }))
        }
        None => Err("当前无可执行的修复（队列为空或正在执行中）".into()),
    }
}

/// 修复页统一扫描：input 为空扫全部任务，否则按深链/id 扫指定任务。
#[tauri::command]
pub async fn scan_unified(input: Option<String>) -> Result<Value, String> {
    // 全量扫描 30-40 秒：必须在阻塞线程池执行，绝不能在主线程。
    tauri::async_runtime::spawn_blocking(move || scan_unified_blocking(input))
        .await
        .map_err(|e| format!("扫描失败：{e}"))?
}

fn scan_unified_blocking(input: Option<String>) -> Result<Value, String> {
    let info = load_model_info()?;
    let sessions = codex_sessions_root()?;
    let home = crate::auto_repair_loop::codex_home().ok_or("找不到用户主目录")?;
    let tasks = match input.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(one) => halcyon_core::unified_scan::scan_one(&home, &sessions, &info, one)
            .into_iter()
            .collect::<Vec<_>>(),
        None => halcyon_core::unified_scan::scan_all(&home, &sessions, &info),
    };
    Ok(json!({
        "default_model": info.default_model,
        "tasks": tasks,
    }))
}

/// 修复页「修复所选」：重扫所选线程并入队（全类型）。
/// Codex 未运行时的立即执行由前端再调 execute_auto_repair_now。
#[tauri::command]
pub async fn enqueue_unified_repairs(
    app: AppHandle,
    thread_ids: Vec<String>,
) -> Result<Value, String> {
    let handle = app.clone();
    let touched =
        tauri::async_runtime::spawn_blocking(move || enqueue_unified_blocking(&handle, thread_ids))
            .await
            .map_err(|e| format!("入队失败：{e}"))??;
    let state = app.state::<ShimState>();
    crate::auto_repair_loop::emit_queue(&app, &state);
    Ok(json!({ "enqueued": touched }))
}

fn enqueue_unified_blocking(app: &AppHandle, thread_ids: Vec<String>) -> Result<usize, String> {
    let info = load_model_info()?;
    let sessions = codex_sessions_root()?;
    let home = crate::auto_repair_loop::codex_home().ok_or("找不到用户主目录")?;
    let all = halcyon_core::unified_scan::scan_all(&home, &sessions, &info);
    let selected: Vec<_> = all
        .into_iter()
        .filter(|t| thread_ids.contains(&t.thread_id))
        .collect();
    let state = app.state::<ShimState>();
    let touched = {
        let mut guard = state.auto_repair.lock().unwrap();
        let Some(ar) = guard.as_mut() else {
            return Err("自动修复未初始化".into());
        };
        let touched = ar.enqueue_unified(&info.default_model, &selected);
        ar.save();
        touched
    };
    Ok(touched)
}

/// 修复历史（「记录」页）：JSONL 全量读取，最新在前。
#[tauri::command]
pub async fn get_repair_history() -> Value {
    tauri::async_runtime::spawn_blocking(repair_history_blocking)
        .await
        .unwrap_or_else(|_| json!({ "records": [] }))
}

fn repair_history_blocking() -> Value {
    let mut records =
        halcyon_core::auto_repair::AutoRepair::load_history(&crate::auto_repair_loop::data_dir());
    records.reverse();
    json!({ "records": records })
}

/// 删除一条修复历史（index 为「最新在前」展示顺序的下标）。
#[tauri::command]
pub async fn delete_repair_history_record(index: usize) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        halcyon_core::auto_repair::AutoRepair::delete_history_at(
            &crate::auto_repair_loop::data_dir(),
            index,
        )?;
        Ok::<Value, String>(json!({ "deleted": true }))
    })
    .await
    .map_err(|e| format!("删除历史记录失败：{e}"))?
}

/// 清空全部修复历史。
#[tauri::command]
pub async fn clear_repair_history() -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let cleared = halcyon_core::auto_repair::AutoRepair::clear_history(
            &crate::auto_repair_loop::data_dir(),
        )?;
        Ok::<Value, String>(json!({ "cleared": cleared }))
    })
    .await
    .map_err(|e| format!("清空历史记录失败：{e}"))?
}
