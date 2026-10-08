use halcyon_core::update::github::{GitHubReleaseSource, GitHubSourceConfig};
use halcyon_core::update::plan::{check_for_update, stage_update, StagedUpdate, UpdateOffer};
use halcyon_core::update::{UpdateError, UpdateSource, UpdateSourceError};
use halcyon_core::velopack_runtime::{self, InstallKind, VelopackErrorView};
use serde::Serialize;
use std::sync::{Arc, Mutex};

pub fn public_key() -> &'static str {
    option_env!("HALCYON_UPDATE_PUBLIC_KEY").unwrap_or("")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    UpdateAvailable,
    Downloading,
    ReadyToInstall,
    Installing,
    Completed,
    ManualDownload,
    Error,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStateView {
    pub phase: UpdatePhase,
    pub current_version: String,
    pub available_version: Option<String>,
    pub notes: Option<String>,
    pub release_url: Option<String>,
    pub manual_only: bool,
    pub error: Option<UpdateSourceError>,
}

#[derive(Default)]
struct RuntimeState {
    phase: Option<UpdatePhase>,
    available_version: Option<String>,
    notes: Option<String>,
    release_url: Option<String>,
    manual_only: bool,
    error: Option<UpdateSourceError>,
    offer: Option<UpdateOffer>,
    staged: Option<StagedUpdate>,
    /// 安装版：Velopack 检查得到的可用更新。
    velopack_update: Option<velopack::UpdateInfo>,
}

pub struct UpdateRuntime {
    source: Arc<dyn UpdateSource>,
    public_key: String,
    current_version: String,
    state: Mutex<RuntimeState>,
    /// Velopack 判定出的运行形态（installed / portable / 非 Velopack）。
    install_kind: InstallKind,
    /// 安装版更新是否包含 pre-release（运行时配置 `update.include_prerelease`）。
    ///
    /// 1.0.6 那个已打包的二进制把它写死成 false，只有运行时读取才有意义；
    /// 保存配置 → 重启代理 后由 app 侧刷新。
    include_prerelease: std::sync::atomic::AtomicBool,
}

impl UpdateRuntime {
    pub fn new(
        current_version: impl Into<String>,
        include_prerelease: bool,
    ) -> Result<Self, String> {
        let source = Arc::new(
            GitHubReleaseSource::new(GitHubSourceConfig::default())
                .map_err(|error| error.to_string())?,
        );
        Ok(Self {
            source,
            public_key: public_key().to_string(),
            current_version: current_version.into(),
            state: Mutex::new(RuntimeState::default()),
            install_kind: velopack_runtime::detect_install_kind(),
            include_prerelease: std::sync::atomic::AtomicBool::new(include_prerelease),
        })
    }

    /// 刷新「是否包含 pre-release」（配置加载 / 保存重启后调用）。
    pub fn set_include_prerelease(&self, value: bool) {
        self.include_prerelease
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    fn include_prerelease(&self) -> bool {
        self.include_prerelease
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn view(&self) -> UpdateStateView {
        let state = self.state.lock().unwrap();
        UpdateStateView {
            phase: state.phase.unwrap_or(UpdatePhase::Idle),
            current_version: self.current_version.clone(),
            available_version: state.available_version.clone(),
            notes: state.notes.clone(),
            release_url: state.release_url.clone(),
            manual_only: state.manual_only,
            error: state.error.clone(),
        }
    }

    /// 自动检查更新（启动后 + 每 3 小时）：静默检查，有更新（且非手动平台）时静默下载。
    ///
    /// 与手动操作的差异：
    /// - 忙碌（检查 / 下载 / 安装中）直接跳过，不打断用户或并发 tick；
    /// - 失败只写日志，不写「检查失败」可见状态（凌晨的网络抖动不该变成面板红字）；
    /// - 阶段最多推进到 ReadyToInstall——安装永远由用户手动触发。
    pub async fn auto_tick(&self, staging_root: &std::path::Path) {
        {
            let state = self.state.lock().unwrap();
            if matches!(
                state.phase,
                Some(UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Installing)
            ) {
                return;
            }
        }
        if self.install_kind().is_installed() {
            if let Err(error) = self.check_velopack().await {
                log::warn!("自动检查更新失败（安装版）：{}", error.message);
                self.quiet_idle();
                return;
            }
            let (version, manual) = {
                let state = self.state.lock().unwrap();
                (state.available_version.clone(), state.manual_only)
            };
            if version.is_some() && !manual {
                if let Err(error) = self.download_velopack().await {
                    log::warn!("自动下载更新失败（安装版）：{}", error.message);
                    self.quiet_idle();
                }
            }
            return;
        }
        if let Err(error) = self.check().await {
            log::warn!("自动检查更新失败：{error}");
            self.quiet_idle();
            return;
        }
        let (version, manual, phase) = {
            let state = self.state.lock().unwrap();
            (
                state.available_version.clone(),
                state.manual_only,
                state.phase,
            )
        };
        if version.is_some() && !manual && phase != Some(UpdatePhase::ReadyToInstall) {
            if let Err(error) = self.download(staging_root).await {
                log::warn!("自动下载更新失败：{error}");
                self.quiet_idle();
            }
        }
    }

    /// 自动路径的安静复位：中途失败时把"检查中/下载中"收回 Idle，
    /// 不把瞬时错误变成可见的「检查失败」。
    fn quiet_idle(&self) {
        let mut state = self.state.lock().unwrap();
        if matches!(
            state.phase,
            Some(UpdatePhase::Checking | UpdatePhase::Downloading)
        ) {
            state.phase = Some(UpdatePhase::Idle);
        }
    }

    /// 红点徽标：有可用的更高版本时返回（含是否已就绪、是否仅手动）。
    pub fn badge(&self) -> Option<crate::UpdateBadge> {
        let state = self.state.lock().unwrap();
        let version = state.available_version.clone()?;
        Some(crate::UpdateBadge {
            version,
            ready: state.phase == Some(UpdatePhase::ReadyToInstall),
            manual_only: state.manual_only,
        })
    }

    pub async fn check(&self) -> Result<UpdateStateView, UpdateError> {
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::Checking);
            state.error = None;
        }
        if self.public_key.trim().is_empty() {
            let error = UpdateError::InvalidSignature("未配置更新公钥".into());
            self.set_error(error.clone());
            return Err(error);
        }
        let offer = check_for_update(
            self.source.as_ref(),
            &self.current_version,
            &self.public_key,
        )
        .await?;
        {
            let mut state = self.state.lock().unwrap();
            if let Some(offer) = offer {
                state.phase = Some(if offer.artifact.manual_only {
                    UpdatePhase::ManualDownload
                } else {
                    UpdatePhase::UpdateAvailable
                });
                state.available_version = Some(offer.manifest.version.clone());
                state.notes = offer.manifest.notes.clone();
                state.release_url = Some(offer.release.html_url.clone());
                state.manual_only = offer.artifact.manual_only;
                state.offer = Some(offer);
            } else {
                state.phase = Some(UpdatePhase::UpToDate);
                state.available_version = None;
                state.notes = None;
                state.release_url = None;
                state.manual_only = false;
                state.offer = None;
            }
            state.error = None;
        }
        Ok(self.view().await)
    }

    pub async fn download(
        &self,
        staging_root: &std::path::Path,
    ) -> Result<UpdateStateView, UpdateError> {
        let offer = {
            let state = self.state.lock().unwrap();
            state
                .offer
                .clone()
                .ok_or_else(|| UpdateError::Internal("请先检查更新并在有可用版本时下载".into()))?
        };
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::Downloading);
            state.error = None;
        }
        let staged =
            stage_update(self.source.as_ref(), &offer, &self.public_key, staging_root).await?;
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::ReadyToInstall);
            state.staged = Some(staged);
            state.error = None;
        }
        Ok(self.view().await)
    }

    /// 当前 Velopack 运行形态。
    pub fn install_kind(&self) -> InstallKind {
        self.install_kind
    }

    /// 安装版：用 Velopack UpdateManager 检查更新；仅更新内部状态。
    pub async fn check_velopack(&self) -> Result<(), VelopackErrorView> {
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::Checking);
            state.error = None;
        }
        // 公开仓：Velopack 通道同样匿名读取。
        match velopack_runtime::check_for_updates(self.include_prerelease()) {
            Ok(Some(info)) => {
                let version = info.TargetFullRelease.Version.clone();
                let notes = {
                    let n = info.TargetFullRelease.NotesMarkdown.clone();
                    if n.is_empty() {
                        None
                    } else {
                        Some(n)
                    }
                };
                let mut state = self.state.lock().unwrap();
                state.phase = Some(UpdatePhase::UpdateAvailable);
                state.available_version = Some(version);
                state.notes = notes;
                state.release_url = Some(format!("{}/releases", velopack_runtime::GITHUB_REPO_URL));
                state.manual_only = false;
                state.velopack_update = Some(info);
                state.error = None;
                Ok(())
            }
            Ok(None) => {
                let mut state = self.state.lock().unwrap();
                state.phase = Some(UpdatePhase::UpToDate);
                state.available_version = None;
                state.notes = None;
                state.release_url = None;
                state.manual_only = false;
                state.velopack_update = None;
                state.error = None;
                Ok(())
            }
            Err(error) => {
                self.set_velopack_error(&error);
                Err(error)
            }
        }
    }

    /// 安装版：下载已检查到的更新；仅更新内部状态。
    pub async fn download_velopack(&self) -> Result<(), VelopackErrorView> {
        let update = {
            let state = self.state.lock().unwrap();
            state
                .velopack_update
                .clone()
                .ok_or_else(|| VelopackErrorView {
                    kind: velopack_runtime::VelopackErrorKind::Internal,
                    message: "请先检查更新并在有可用版本时下载".into(),
                    retryable: false,
                })?
        };
        {
            let mut state = self.state.lock().unwrap();
            state.phase = Some(UpdatePhase::Downloading);
            state.error = None;
        }
        match velopack_runtime::download_updates(&update, None, self.include_prerelease()) {
            Ok(()) => {
                let mut state = self.state.lock().unwrap();
                state.phase = Some(UpdatePhase::ReadyToInstall);
                state.error = None;
                Ok(())
            }
            Err(error) => {
                self.set_velopack_error(&error);
                Err(error)
            }
        }
    }

    /// 安装版：应用更新并重启（Velopack）。
    pub async fn apply_velopack(&self) -> Result<(), VelopackErrorView> {
        let update = {
            let state = self.state.lock().unwrap();
            state
                .velopack_update
                .clone()
                .ok_or_else(|| VelopackErrorView {
                    kind: velopack_runtime::VelopackErrorKind::Internal,
                    message: "没有已下载的更新".into(),
                    retryable: false,
                })?
        };
        self.mark_installing();
        velopack_runtime::apply_updates_and_restart(&update, self.include_prerelease())
    }

    fn set_velopack_error(&self, error: &VelopackErrorView) {
        // 以前这里只把错误塞进 state：日志里什么都没有，现场排查只能靠猜（F2）。
        // 只记分类与 Velopack 原始 message，token 绝不进日志。
        log::warn!(
            "更新失败：kind={:?} retryable={} message={}",
            error.kind,
            error.retryable,
            error.message
        );
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::Error);
        state.error = Some(UpdateSourceError::new(
            match error.kind {
                velopack_runtime::VelopackErrorKind::AuthRequired => {
                    halcyon_core::update::UpdateSourceErrorCode::Unauthorized
                }
                _ => halcyon_core::update::UpdateSourceErrorCode::Internal,
            },
            error.message.clone(),
            error.retryable,
        ));
        state.velopack_update = None;
    }

    pub fn staged(&self) -> Option<StagedUpdate> {
        self.state.lock().unwrap().staged.clone()
    }

    pub fn mark_installing(&self) {
        self.state.lock().unwrap().phase = Some(UpdatePhase::Installing);
    }

    pub fn mark_completed(&self) {
        self.state.lock().unwrap().phase = Some(UpdatePhase::Completed);
    }

    pub fn set_error(&self, error: UpdateError) {
        log::warn!(
            target: "halcyon::update",
            "{}",
            error.diagnostic_log()
        );
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::Error);
        state.error = Some(error.view());
    }

    pub fn mark_recovery_required(&self, message: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.phase = Some(UpdatePhase::Error);
        state.error = Some(UpdateSourceError::new(
            halcyon_core::update::UpdateSourceErrorCode::Internal,
            message,
            true,
        ));
    }
}

#[cfg(test)]
impl UpdateRuntime {
    fn with_source_for_test(
        source: Arc<dyn UpdateSource>,
        public_key: impl Into<String>,
        current_version: impl Into<String>,
    ) -> Self {
        Self {
            source,
            public_key: public_key.into(),
            current_version: current_version.into(),
            state: Mutex::new(RuntimeState::default()),
            install_kind: InstallKind::NotVelopack,
            include_prerelease: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halcyon_core::update::{
        GitHubErrorInfo, ManifestEnvelope, ResolvedRelease, UpdateSourceErrorCode, UpdateSourceKind,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakySource {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl UpdateSource for FlakySource {
        fn kind(&self) -> UpdateSourceKind {
            UpdateSourceKind::Github
        }

        fn configured(&self) -> bool {
            true
        }

        async fn resolve_latest_release(&self) -> Result<Option<ResolvedRelease>, UpdateError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut info = GitHubErrorInfo::new(
                    403,
                    UpdateSourceErrorCode::Forbidden,
                    "GitHub forbidden",
                    false,
                );
                info.request_id = Some("REQ-STALE".into());
                return Err(UpdateError::GitHub(Box::new(info)));
            }
            Ok(None)
        }

        async fn fetch_manifest(
            &self,
            _release: &ResolvedRelease,
        ) -> Result<ManifestEnvelope, UpdateError> {
            Err(UpdateError::Internal("not implemented".into()))
        }

        async fn fetch_artifact(
            &self,
            _asset_id: u64,
            _max_size: u64,
        ) -> Result<Vec<u8>, UpdateError> {
            Err(UpdateError::Internal("not implemented".into()))
        }
    }

    #[tokio::test]
    async fn new_check_clears_stale_forbidden_error() {
        let runtime = UpdateRuntime::with_source_for_test(
            Arc::new(FlakySource {
                calls: AtomicUsize::new(0),
            }),
            "test-public-key",
            "1.0.0",
        );
        let error = runtime.check().await.unwrap_err();
        runtime.set_error(error);
        assert_eq!(
            runtime.view().await.error.map(|view| view.code),
            Some(UpdateSourceErrorCode::Forbidden)
        );
        let view = runtime.check().await.unwrap();
        assert!(view.error.is_none());
        assert!(matches!(view.phase, UpdatePhase::UpToDate));
    }
}
