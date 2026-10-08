pub use halcyon_core::rewrite;

pub mod auto_repair_loop;
pub mod autostart;
pub mod balance_poll;
pub mod commands;
pub mod tray;
pub mod update_check_loop;
mod update_runtime;

use std::collections::HashMap;
use std::sync::{mpsc::Sender, Arc, Mutex};

use halcyon_core::{
    auto_repair::AutoRepair,
    balance, config,
    logging::{LogHealth, TrackedRotatingWriter, LOG_MAX_SIZE},
    rewrite::ModelInfoCache,
    server::ServerHandle,
};
use tauri::{AppHandle, Emitter, Manager};

/// 自动修复的实时进度：托盘蓝点、托盘 tooltip、任务栏上方的进度卡片共用。
///
/// `total == 0` 表示"还没有可计数的总量"（扫描阶段），进度卡片显示为不确定进度。
#[derive(Debug, Clone, PartialEq)]
pub struct RepairProgress {
    pub settled: usize,
    pub total: usize,
    /// 副标题：最近一项的结果说明（含失败原因），空串表示不显示。
    pub note: String,
}

/// 自动修复的一次性提示（卡片只在"正在做"与"刚做完"两种情形出现）。
///
/// 为什么需要它：扫描完成不等于修复完成——Codex 正在运行时只能排队，
/// 此时卡片必须如实说"已排队，退出 Codex 后自动修复"，不能说"修复完成"。
#[derive(Debug, Clone, PartialEq)]
pub struct RepairNotice {
    pub title: String,
    pub note: String,
}

impl RepairNotice {
    pub fn new(title: impl Into<String>, note: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            note: note.into(),
        }
    }
}

impl RepairProgress {
    /// 扫描阶段（总量未知）。
    pub fn scanning(note: impl Into<String>) -> Self {
        Self {
            settled: 0,
            total: 0,
            note: note.into(),
        }
    }
}

/// 一条路由的余额/额度查询结果（轮询线程写入，托盘菜单与设置面板读取）。
#[derive(Debug, Clone)]
pub struct RouteBalance {
    pub rows: Vec<balance::QuotaRow>,
    /// "ok" / "等待首次请求" / 错误文本
    pub status: String,
    /// 原始响应（pretty JSON 文本），面板"原始数据"折叠区展示用
    pub raw: Option<String>,
    /// 这次数据是什么时候取回来的（Unix 毫秒）。托盘菜单用来标注「N 分钟前」；
    /// 只有真正查成功并解析出档位时才写，失败/等待 key 都留空。
    pub fetched_at_ms: Option<i64>,
}

/// 自动更新检查的产物：有可用更新时的徽标（红点）信息。
/// 后台循环写入；托盘菜单 / tooltip 与侧边栏红点读取。
#[derive(Debug, Clone)]
pub struct UpdateBadge {
    pub version: String,
    /// 已静默下载并校验就绪（可直接安装）。
    pub ready: bool,
    /// 该平台只支持手动下载（macOS）。
    pub manual_only: bool,
}

/// 应用全局状态。
pub struct ShimState {
    pub update: update_runtime::UpdateRuntime,
    pub server: Mutex<Option<ServerHandle>>,
    pub config_source: Mutex<String>,
    pub last_error: Mutex<String>,
    /// 当前生效的路由（供余额轮询）。
    pub routes: Mutex<Vec<config::Route>>,
    /// 路由名 → 余额/额度结果（轮询线程写入）。
    pub balance_rows: Mutex<HashMap<String, RouteBalance>>,
    /// 余额手动刷新信号（托盘左键触发）。
    pub balance_refresh: Mutex<Option<Sender<()>>>,
    /// 文件日志目标健康状态，随 `/health` 暴露。
    pub log_health: Arc<LogHealth>,
    /// config.toml 模型信息（指纹缓存，自动修复的触发器）。
    pub model_info: ModelInfoCache,
    /// 模型切换自动修复（队列持久化，后台 tick 驱动）。
    pub auto_repair: Mutex<Option<AutoRepair>>,
    /// 自动修复执行互斥旗标。
    pub auto_repair_executing: Mutex<bool>,
    /// 自动修复进度；None = 空闲。托盘蓝点/tooltip 与进度卡片用。
    pub auto_repair_progress: Mutex<Option<RepairProgress>>,
    /// 自动修复一次性提示（排队 / 完成 / 部分完成）；进度卡片取走后即清空。
    pub auto_repair_notice: Mutex<Option<RepairNotice>>,
    /// 自动更新检查的红点徽标；None = 没有可用更新。
    pub update_badge: Mutex<Option<UpdateBadge>>,
}

impl Default for ShimState {
    fn default() -> Self {
        Self {
            update: update_runtime::UpdateRuntime::new(
                env!("CARGO_PKG_VERSION"),
                // 运行时配置：缺省 false = 保持 1.0.6 的现状语义（过滤 pre-release）。
                config::load_default().0.update.include_prerelease,
            )
            .expect("初始化更新运行时失败"),
            server: Mutex::new(None),
            config_source: Mutex::new(String::new()),
            last_error: Mutex::new(String::new()),
            routes: Mutex::new(Vec::new()),
            balance_rows: Mutex::new(HashMap::new()),
            balance_refresh: Mutex::new(None),
            log_health: Arc::new(LogHealth::new()),
            model_info: ModelInfoCache::new(auto_repair_loop::codex_home().unwrap_or_default()),
            auto_repair: Mutex::new(None),
            auto_repair_executing: Mutex::new(false),
            auto_repair_progress: Mutex::new(None),
            auto_repair_notice: Mutex::new(None),
            update_badge: Mutex::new(None),
        }
    }
}

/// 加载配置并启动代理服务；结果写入 ShimState（托盘红绿灯由 tray::refresh_loop 持续反映）。
pub fn start_server(app: &AppHandle) {
    start_server_inner(app);
    // 启动/重启后连接信息（监听地址 / 未运行）会变，同步刷新右键菜单。
    tray::refresh_actions_menu(app);
}

fn start_server_inner(app: &AppHandle) {
    let state = app.state::<ShimState>();
    let (raw, source) = config::load_default();
    *state.config_source.lock().unwrap() = source.clone();
    let resolved = match config::ResolvedConfig::try_from(raw) {
        Ok(r) => r,
        Err(e) => {
            *state.last_error.lock().unwrap() = "配置错误".to_string();
            log::error!("配置错误（来源：{source}）：{e}");
            return;
        }
    };
    if resolved.routes.is_empty() {
        *state.last_error.lock().unwrap() = "无路由配置".to_string();
        log::warn!("配置中没有路由（来源：{source}），代理未启动");
        return;
    }
    // 安装版更新通道的 pre-release 开关随后端配置生效（保存配置 → 重启代理）。
    state
        .update
        .set_include_prerelease(resolved.include_prerelease);
    *state.routes.lock().unwrap() = resolved.routes.clone();
    let listen = resolved.listen.clone();
    match halcyon_core::server::start_with_log_health(resolved, Some(state.log_health.clone())) {
        Ok(handle) => {
            let addr = handle.addr();
            *state.server.lock().unwrap() = Some(handle);
            *state.last_error.lock().unwrap() = String::new();
            log::info!("代理已启动：http://{addr}（监听 {listen}，配置来源：{source}）");
        }
        Err(e) => {
            *state.last_error.lock().unwrap() = "监听失败（端口被占用？）".to_string();
            log::error!("监听 {listen} 失败：{e}（可能已有一个代理在运行）");
        }
    }
}

fn cleanup_completed_update() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(app_dir) = exe.parent().map(std::path::Path::to_path_buf) else {
        return;
    };
    // The helper may still be running inside staging when the new app starts.
    std::thread::spawn(move || {
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            match halcyon_core::update::install::cleanup_completed_update_once(
                &app_dir,
                env!("CARGO_PKG_VERSION"),
            ) {
                Ok(true) => return,
                Ok(false) => continue,
                Err(error) => log::warn!("更新临时文件清理等待重试：{error}"),
            }
        }
        log::warn!("已完成更新的临时文件未能在本次启动中清理，下一次启动会重试");
    });
}

fn write_readiness_if_requested(app: &AppHandle) {
    let Some(token) = std::env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .windows(2)
        .find(|pair| pair[0] == "--readiness-token")
        .map(|pair| pair[1].clone())
    else {
        return;
    };
    let initialized = app.state::<ShimState>().server.lock().unwrap().is_some();
    if !initialized {
        log::error!("更新 readiness 未写入：代理服务未成功启动");
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        log::error!("更新 readiness 未写入：无法定位当前 exe");
        return;
    };
    let Some(app_dir) = exe.parent() else {
        log::error!("更新 readiness 未写入：无法定位 app 目录");
        return;
    };
    match halcyon_core::update::install::write_readiness_after_startup(
        app_dir,
        &token,
        env!("CARGO_PKG_VERSION"),
        initialized,
        false,
        true,
    ) {
        Ok(path) => log::info!("更新 readiness 已写入：{}", path.display()),
        Err(error) => log::error!("更新 readiness 写入失败：{error}"),
    }
}

/// 便携版日志目录：exe 旁 `data\logs`（与 config.json / 队列 / 修复历史同一处）。
/// 非便携（安装版 / mac 固定目录）返回 None，由调用方回落到系统 app_log_dir。
fn portable_log_dir() -> Option<std::path::PathBuf> {
    let resolved = halcyon_core::data_dir::resolve(
        halcyon_core::data_dir::parse_cli_data_dir(&std::env::args().collect::<Vec<_>>())
            .as_deref(),
    );
    if !resolved.is_portable {
        return None;
    }
    Some(resolved.path.join("logs"))
}

/// 托盘菜单"重启代理"：停掉旧服务、重新读配置、再启动，并立刻刷新一次余额。
pub fn restart_server(app: &AppHandle) {
    let state = app.state::<ShimState>();
    if let Some(h) = state.server.lock().unwrap().take() {
        h.shutdown();
    }
    start_server(app);
    let tx = state.balance_refresh.lock().unwrap().clone();
    if let Some(tx) = tx {
        let _ = tx.send(());
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Velopack lifecycle hook: handles --veloapp-* args (install/uninstall/update etc).
    // In non-Velopack environments (portable app\, dev directory), this is a complete no-op.
    velopack::VelopackApp::build().run();

    // HALCYON_DEV=1 时跳过单实例插件：开发实例可与生产实例并存（不同端口、互不影响）
    let mut builder = tauri::Builder::default().plugin(tauri_plugin_autostart::init(
        tauri_plugin_autostart::MacosLauncher::LaunchAgent,
        None,
    ));
    builder = builder.plugin(tauri_plugin_notification::init());
    if std::env::var_os("HALCYON_DEV").is_none() {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 二次启动：聚焦已有实例的设置窗口
            tray::show_settings_window(app);
        }));
    }
    builder
        .setup(|app| {
            // release 也写日志文件：开机启动这类问题只有在真实重启之后才能定位，
            // 没有日志就只剩「它没起来」这一个信息。这里不用 plugin 内建 LogDir
            // 轮转（Windows 下会带句柄删除并静默失败），改为可观测 writer。
            let log_health = Arc::new(LogHealth::new());
            // 便携版的日志必须跟随便携数据目录（exe\data\logs），不能用 app_log_dir()：
            // 后者取的是"启动者的用户环境"，从 Codex 内部（MSIX 容器）启动时会被重定向到
            // 容器视图，于是同一台机器上出现两份互不可见的 halcyon.log，
            // 排障时表现为"日志忽然停了"（2026-10-07 实际踩到）。安装版沿用系统目录。
            let log_dir = portable_log_dir().unwrap_or(app.path().app_log_dir()?);
            std::fs::create_dir_all(&log_dir)?;
            let log_writer = TrackedRotatingWriter::new(
                log_dir.join("halcyon.log"),
                log_dir.join("halcyon.log.fallback"),
                log_health.clone(),
                LOG_MAX_SIZE,
            );
            let file_dispatch = tauri_plugin_log::fern::Dispatch::new().chain(
                tauri_plugin_log::fern::Output::writer(Box::new(log_writer), "\n"),
            );
            app.handle().plugin(
                tauri_plugin_log::Builder::default()
                    .level(log::LevelFilter::Info)
                    // 默认是 UTC，日志时间会比本地早 8 小时，排障时容易看错。
                    .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
                    .targets([
                        tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                        tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Dispatch(
                            file_dispatch,
                        )),
                    ])
                    .build(),
            )?;
            if std::env::args().any(|arg| arg == "--autostart") {
                log::info!("本次由开机启动拉起（--autostart）");
            }
            app.manage(ShimState {
                log_health,
                ..ShimState::default()
            });
            // 必须早于 setup_tray：托盘那个「开机启动」勾选项读的就是这里的结果。
            match autostart::reconcile(app.handle()) {
                Ok(autostart::State::Enabled) => {
                    log::info!("开机启动：本份已启用，启动文件夹快捷方式已就绪")
                }
                Ok(autostart::State::HeldByOther(path)) => {
                    log::info!("开机启动：启动文件夹快捷方式由另一份安装持有（{path}），未改动")
                }
                Ok(autostart::State::Disabled) => log::info!("开机启动：未启用"),
                Err(error) => log::error!("开机启动自检失败：{error}"),
            }
            tray::setup_tray(app)?;
            tray::setup_progress_window(app.handle())?;
            start_server(app.handle());
            write_readiness_if_requested(app.handle());
            cleanup_completed_update();
            // 余额轮询线程 + 手动刷新通道
            let tx = balance_poll::spawn(app.handle().clone());
            *app.state::<ShimState>().balance_refresh.lock().unwrap() = Some(tx);
            // 模型切换自动修复：加载持久化队列 + 启动后台 tick
            *app.state::<ShimState>().auto_repair.lock().unwrap() =
                Some(auto_repair_loop::load_auto_repair());
            auto_repair_loop::spawn(app.handle().clone());
            // 定时自动更新检查（启动后 60s 首次，之后每 3 小时）
            update_check_loop::spawn(app.handle().clone());
            // 托盘刷新循环
            let handle = app.handle().clone();
            std::thread::spawn(move || tray::refresh_loop(handle));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::get_status,
            commands::get_app_info,
            commands::open_external_url,
            commands::check_update,
            commands::update_download,
            commands::update_install,
            commands::get_balances,
            commands::refresh_balances,
            commands::test_balance_query,
            commands::get_runtime_info,
            commands::get_auto_repair_queue,
            commands::cancel_auto_repair_item,
            commands::execute_auto_repair_now,
            commands::scan_unified,
            commands::enqueue_unified_repairs,
            commands::get_repair_history,
            commands::delete_repair_history_record,
            commands::clear_repair_history,
        ])
        .on_window_event(|window, event| {
            // 托盘应用惯例：关闭主窗口 = 隐藏到托盘，而不是退出
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
                // 扫描结果一次性化：通知前端清空修复页扫描状态
                let _ = window.emit("panel-hidden", ());
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
