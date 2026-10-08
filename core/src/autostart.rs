//! 开机启动的纯逻辑（不碰文件系统与注册表）。
//!
//! Windows 端的读写由 `app` crate 负责；这里只回答三件事：启动文件夹里那条
//! 快捷方式算不算「我的」、启动自愈该做什么、用户点开关该做什么。放进 core 是
//! 为了能跑单测——app crate 的测试 harness 在本机 windows-gnu 工具链下起不来
//! （见 `scripts/check.ps1` 的说明）。
//!
//! 背景：安装版（`%LOCALAPPDATA%\Halcyon\Halcyon.exe`）与便携版
//! （`app\halcyon.exe`）共用同一条 `…\Startup\Halcyon.lnk`。只按「链接是否
//! 存在」判断会互相抢：谁启动谁把它改写到自己的 exe 上。所以归属判定必须带上
//! 「链接目标是不是本份」。

/// `StartupApproved\StartupFolder` 的「已启用」值：12 字节，首字节 `0x02`，
/// 其余 11 字节是保留的时间戳位（置零即可）。
pub const APPROVED_ENABLED: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// 审批值是否表示「已启用」。
///
/// 只有首字节 `0x02` 才算。`0x03` 是「在任务管理器里被禁用过」留下的残值，
/// 必须当成未启用并在启用时覆盖写回，否则重建快捷方式也不会开机启动。
pub fn approved_is_enabled(bytes: &[u8]) -> bool {
    bytes.first() == Some(&APPROVED_ENABLED[0])
}

/// 启动文件夹里那条快捷方式相对「本份安装」的归属。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// 既没有快捷方式，也没有旧版 HKCU Run 项
    Absent,
    /// 快捷方式目标就是本 exe
    Ours,
    /// 快捷方式存在但指向别的 exe（另一份安装持有）
    Other(String),
    /// 只有旧版 HKCU Run 项（历史遗留，需要迁移成快捷方式）
    LegacyRun,
}

/// 路径等价：忽略大小写、首尾空白与成对引号，并把 `/` 归一成 `\`。
pub fn paths_equal(a: &str, b: &str) -> bool {
    normalize_path(a) == normalize_path(b)
}

fn normalize_path(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .trim()
        .replace('/', "\\")
        .to_ascii_lowercase()
}

/// 归属判定。`link_target` 为 `None` 表示没有快捷方式。
pub fn ownership(link_target: Option<&str>, our_exe: &str, legacy_run: bool) -> Ownership {
    match link_target.map(|value| value.trim()) {
        Some(value) if !value.is_empty() => {
            if paths_equal(value, our_exe) {
                Ownership::Ours
            } else {
                Ownership::Other(value.trim_matches('"').to_string())
            }
        }
        _ if legacy_run => Ownership::LegacyRun,
        _ => Ownership::Absent,
    }
}

impl Ownership {
    /// 托盘勾选态：只有「就是本份」或「待迁移的旧 Run 项」才算开。
    pub fn is_on(&self) -> bool {
        matches!(self, Ownership::Ours | Ownership::LegacyRun)
    }

    /// 由另一份安装持有时的目标路径（给 UI 展示）。
    pub fn held_by(&self) -> Option<&str> {
        match self {
            Ownership::Other(path) => Some(path),
            _ => None,
        }
    }
}

/// 启动时自愈动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconcile {
    /// 什么都不做
    Nothing,
    /// 按本 exe 重建链接（目录搬迁 / 改名 / 参数变化的自愈）
    Rebuild,
    /// 旧 Run 项迁移成链接
    Migrate,
    /// 链接属于另一份安装，**不要动它**
    LeaveOther,
}

/// 自愈动作判定。
pub fn reconcile_action(ownership: &Ownership) -> Reconcile {
    match ownership {
        Ownership::Ours => Reconcile::Rebuild,
        Ownership::LegacyRun => Reconcile::Migrate,
        Ownership::Other(_) => Reconcile::LeaveOther,
        Ownership::Absent => Reconcile::Nothing,
    }
}

/// 用户显式开关时要做的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// 写一条指向本 exe 的链接（已经是我们时是重建，是别人时是接管）
    Create,
    /// 删链接与同名审批值
    Remove,
    /// 什么都不做（典型：要求关闭，但链接是另一份安装的）
    Nothing,
}

/// 开关动作判定。
pub fn toggle_action(ownership: &Ownership, enabled: bool) -> Toggle {
    if enabled {
        return Toggle::Create;
    }
    match ownership {
        Ownership::Ours => Toggle::Remove,
        _ => Toggle::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 安装版：`%LOCALAPPDATA%\Halcyon\Halcyon.exe`
    const EXE_INSTALLED: &str = r"C:\Users\me\AppData\Local\Halcyon\Halcyon.exe";
    /// 便携版：exe 旁的 `data\` 目录（示例用通用路径，不写开发者本机路径）
    const EXE_PORTABLE: &str = r"C:\Apps\Halcyon\app\halcyon.exe";

    // ---------- 归属判定 ----------

    #[test]
    fn absent_when_no_link_and_no_legacy_run() {
        assert_eq!(ownership(None, EXE_PORTABLE, false), Ownership::Absent);
        assert_eq!(
            ownership(Some("   "), EXE_PORTABLE, false),
            Ownership::Absent
        );
        assert!(!Ownership::Absent.is_on());
        assert_eq!(Ownership::Absent.held_by(), None);
    }

    #[test]
    fn ours_ignores_case_quotes_and_slash_direction() {
        assert_eq!(
            ownership(Some(EXE_PORTABLE), EXE_PORTABLE, false),
            Ownership::Ours
        );
        assert_eq!(
            ownership(
                Some(&EXE_PORTABLE.to_ascii_uppercase()),
                EXE_PORTABLE,
                false
            ),
            Ownership::Ours
        );
        // 斜杠方向 + 引号：用通用路径动态构造，避免把开发者本机路径写进仓库
        assert_eq!(
            ownership(
                Some(&format!("\"{}\"", EXE_PORTABLE.replace('\\', "/"))),
                EXE_PORTABLE,
                false
            ),
            Ownership::Ours
        );
        assert!(Ownership::Ours.is_on());
    }

    #[test]
    fn other_when_target_is_a_different_install() {
        let other = ownership(Some(EXE_INSTALLED), EXE_PORTABLE, false);
        assert_eq!(other, Ownership::Other(EXE_INSTALLED.to_string()));
        assert!(!other.is_on(), "另一份安装持有 ≠ 本份已启用");
        assert_eq!(other.held_by(), Some(EXE_INSTALLED));
    }

    #[test]
    fn a_different_path_is_not_ours() {
        assert!(!paths_equal(EXE_PORTABLE, EXE_INSTALLED));
        assert!(!paths_equal(
            r"C:\app\halcyon.exe",
            r"C:\app\halcyon-other.exe"
        ));
        assert!(paths_equal(r"C:\App\Halcyon.exe", r"c:/app/halcyon.exe"));
    }

    #[test]
    fn legacy_run_only_decides_when_there_is_no_link() {
        assert_eq!(ownership(None, EXE_PORTABLE, true), Ownership::LegacyRun);
        // 链接存在时以链接为准；旧 Run 项只做清理，不改变归属
        assert_eq!(
            ownership(Some(EXE_INSTALLED), EXE_PORTABLE, true),
            Ownership::Other(EXE_INSTALLED.to_string())
        );
        assert!(
            Ownership::LegacyRun.is_on(),
            "旧 Run 项代表「曾经启用」的意图"
        );
    }

    // ---------- 自愈 ----------

    #[test]
    fn reconcile_rebuilds_only_our_own_link() {
        assert_eq!(reconcile_action(&Ownership::Ours), Reconcile::Rebuild);
        assert_eq!(reconcile_action(&Ownership::LegacyRun), Reconcile::Migrate);
        assert_eq!(reconcile_action(&Ownership::Absent), Reconcile::Nothing);
        assert_eq!(
            reconcile_action(&Ownership::Other(EXE_INSTALLED.to_string())),
            Reconcile::LeaveOther
        );
    }

    // ---------- 开关 ----------

    #[test]
    fn disable_only_removes_our_own_link() {
        assert_eq!(toggle_action(&Ownership::Ours, false), Toggle::Remove);
        assert_eq!(toggle_action(&Ownership::Absent, false), Toggle::Nothing);
        assert_eq!(toggle_action(&Ownership::LegacyRun, false), Toggle::Nothing);
        assert_eq!(
            toggle_action(&Ownership::Other(EXE_INSTALLED.to_string()), false),
            Toggle::Nothing,
            "不能替另一份安装关掉开机启动"
        );
    }

    #[test]
    fn enable_always_writes_our_own_link_and_takes_over() {
        assert_eq!(toggle_action(&Ownership::Absent, true), Toggle::Create);
        assert_eq!(toggle_action(&Ownership::Ours, true), Toggle::Create);
        assert_eq!(toggle_action(&Ownership::LegacyRun, true), Toggle::Create);
        assert_eq!(
            toggle_action(&Ownership::Other(EXE_INSTALLED.to_string()), true),
            Toggle::Create,
            "用户显式开启 = 接管"
        );
    }

    // ---------- 两份安装互不篡改（H1 必测） ----------

    #[test]
    fn two_installs_do_not_fight_over_the_single_link() {
        // 机器上只有一条链接：`…\Startup\Halcyon.lnk`
        let mut link: Option<String> = None;

        // 1. 便携版启用 → 链接指向便携版
        let portable_view = ownership(link.as_deref(), EXE_PORTABLE, false);
        assert_eq!(toggle_action(&portable_view, true), Toggle::Create);
        link = Some(EXE_PORTABLE.to_string());

        // 2. 安装版启动：看到链接是别人的 → 不抢、不动，只报告
        let installed_view = ownership(link.as_deref(), EXE_INSTALLED, false);
        assert_eq!(installed_view, Ownership::Other(EXE_PORTABLE.to_string()));
        assert!(!installed_view.is_on());
        assert_eq!(reconcile_action(&installed_view), Reconcile::LeaveOther);
        assert_eq!(
            link.as_deref(),
            Some(EXE_PORTABLE),
            "另一份安装不得改写链接"
        );

        // 3. 便携版自己再启动：仍然自愈重建，归属不变
        let portable_again = ownership(link.as_deref(), EXE_PORTABLE, false);
        assert!(portable_again.is_on());
        assert_eq!(reconcile_action(&portable_again), Reconcile::Rebuild);

        // 4. 安装版被显式「开启」→ 接管，链接转到安装版
        assert_eq!(toggle_action(&installed_view, true), Toggle::Create);
        link = Some(EXE_INSTALLED.to_string());

        // 5. 反过来成立：便携版此时看到的是「别人持有」
        let portable_after = ownership(link.as_deref(), EXE_PORTABLE, false);
        assert_eq!(portable_after, Ownership::Other(EXE_INSTALLED.to_string()));
        assert_eq!(reconcile_action(&portable_after), Reconcile::LeaveOther);
        assert_eq!(link.as_deref(), Some(EXE_INSTALLED));
    }

    // ---------- StartupApproved 审批值（0x03 残留坑） ----------

    #[test]
    fn approved_value_is_twelve_bytes_starting_with_0x02() {
        assert_eq!(APPROVED_ENABLED.len(), 12);
        assert_eq!(APPROVED_ENABLED[0], 0x02);
        assert!(APPROVED_ENABLED[1..].iter().all(|byte| *byte == 0));
        assert!(approved_is_enabled(&APPROVED_ENABLED));
    }

    #[test]
    fn stale_0x03_approval_counts_as_disabled() {
        let mut stale = APPROVED_ENABLED;
        stale[0] = 0x03;
        assert!(
            !approved_is_enabled(&stale),
            "0x03 是任务管理器里禁用过的残值，必须当成未启用"
        );
        assert!(!approved_is_enabled(&[]));
        assert!(!approved_is_enabled(&[0x00; 12]));
    }
}
