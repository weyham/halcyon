//! Velopack 更新通道（仅安装版）。portable 路径仍走自研 helper/journal，见 update_runtime.rs。
//!
//! 运行形态判定优先使用 Velopack 自身能力（locator 是否存在）：`UpdateManager::new`
//! 在非 Velopack 目录下会返回错误，因此可用来区分 installed / portable / 非 Velopack。
//! 这与 `data\` 目录判定是**两套独立维度**，两者冲突时以本模块的 Velopack
//! 判定为准。

use serde::Serialize;
use velopack::sources::{HttpSource, NoneSource};
use velopack::{UpdateCheck, UpdateInfo, UpdateManager};

/// 仓库主页（UI「查看发布」链接）。
pub const GITHUB_REPO_URL: &str = "https://github.com/weyham/halcyon";

/// 零 API 更新源：`releases/latest/download` 是 CDN 路由，不占 api.github.com 匿名限额。
/// Velopack HttpSource 会取 `<base>/releases.win.json`，包文件按相对文件名拼到同一 base。
const UPDATE_FEED_URL: &str = "https://github.com/weyham/halcyon/releases/latest/download";

/// 运行形态（Velopack 维度）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    /// Velopack 安装版（locator 存在且非 portable）。
    Installed,
    /// Velopack portable 布局（`.portable` + `current\`）。
    VelopackPortable,
    /// 非 Velopack 管理（我们的 `app\` portable、dev `target\`）。
    NotVelopack,
}

impl InstallKind {
    pub fn is_installed(self) -> bool {
        matches!(self, InstallKind::Installed)
    }
}

/// 用 Velopack locator 判定当前运行形态。非 Velopack 目录返回 `NotVelopack`。
pub fn detect_install_kind() -> InstallKind {
    match UpdateManager::new(NoneSource {}, None, None) {
        Ok(manager) => {
            if manager.get_is_portable() {
                InstallKind::VelopackPortable
            } else {
                InstallKind::Installed
            }
        }
        Err(_) => InstallKind::NotVelopack,
    }
}

/// Velopack 错误分类（映射到可读的 UI 分类）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VelopackErrorKind {
    NotInstalled,
    /// GitHub 端鉴权类错误（如限流/权限分类的提示文案）——提示后重试。
    AuthRequired,
    Network,
    Checksum,
    Size,
    Package,
    Unsupported,
    Internal,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VelopackErrorView {
    pub kind: VelopackErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl std::fmt::Display for VelopackErrorView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Velopack 检查结果的语义分类（供 app 层映射到 UI 九态；纯函数便于单测）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    UpToDate,
    UpdateAvailable,
    Error,
}

/// 纯函数：由「是否有可用更新」「是否出错」推导检查结果分类。
pub fn check_outcome(has_update: bool, is_error: bool) -> CheckOutcome {
    if is_error {
        CheckOutcome::Error
    } else if has_update {
        CheckOutcome::UpdateAvailable
    } else {
        CheckOutcome::UpToDate
    }
}

/// 把 velopack::Error 映射成可读分类，不泄漏 token（错误信息只来自 velopack 本身）。
pub fn classify_error(error: &velopack::Error) -> VelopackErrorView {
    use velopack::Error as E;
    let (kind, retryable) = match error {
        E::NotInstalled(_) => (VelopackErrorKind::NotInstalled, false),
        E::Network(_) => (VelopackErrorKind::Network, true),
        E::ChecksumInvalid(..) => (VelopackErrorKind::Checksum, false),
        E::SizeInvalid(..) => (VelopackErrorKind::Size, false),
        E::InvalidPackage(_) | E::Zip(_) | E::Semver(_) => (VelopackErrorKind::Package, false),
        E::NotSupported(_) => (VelopackErrorKind::Unsupported, false),
        _ => (VelopackErrorKind::Internal, false),
    };
    VelopackErrorView {
        kind,
        message: error.to_string(),
        retryable,
    }
}

/// 构造 UpdateManager（HttpSource，零 API 匿名读取公开仓）。
///
/// 零 API 源只暴露 Latest Release（天然不含 pre-release），等价于「过滤 pre-release」语义。
pub fn build_manager() -> Result<UpdateManager, VelopackErrorView> {
    let source = HttpSource::new(UPDATE_FEED_URL);
    UpdateManager::new(source, None, None).map_err(|error| classify_error(&error))
}

/// 检查更新；返回可用的 UpdateInfo（None = 已是最新或源为空）。
pub fn check_for_updates() -> Result<Option<UpdateInfo>, VelopackErrorView> {
    let manager = build_manager()?;
    match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(info)) => Ok(Some(*info)),
        Ok(_) => Ok(None),
        Err(error) => Err(classify_error(&error)),
    }
}

/// 下载指定更新到本地 packages 目录。进度回调可选。
pub fn download_updates(
    update: &UpdateInfo,
    progress: Option<std::sync::mpsc::Sender<i16>>,
) -> Result<(), VelopackErrorView> {
    let manager = build_manager()?;
    manager
        .download_updates(update, progress)
        .map_err(|error| classify_error(&error))
}

/// 应用已下载的更新并重启应用。
pub fn apply_updates_and_restart(update: &UpdateInfo) -> Result<(), VelopackErrorView> {
    let manager = build_manager()?;
    manager
        .apply_updates_and_restart(update)
        .map_err(|error| classify_error(&error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_not_installed_is_not_retryable() {
        let view = classify_error(&velopack::Error::NotInstalled("nope".into()));
        assert_eq!(view.kind, VelopackErrorKind::NotInstalled);
        assert!(!view.retryable);
    }

    #[test]
    fn classify_invalid_package() {
        let view = classify_error(&velopack::Error::InvalidPackage("bad".into()));
        assert_eq!(view.kind, VelopackErrorKind::Package);
        assert!(!view.retryable);
    }

    #[test]
    fn classify_unsupported() {
        let view = classify_error(&velopack::Error::NotSupported("nope".into()));
        assert_eq!(view.kind, VelopackErrorKind::Unsupported);
        assert!(!view.retryable);
    }

    #[test]
    fn classify_other_is_internal() {
        let view = classify_error(&velopack::Error::Other("boom".into()));
        assert_eq!(view.kind, VelopackErrorKind::Internal);
        assert!(!view.retryable);
    }

    #[test]
    fn classify_checksum_is_not_retryable() {
        let view = classify_error(&velopack::Error::ChecksumInvalid(
            std::path::PathBuf::from("x.nupkg"),
            "expected".into(),
            "actual".into(),
        ));
        assert_eq!(view.kind, VelopackErrorKind::Checksum);
        assert!(!view.retryable);
    }

    #[test]
    fn detect_install_kind_is_not_velopack_in_dev_dir() {
        // 单元测试运行在 target/ 下，不是 Velopack 安装目录。
        assert_eq!(detect_install_kind(), InstallKind::NotVelopack);
    }

    #[test]
    fn install_kind_is_installed_only_for_installed() {
        assert!(InstallKind::Installed.is_installed());
        assert!(!InstallKind::VelopackPortable.is_installed());
        assert!(!InstallKind::NotVelopack.is_installed());
    }

    #[test]
    fn check_outcome_mapping() {
        assert_eq!(check_outcome(true, false), CheckOutcome::UpdateAvailable);
        assert_eq!(check_outcome(false, false), CheckOutcome::UpToDate);
        assert_eq!(check_outcome(false, true), CheckOutcome::Error);
        assert_eq!(check_outcome(true, true), CheckOutcome::Error);
    }
}
