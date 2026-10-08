//! 开机启动。
//!
//! Windows 走「用户启动文件夹里的快捷方式」（`...\Startup\Halcyon.lnk`），
//! 不再用 tauri-plugin-autostart 写的 `HKCU\...\Run` 项：实测本机上新建的
//! Run 项不会被 Windows 执行（Shell-Core 9707/9708 里查不到记录），而同一批
//! exe 放进启动文件夹就能正常拉起（实测结论）。
//!
//! 快捷方式保存启动参数（`--autostart`）和工作目录，比 Run 字符串更可靠；
//! 应用每次启动做一次 reconcile 自愈（目录搬迁、改名、旧 Run 项残留都能收敛）。
//!
//! 同一台机器上可能同时存在安装版（`%LOCALAPPDATA%\Halcyon\Halcyon.exe`）与
//! 便携版（`app\halcyon.exe`），它们**共用这一条链接名**。「这条链接算不算本份的」
//! 由 core 的纯逻辑判定（`halcyon_core::autostart`）：
//! 目标指向另一份安装时，本份不接管、不改写，只在托盘里显示「由另一份安装持有」。
//!
//! macOS 继续用 tauri-plugin-autostart 的 LaunchAgent。

use tauri::AppHandle;

#[cfg(not(windows))]
use tauri_plugin_autostart::ManagerExt;

/// 开机启动的对外状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// 未启用
    Disabled,
    /// 本份安装已启用
    Enabled,
    /// 启动文件夹里的快捷方式由另一份安装持有（值为它指向的 exe）
    HeldByOther(String),
}

impl State {
    /// 托盘勾选态：只有「本份已启用」才算开。
    pub fn is_on(&self) -> bool {
        matches!(self, State::Enabled)
    }
}

/// 托盘菜单项文案：被另一份安装持有时要明说，免得用户以为开关坏了。
pub fn label(state: &State) -> &'static str {
    match state {
        State::HeldByOther(_) => "开机启动（由另一份安装持有）",
        _ => "开机启动",
    }
}

/// 当前状态。
pub fn state(app: &AppHandle) -> Result<State, String> {
    #[cfg(windows)]
    {
        let _ = app;
        platform::state()
    }
    #[cfg(not(windows))]
    {
        let on = app.autolaunch().is_enabled().map_err(|e| e.to_string())?;
        Ok(if on { State::Enabled } else { State::Disabled })
    }
}

/// 当前是否已启用开机启动（等价于 `state(app)?.is_on()`）。
pub fn is_enabled(app: &AppHandle) -> Result<bool, String> {
    state(app).map(|state| state.is_on())
}

/// 打开/关闭开机启动。
///
/// 开启时若链接被另一份安装持有，会**接管**（改写成本份）；关闭时若链接不是本份的，
/// 什么都不做——不替别人关。
pub fn set_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = app;
        platform::set_enabled(enabled)
    }
    #[cfg(not(windows))]
    {
        let manager = app.autolaunch();
        if enabled {
            manager.enable()
        } else {
            manager.disable()
        }
        .map_err(|e| e.to_string())
    }
}

/// 启动时自愈：本份的链接按当前 exe 重建、旧版 Run 项迁移、别人的链接不动。
/// 返回自愈后的状态。
pub fn reconcile(app: &AppHandle) -> Result<State, String> {
    #[cfg(windows)]
    {
        let _ = app;
        platform::reconcile()
    }
    #[cfg(not(windows))]
    {
        state(app)
    }
}

#[cfg(windows)]
mod platform {
    use halcyon_core::autostart as policy;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use super::State;

    /// 启动文件夹里的快捷方式名（同时用作 StartupApproved\StartupFolder 的值名）。
    const SHORTCUT_NAME: &str = "Halcyon.lnk";
    /// 快捷方式里带的启动参数，应用据此知道本次是开机拉起。
    const AUTOSTART_ARG: &str = "--autostart";
    /// 旧版（tauri-plugin-autostart）写在 HKCU Run 里的可能名字，需要清理。
    const LEGACY_RUN_NAMES: &[&str] = &["Halcyon", "codex-responses-shim"];
    /// `reg.exe query` 用的形式（不带冒号）。
    const RUN_KEY_REG: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    /// PowerShell 注册表提供程序用的形式（带冒号）—— `Remove-ItemProperty` 只认这个。
    const RUN_KEY_PS: &str = r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED_RUN_KEY: &str =
        r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    const APPROVED_STARTUP_KEY: &str =
        r"HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// StartupApproved 的「已启用」值，字节布局由 core 单测锁定
    /// （首字节 0x02，其余 11 字节置零；0x03 是禁用残值）。
    fn approved_enabled_literal() -> String {
        let bytes = policy::APPROVED_ENABLED
            .iter()
            .map(|byte| format!("0x{byte:02X}"))
            .collect::<Vec<_>>()
            .join(",");
        format!("([byte[]]({bytes}))")
    }

    fn startup_dir() -> Result<PathBuf, String> {
        let appdata = std::env::var_os("APPDATA")
            .ok_or_else(|| "读不到 APPDATA，无法定位 Windows 启动文件夹".to_string())?;
        Ok(PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup"))
    }

    fn shortcut_path() -> Result<PathBuf, String> {
        Ok(startup_dir()?.join(SHORTCUT_NAME))
    }

    /// PowerShell 单引号字面量（内部单引号翻倍转义）。
    fn ps_literal(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn spawn_powershell(script: &str) -> Result<std::process::Output, String> {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-WindowStyle",
                "Hidden",
                "-Command",
                script,
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("无法调用 PowerShell 配置启动项：{error}"))
    }

    fn run_powershell(script: &str) -> Result<(), String> {
        let output = spawn_powershell(script)?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "Windows 启动项配置失败：{}",
            powershell_error(&output)
        ))
    }

    fn run_powershell_capture(script: &str) -> Result<String, String> {
        let output = spawn_powershell(script)?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).to_string());
        }
        Err(format!(
            "Windows 启动项读取失败：{}",
            powershell_error(&output)
        ))
    }

    fn powershell_error(output: &std::process::Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "未知错误".to_string()
        }
    }

    /// 读回快捷方式的目标；不存在返回 `None`，存在但读不到返回 `Err`。
    fn link_target() -> Result<Option<String>, String> {
        let shortcut = shortcut_path()?;
        if !shortcut.exists() {
            return Ok(None);
        }
        let script = format!(
            "$ErrorActionPreference='Stop'; \
             $shell=New-Object -ComObject WScript.Shell; \
             $link=$shell.CreateShortcut({path}); \
             [Console]::Out.Write($link.TargetPath); exit 0",
            path = ps_literal(&shortcut.display().to_string()),
        );
        let target = run_powershell_capture(&script)?;
        Ok(Some(target.trim().to_string()))
    }

    /// 本条链接相对「本份安装」的归属。
    fn current_ownership() -> Result<policy::Ownership, String> {
        let exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序：{e}"))?;
        let exe = exe.display().to_string();
        let target = link_target()?;
        Ok(policy::ownership(
            target.as_deref(),
            &exe,
            legacy_run_exists(),
        ))
    }

    pub fn state() -> Result<State, String> {
        Ok(match current_ownership()? {
            policy::Ownership::Other(path) => State::HeldByOther(path),
            policy::Ownership::Ours | policy::Ownership::LegacyRun => State::Enabled,
            policy::Ownership::Absent => State::Disabled,
        })
    }

    pub fn set_enabled(enabled: bool) -> Result<(), String> {
        let ownership = current_ownership().unwrap_or_else(|error| {
            log::warn!("开机启动：读取现有快捷方式失败（{error}），按「没有链接」处理");
            policy::Ownership::Absent
        });
        match policy::toggle_action(&ownership, enabled) {
            policy::Toggle::Create => {
                create_shortcut()?;
                if let policy::Ownership::Other(path) = &ownership {
                    log::info!("开机启动：已接管原本指向 {path} 的快捷方式");
                }
            }
            policy::Toggle::Remove => remove_shortcut()?,
            policy::Toggle::Nothing => {
                if let policy::Ownership::Other(path) = &ownership {
                    log::info!("开机启动：快捷方式由另一份安装持有（{path}），保持不变");
                }
            }
        }
        // 旧 Run 项无论是开还是关都清掉：它已经不再是我们使用的机制。
        cleanup_legacy_run()
    }

    pub fn reconcile() -> Result<State, String> {
        let ownership = current_ownership()?;
        let state = match policy::reconcile_action(&ownership) {
            policy::Reconcile::Rebuild | policy::Reconcile::Migrate => {
                // 目录搬迁 / 改名 / 旧 Run 项迁移都收敛到「按本 exe 重建」。
                create_shortcut()?;
                State::Enabled
            }
            policy::Reconcile::LeaveOther => match ownership {
                policy::Ownership::Other(path) => State::HeldByOther(path),
                _ => State::Disabled,
            },
            policy::Reconcile::Nothing => State::Disabled,
        };
        cleanup_legacy_run()?;
        Ok(state)
    }

    fn create_shortcut() -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("无法定位当前程序：{e}"))?;
        let working = exe
            .parent()
            .ok_or_else(|| "无法定位程序所在目录".to_string())?;
        let shortcut = shortcut_path()?;
        let script = format!(
            "$ErrorActionPreference='Stop'; \
             $startup={shortcut}; $target={target}; $working={working}; $arg={arg}; \
             New-Item -ItemType Directory -Path (Split-Path -Parent $startup) -Force | Out-Null; \
             $shell=New-Object -ComObject WScript.Shell; \
             $link=$shell.CreateShortcut($startup); \
             $link.TargetPath=$target; $link.Arguments=$arg; $link.WorkingDirectory=$working; \
             $link.WindowStyle=7; $link.Description='Halcyon'; $link.Save(); \
             $read=$shell.CreateShortcut($startup); \
             if ($read.TargetPath -ne $target) {{ throw ('shortcut-target-mismatch: ' + $read.TargetPath) }}; \
             if ($read.Arguments -ne $arg) {{ throw ('shortcut-arguments-mismatch: ' + $read.Arguments) }}; \
             if ($read.WorkingDirectory -ne $working) {{ throw ('shortcut-workdir-mismatch: ' + $read.WorkingDirectory) }}; \
             $approved={approved}; New-Item -Path $approved -Force | Out-Null; \
             Remove-ItemProperty -LiteralPath $approved -Name {name} -ErrorAction SilentlyContinue; \
             Set-ItemProperty -LiteralPath $approved -Name {name} -Value {value} -Type Binary -Force; \
             exit 0",
            shortcut = ps_literal(&shortcut.display().to_string()),
            target = ps_literal(&exe.display().to_string()),
            working = ps_literal(&working.display().to_string()),
            arg = ps_literal(AUTOSTART_ARG),
            approved = ps_literal(APPROVED_STARTUP_KEY),
            name = ps_literal(SHORTCUT_NAME),
            value = approved_enabled_literal(),
        );
        run_powershell(&script)
    }

    fn remove_shortcut() -> Result<(), String> {
        let shortcut = shortcut_path()?;
        let script = format!(
            "$ErrorActionPreference='SilentlyContinue'; \
             Remove-Item -LiteralPath {shortcut} -Force -ErrorAction SilentlyContinue; \
             Remove-ItemProperty -LiteralPath {approved} -Name {name} -ErrorAction SilentlyContinue; \
             exit 0",
            shortcut = ps_literal(&shortcut.display().to_string()),
            approved = ps_literal(APPROVED_STARTUP_KEY),
            name = ps_literal(SHORTCUT_NAME),
        );
        run_powershell(&script)
    }

    fn legacy_run_exists() -> bool {
        LEGACY_RUN_NAMES.iter().any(|name| {
            Command::new("reg.exe")
                .args(["query", RUN_KEY_REG, "/v", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        })
    }

    fn cleanup_legacy_run() -> Result<(), String> {
        let names = LEGACY_RUN_NAMES
            .iter()
            .map(|name| ps_literal(name))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!(
            "$ErrorActionPreference='SilentlyContinue'; \
             $run={run}; $approved={approved}; \
             foreach ($n in @({names})) {{ \
               Remove-ItemProperty -LiteralPath $run -Name $n -ErrorAction SilentlyContinue; \
               Remove-ItemProperty -LiteralPath $approved -Name $n -ErrorAction SilentlyContinue \
             }}; exit 0",
            run = ps_literal(RUN_KEY_PS),
            approved = ps_literal(APPROVED_RUN_KEY),
            names = names,
        );
        run_powershell(&script)
    }
}
