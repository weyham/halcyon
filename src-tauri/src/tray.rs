use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tauri::{
    image::Image,
    menu::{CheckMenuItemBuilder, ContextMenu, MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, Wry,
};

const ICON_GREEN: &[u8] = include_bytes!("../icons/status-green.png");
const ICON_GREEN_DIM: &[u8] = include_bytes!("../icons/status-green-dim.png");
const ICON_YELLOW: &[u8] = include_bytes!("../icons/status-yellow.png");
const ICON_RED: &[u8] = include_bytes!("../icons/status-red.png");
const ICON_BLUE: &[u8] = include_bytes!("../icons/status-blue.png");

/// 待弹出的统计菜单代际计数：每次左键抬起 +1 并启动延迟任务；
/// 双击到达时 +1 使未兑现的任务失效（见 on_tray_icon_event）。
static PENDING_STATS_MENU: AtomicU32 = AtomicU32::new(0);

/// 最近一次左键双击的时刻：Windows 双击序列是「抬起→按下→双击事件→抬起」，
/// 第二次抬起会紧随双击事件到达，必须被抑制，否则统计菜单会再次弹出。
static LAST_DOUBLE_CLICK: Mutex<Option<std::time::Instant>> = Mutex::new(None);

const DOUBLE_CLICK_SUPPRESS: Duration = Duration::from_millis(1000);

/// 任务栏上方的自动修复进度卡片（独立无边框窗口）。
/// 用户反馈：托盘 tooltip 里的 x/N 太不显眼，需要一眼能看到进度条。
const PROGRESS_WINDOW_LABEL: &str = "repair-progress";
const PROGRESS_WINDOW_WIDTH: f64 = 360.0;
const PROGRESS_WINDOW_HEIGHT: f64 = 86.0;
/// 结束态（"自动修复完成"）在屏幕上停留多久再隐藏。
const PROGRESS_LINGER: Duration = Duration::from_secs(5);

/// 上一次的进度值：None 表示卡片当前是隐藏态。
static LAST_PROGRESS: Mutex<Option<crate::RepairProgress>> = Mutex::new(None);
/// 隐藏任务代际计数：新一轮修复开始时让"到点隐藏"的旧任务失效。
static PROGRESS_GEN: AtomicU32 = AtomicU32::new(0);

#[cfg(windows)]
fn schedule_stats_menu(app: AppHandle) {
    if LAST_DOUBLE_CLICK
        .lock()
        .unwrap()
        .is_some_and(|t| t.elapsed() < DOUBLE_CLICK_SUPPRESS)
    {
        return;
    }
    let gen = PENDING_STATS_MENU.fetch_add(1, Ordering::SeqCst) + 1;
    // Windows 默认双击间隔 500ms（GetDoubleClickTime 在 windows crate 0.62 未导出，
    // 用系统默认值 + 余量即可覆盖绝大多数用户设置）。
    let wait = 530;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(wait));
        if PENDING_STATS_MENU.load(Ordering::SeqCst) == gen {
            show_stats_menu(&app);
        }
    });
}

#[cfg(not(windows))]
fn schedule_stats_menu(app: AppHandle) {
    show_stats_menu(&app);
}

#[cfg(windows)]
fn cancel_pending_stats_menu() {
    PENDING_STATS_MENU.fetch_add(1, Ordering::SeqCst);
}

/// 代理运行状态，对应托盘红绿灯。
///
/// 语义：
/// - 红：服务没起来（监听失败 / 配置错误 / 未启动）
/// - 黄：服务在跑但无流量——启动以来零流量，或持续 60 秒闲置
/// - 绿：最近 60 秒内有流量
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShimStatus {
    Running,
    Warning,
    Down,
    /// 自动修复执行中（蓝点 + tooltip 进度）。
    Working,
}

/// 当前展示的图标状态（避免每帧重复 set_icon）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct IconState {
    status: ShimStatus,
    active: bool,
}

static LAST_ICON: Mutex<Option<IconState>> = Mutex::new(None);
/// "开机启动"勾选项的句柄（随每次菜单重建指向新项）。
static AUTOSTART_ITEM: Mutex<Option<tauri::menu::CheckMenuItem<Wry>>> = Mutex::new(None);

/// 托盘用量区每路由最多显示的行数（内置模板上限：5h/7d/30d 三档）。
const MAX_USAGE_ROWS: usize = 3;

/// 主线程（GUI/事件循环）的 Win32 线程 id，setup 时捕获；菜单打开检测用。
#[cfg(target_os = "windows")]
static MAIN_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// 托盘菜单当前是否打开：枚举主线程窗口，找可见的 "#32768"（Windows 菜单窗口类）。
/// 打开期间禁止重建菜单和切换图标——两者都会把已弹出的菜单销毁（实测闪关）。
#[cfg(target_os = "windows")]
fn menu_window_open() -> bool {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, GetClassNameW, IsWindowVisible,
    };

    unsafe extern "system" fn find_menu(hwnd: HWND, lp: LPARAM) -> BOOL {
        let mut class = [0u16; 16];
        let len = unsafe { GetClassNameW(hwnd, &mut class) };
        let expected = [
            '#' as u16, '3' as u16, '2' as u16, '7' as u16, '6' as u16, '8' as u16,
        ];
        let is_menu = len == 6 && class[..6] == expected;
        if is_menu && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            unsafe { *(lp.0 as *mut bool) = true };
            return BOOL(0);
        }
        BOOL(1)
    }

    let tid = MAIN_THREAD_ID.load(Ordering::Relaxed);
    if tid == 0 {
        return false;
    }
    let mut found = false;
    unsafe {
        let _ = EnumThreadWindows(
            tid,
            Some(find_menu),
            LPARAM(&mut found as *mut bool as isize),
        );
    }
    found
}

#[cfg(not(target_os = "windows"))]
fn menu_window_open() -> bool {
    false
}

/// 安装托盘图标与两套菜单。
///
/// Windows 的托盘回调只会稳定送来**抬起**消息（实测：真实点击不投递按下），
/// 所以「按下时换菜单、抬起时弹出」这条路走不通。改为：
/// - 右键：挂一份常驻菜单（连接信息 + 操作项），由系统弹出；
/// - 左键：关掉系统的左键弹菜单，收到抬起后由我们主动 popup 一份统计菜单。
pub fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    #[cfg(target_os = "windows")]
    MAIN_THREAD_ID.store(
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
        Ordering::Relaxed,
    );
    let menu = build_actions_menu(app.handle())?;

    TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(ICON_YELLOW)?)
        .tooltip("Halcyon · 启动中")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            match event {
                // 左键单击 = 统计弹层；左键双击 = 打开功能面板。
                // 统计弹层是同步模态菜单，会吞掉双击的第二次点击，
                // 因此单击动作延迟一个系统双击间隔，双击到达即取消。
                // （tray-icon 的 DoubleClick 事件仅 Windows 提供；macOS 无双击行为。）
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => schedule_stats_menu(tray.app_handle().clone()),
                #[cfg(windows)]
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => {
                    *LAST_DOUBLE_CLICK.lock().unwrap() = Some(std::time::Instant::now());
                    cancel_pending_stats_menu();
                    show_settings_window(tray.app_handle());
                }
                _ => {}
            }
        })
        .on_menu_event(|app: &AppHandle, event| match event.id().as_ref() {
            "panel" => show_settings_window(app),
            // 更新红点条目：打开面板并让前端跳到「关于」页直接看到安装按钮
            "update-available" => {
                show_settings_window(app);
                let _ = app.emit("navigate-about", ());
            }
            "restart" => crate::restart_server(app),
            "autostart" => toggle_autostart(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

/// 左键：在光标处主动弹出统计菜单（统计栏 + 用量区，快照）。
///
/// 宿主用隐藏的主窗口——和 tray-icon 自己拿隐藏窗口当宿主同一套路。
/// muda 的 popup 是同步 TrackPopupMenu，菜单对象在这次调用期间一直存活。
fn show_stats_menu(app: &AppHandle) {
    let Some(webview_window) = app.get_webview_window("main") else {
        log::error!("找不到主窗口，无法弹出统计菜单");
        return;
    };
    let window = AsRef::<tauri::Webview<Wry>>::as_ref(&webview_window).window();
    match build_stats_menu(app) {
        Ok(menu) => {
            if let Err(error) = menu.popup(window) {
                log::error!("统计菜单弹出失败：{error}");
            }
        }
        Err(error) => log::error!("统计菜单构建失败：{error}"),
    }
}

/// 左键菜单：统计栏 + 分隔 + 用量区。展示型行一律禁用。
fn build_stats_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<Wry>> {
    let state = app.state::<crate::ShimState>();
    let stats = stats_text(&state);
    let stats_item = MenuItemBuilder::with_id("stats", stats)
        .enabled(false)
        .build(app)?;
    let mut builder = MenuBuilder::new(app).item(&stats_item).separator();
    for (index, line) in usage_lines(&state).iter().enumerate() {
        let item = MenuItemBuilder::with_id(format!("usage-{index}"), line)
            .enabled(false)
            .build(app)?;
        builder = builder.item(&item);
    }
    builder.build()
}

/// 右键菜单：版本信息 + 可点击操作项。挂到托盘由系统弹出。
fn build_actions_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<Wry>> {
    // 顶行只放身份信息：监听地址与运行状态在左键菜单（以及托盘图标颜色）里已经能看到。
    let version = app.package_info().version.to_string();
    let info_item = MenuItemBuilder::with_id("info", format!("Halcyon · {version}"))
        .enabled(false)
        .build(app)?;
    // 归属判定：链接被另一份安装持有时不算「本份已启用」，菜单里明说。
    let autostart_state = crate::autostart::state(app).unwrap_or_else(|error| {
        log::warn!("开机启动状态读取失败：{error}");
        crate::autostart::State::Disabled
    });
    let autostart_on = autostart_state.is_on();
    let panel = MenuItemBuilder::with_id("panel", "打开功能面板").build(app)?;
    let autostart =
        CheckMenuItemBuilder::with_id("autostart", crate::autostart::label(&autostart_state))
            .checked(autostart_on)
            .build(app)?;
    *AUTOSTART_ITEM.lock().unwrap() = Some(autostart.clone());
    let restart = MenuItemBuilder::with_id("restart", "重启代理").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出").build(app)?;
    MenuBuilder::new(app)
        .item(&info_item)
        .separator()
        .item(&panel)
        .item(&autostart)
        .item(&restart)
        .separator()
        .item(&quit)
        .build()
}

/// 重建右键菜单：顶行是版本号属于静态内容，但开机启动勾选状态可能被外部改动，
/// 服务启动/重启后顺带同步一次。菜单已打开时跳过——set_menu 会把正在弹出的菜单销毁。
pub fn refresh_actions_menu(app: &AppHandle) {
    if menu_window_open() {
        return;
    }
    match build_actions_menu(app) {
        Ok(menu) => {
            if let Some(tray) = app.tray_by_id("main") {
                if let Err(error) = tray.set_menu(Some(menu)) {
                    log::error!("托盘右键菜单刷新失败：{error}");
                }
            }
        }
        Err(error) => log::error!("托盘右键菜单构建失败：{error}"),
    }
}

/// 左键菜单顶行的统计（点击那一刻的值）。
fn stats_text(state: &crate::ShimState) -> String {
    let server = state.server.lock().unwrap();
    match server.as_ref() {
        Some(handle) => {
            let snap = handle.stats().snapshot();
            let requests = snap["requests"].as_u64().unwrap_or(0);
            let rewritten = snap["rewritten_requests"].as_u64().unwrap_or(0);
            let errors = snap["upstream_errors"].as_u64().unwrap_or(0);
            format!("请求 {requests} · 改写 {rewritten} · 上游错误 {errors}")
        }
        None => "请求 - · 改写 - · 上游错误 -".to_string(),
    }
}

/// 用量区文本行：只含「本次启动以来有流量的路由」（Stats.route_last_request_ms）。
/// 每路由一个标题行 + 至多 MAX_USAGE_ROWS 个数据行；行数由解析结果驱动，
/// 无月度档则没有该行。余额型显示 `余额: ¥12.34`。
/// 标题行带上数据年龄（`用量快照 · ds · 3 分钟前`），免得把几分钟前的快照当现值看。
fn usage_lines(state: &crate::ShimState) -> Vec<String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let activity: std::collections::HashSet<String> = state
        .server
        .lock()
        .unwrap()
        .as_ref()
        .map(|handle| {
            handle.stats().snapshot()["route_last_request_ms"]
                .as_object()
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let balances = state.balance_rows.lock().unwrap();
    let routes = state.routes.lock().unwrap();
    let mut lines = Vec::new();
    for route in routes.iter() {
        if !activity.contains(&route.name) {
            continue;
        }
        let entry = balances.get(&route.name);
        let age = entry
            .and_then(|e| e.fetched_at_ms)
            .map(|fetched_at| halcyon_core::balance::fetched_age_text(now_ms, fetched_at));
        lines.push(match age {
            Some(age) => format!("用量快照 · {} · {age}", route.name),
            None => format!("用量快照 · {}", route.name),
        });
        match entry {
            Some(entry) if entry.status == "ok" && !entry.rows.is_empty() => {
                for row in entry.rows.iter().take(MAX_USAGE_ROWS) {
                    lines.push(halcyon_core::balance::tray_row_text(row, now_ms));
                }
            }
            Some(entry) => {
                let status = if entry.status == "等待首次请求" {
                    "用量查询中…".to_string()
                } else {
                    entry.status.clone()
                };
                lines.push(status);
            }
            None => lines.push("用量查询中…".to_string()),
        }
    }
    if lines.is_empty() {
        lines.push("用量快照：暂无流量经过".to_string());
    }
    lines
}

/// 创建进度卡片窗口（启动时创建、常驻隐藏；只在自动修复期间显示）。
///
/// 为什么不用托盘 tooltip / 通知：tooltip 太不显眼，通知无法承载"实时进度"。
/// 卡片是独立无边框、不抢焦点、不进任务栏的小窗口，位置贴着任务栏上方右侧。
pub fn setup_progress_window(app: &AppHandle) -> tauri::Result<()> {
    let builder = tauri::WebviewWindowBuilder::new(
        app,
        PROGRESS_WINDOW_LABEL,
        tauri::WebviewUrl::App("progress.html".into()),
    )
    .title("Halcyon 自动修复")
    .inner_size(PROGRESS_WINDOW_WIDTH, PROGRESS_WINDOW_HEIGHT)
    .decorations(false)
    .resizable(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .shadow(false)
    .focused(false)
    .visible(false);
    // 透明底 + HTML 圆角：Windows 支持；macOS 需要 macos-private-api，保持不透明。
    #[cfg(windows)]
    let builder = builder.transparent(true);
    builder.build()?;
    Ok(())
}

/// 把进度卡片贴到主显示器工作区（= 任务栏之上）的右下角。
fn place_progress_window(win: &tauri::WebviewWindow) {
    let Ok(Some(monitor)) = win.primary_monitor() else {
        return;
    };
    let area = monitor.work_area();
    let scale = monitor.scale_factor();
    // work_area 是物理像素；窗口尺寸用逻辑像素传给 set_position 前换算成物理像素，
    // 否则高 DPI 屏幕上卡片会偏离右边缘。
    let w = (PROGRESS_WINDOW_WIDTH * scale).round() as i32;
    let h = (PROGRESS_WINDOW_HEIGHT * scale).round() as i32;
    let margin = (14.0 * scale).round() as i32;
    let x = area.position.x + area.size.width as i32 - w - margin;
    let y = area.position.y + area.size.height as i32 - h - margin;
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
}

/// 每 500ms 由 refresh_loop 调用：按当前进度 / 一次性提示显示、更新、隐藏卡片。
///
/// 三种输入：
/// - `progress = Some`：扫描 / 执行中，持续刷新；
/// - `notice = Some`：一次性结论（已排队 / 完成 / 部分完成），显示数秒后隐藏；
/// - 两者都 None：静默隐藏——**不声称"修复完成"**（扫描结束不等于修复结束，
///   Codex 正在运行时只能排队）。
fn update_progress_card(
    app: &AppHandle,
    progress: Option<&crate::RepairProgress>,
    notice: Option<&crate::RepairNotice>,
) {
    let Some(win) = app.get_webview_window(PROGRESS_WINDOW_LABEL) else {
        return;
    };
    let mut last = LAST_PROGRESS.lock().unwrap();
    if progress.is_none() {
        if let Some(n) = notice {
            // 一次性提示：显示 note + 标题，然后按 linger 时间隐藏。
            if last.is_none() {
                place_progress_window(&win);
                let _ = win.show();
            }
            *last = Some(crate::RepairProgress {
                settled: 0,
                total: 0,
                note: n.note.clone(),
            });
            let gen = PROGRESS_GEN.fetch_add(1, Ordering::SeqCst) + 1;
            let title = serde_json::to_string(&n.title).unwrap_or_else(|_| "\"\"".to_string());
            let note = serde_json::to_string(&n.note).unwrap_or_else(|_| "\"\"".to_string());
            let _ = win.eval(format!(
                "window.__halcyon_notice && window.__halcyon_notice({title}, {note})"
            ));
            drop(last);
            let app = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(PROGRESS_LINGER);
                if PROGRESS_GEN.load(Ordering::SeqCst) != gen {
                    return;
                }
                if let Some(win) = app.get_webview_window(PROGRESS_WINDOW_LABEL) {
                    let _ = win.hide();
                }
            });
            return;
        }
        // 既没有进度也没有提示：直接隐藏，不做任何"完成"声明。
        if last.take().is_some() {
            PROGRESS_GEN.fetch_add(1, Ordering::SeqCst);
            let _ = win.hide();
        }
        return;
    }
    if let Some(p) = progress {
        // 每次变化都重新定位：多显示器/分辨率变化后仍贴住任务栏。
        if last.is_none() {
            place_progress_window(&win);
            let _ = win.show();
        }
        let changed = last.as_ref() != Some(p);
        *last = Some(p.clone());
        // 新一轮进度使"到点隐藏"的旧任务失效。
        PROGRESS_GEN.fetch_add(1, Ordering::SeqCst);
        if changed {
            let note = serde_json::to_string(&p.note).unwrap_or_else(|_| "\"\"".to_string());
            let _ = win.eval(format!(
                "window.__halcyon_progress && window.__halcyon_progress({}, {}, {note})",
                p.settled, p.total
            ));
        }
    }
}

/// 状态刷新循环（每 500ms）：红绿灯 + token 流活动闪烁。
/// 由应用启动后 spawn，随进程结束。从 ShimState 读当前服务句柄，重启代理后自动跟随新句柄。
/// 菜单文本不在此更新——菜单是点击那一刻的快照（见 build_menu）。
pub fn refresh_loop(app: AppHandle) {
    let mut tick = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        tick += 1;
        let state = app.state::<crate::ShimState>();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let server = state
            .server
            .lock()
            .unwrap()
            .as_ref()
            .map(|h| (h.addr().to_string(), h.stats()));
        let progress = state.auto_repair_progress.lock().unwrap().clone();
        let (status, tip, last_activity_ms) = if let Some(p) = progress.as_ref() {
            // 自动修复进行中：蓝点 + 进度（优先级高于流量状态）。
            // total = 0 是扫描阶段（总量未知），此时不显示 x/N 以免误导。
            let tip = if p.total == 0 {
                "Halcyon · 正在扫描待修复任务".to_string()
            } else {
                format!("Halcyon · 自动修复中 {}/{}", p.settled, p.total)
            };
            (ShimStatus::Working, tip, 0)
        } else {
            match &server {
                Some((addr, stats)) => {
                    let snap = stats.snapshot();
                    let requests = snap["requests"].as_u64().unwrap_or(0);
                    let last_act = snap["last_activity_ms"].as_u64().unwrap_or(0);
                    // 绿 = 最近 60s 内有流量活动；黄 = 无流量（从未有过，或闲置超过 60s）
                    let (st, tip) = if last_act == 0 {
                        (
                        ShimStatus::Warning,
                        format!("Halcyon · 运行中 http://{addr} · 暂无流量经过（卡片未接或 App 未重启）"),
                    )
                    } else if now_ms.saturating_sub(last_act) > 60_000 {
                        let idle = now_ms.saturating_sub(last_act) / 60_000;
                        (
                            ShimStatus::Warning,
                            format!("Halcyon · 运行中 http://{addr} · 已闲置 {idle} 分钟"),
                        )
                    } else {
                        (
                            ShimStatus::Running,
                            format!("Halcyon · 运行中 http://{addr} · 请求 {requests}"),
                        )
                    };
                    (st, tip, last_act)
                }
                None => {
                    let err = state.last_error.lock().unwrap().clone();
                    let tip = if err.is_empty() {
                        "Halcyon · 未运行".to_string()
                    } else {
                        format!("Halcyon · {err}")
                    };
                    (ShimStatus::Down, tip, 0)
                }
            }
        };

        // 活动闪烁：最近 1.5s 内有 token 流过（pump 每块记录）→ 绿灯芯/亮环交替
        let recently_active =
            last_activity_ms > 0 && now_ms.saturating_sub(last_activity_ms) < 1500;
        let active = recently_active && tick % 2 == 0;

        let want = IconState { status, active };
        let changed = LAST_ICON
            .lock()
            .unwrap()
            .as_ref()
            .map_or(true, |last| *last != want);
        let menu_open = menu_window_open();
        if changed && !menu_open {
            let bytes = match (status, active) {
                // 流量闪烁：亮点（稳态绿）↔ 暗点交替，无外圈
                (ShimStatus::Running, true) => ICON_GREEN_DIM,
                (ShimStatus::Running, false) => ICON_GREEN,
                (ShimStatus::Warning, _) => ICON_YELLOW,
                (ShimStatus::Down, _) => ICON_RED,
                (ShimStatus::Working, _) => ICON_BLUE,
            };
            if let Some(tray) = app.tray_by_id("main") {
                if let Ok(img) = Image::from_bytes(bytes) {
                    let _ = tray.set_icon(Some(img));
                }
                // 更新红点：tooltip 末尾顺带提示（不重写状态文案）
                let tip = state
                    .update_badge
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|badge| format!("{tip} · 有更新 {}", badge.version))
                    .unwrap_or(tip);
                let _ = tray.set_tooltip(Some(&tip));
            }
            *LAST_ICON.lock().unwrap() = Some(want);
        }

        // 任务栏上方的进度卡片（独立于托盘图标，菜单打开也照常更新）。
        // 提示是一次性的：这里取走，交给卡片显示。
        let notice = state.auto_repair_notice.lock().unwrap().take();
        update_progress_card(&app, progress.as_ref(), notice.as_ref());
    }
}

/// 显示并聚焦设置窗口（主窗口）。
pub fn show_settings_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 切换开机启动，并同步勾选状态与文案。
///
/// 「被另一份安装持有」时点一下 = 接管（本份开启）；已经是我们时点一下 = 关闭；
/// 关闭时若链接不是本份的，core 的判定返回「什么都不做」——不替别人关。
fn toggle_autostart(app: &AppHandle) {
    let before = crate::autostart::state(app).unwrap_or(crate::autostart::State::Disabled);
    let turning_on = !before.is_on();
    match crate::autostart::set_enabled(app, turning_on) {
        Ok(()) => {
            if let (crate::autostart::State::HeldByOther(path), true) = (&before, turning_on) {
                log::info!("开机启动已开启：接管原本指向 {path} 的快捷方式");
            } else {
                log::info!("开机启动已{}", if turning_on { "开启" } else { "关闭" });
            }
            let after = crate::autostart::state(app).unwrap_or(if turning_on {
                crate::autostart::State::Enabled
            } else {
                crate::autostart::State::Disabled
            });
            sync_autostart_item(&after);
        }
        Err(error) => {
            log::error!("开机启动切换失败：{error}");
            sync_autostart_item(&before);
        }
    }
}

fn sync_autostart_item(state: &crate::autostart::State) {
    if let Some(item) = AUTOSTART_ITEM.lock().unwrap().as_ref() {
        let _ = item.set_checked(state.is_on());
        let _ = item.set_text(crate::autostart::label(state));
    }
}
