use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

pub mod github;
pub mod install;
pub mod journal;
pub mod manifest;
pub mod plan;
pub mod verify;

pub use github::{GitHubReleaseSource, GitHubSourceConfig};
pub use manifest::{
    compare_versions, evaluate_candidate_version, ManifestEnvelope, PlatformArtifact,
    ResolvedRelease, UpdateManifest, VersionCheck, VersionOrder,
};
pub use verify::{verify_artifact, verify_manifest, SignedPayload};

pub const UPDATE_PROTOCOL: u32 = 1;
pub const UPDATE_SCHEMA: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSourceKind {
    Github,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSourceErrorCode {
    AuthorizationPending,
    SlowDown,
    DeviceCodeExpired,
    AccessDenied,
    BadVerificationCode,
    UnverifiedUserEmail,
    BadRefreshToken,
    Unauthorized,
    Forbidden,
    NotFound,
    RateLimited,
    SamlSso,
    AssetDownloadForbidden,
    DeviceFlowForbidden,
    NetworkUnreachable,
    Offline,
    InvalidManifest,
    InvalidSignature,
    HashMismatch,
    DowngradeRejected,
    InvalidArchive,
    UnsupportedPlatform,
    UpdaterBusy,
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSourceError {
    pub code: UpdateSourceErrorCode,
    pub message: String,
    pub retryable: bool,
    pub retry_after_seconds: Option<u64>,
    pub request_id: Option<String>,
    pub status: Option<u16>,
    pub rate_limit_remaining: Option<u64>,
    pub rate_limit_reset: Option<i64>,
    pub endpoint: Option<String>,
    pub host: Option<String>,
}

impl UpdateSourceError {
    pub fn new(code: UpdateSourceErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            retry_after_seconds: None,
            request_id: None,
            status: None,
            rate_limit_remaining: None,
            rate_limit_reset: None,
            endpoint: None,
            host: None,
        }
    }

    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubErrorInfo {
    pub status: u16,
    pub code: UpdateSourceErrorCode,
    pub message: String,
    pub retryable: bool,
    pub retry_after_seconds: Option<u64>,
    pub request_id: Option<String>,
    pub rate_limit_remaining: Option<u64>,
    pub rate_limit_reset: Option<i64>,
    pub endpoint: Option<String>,
    pub host: Option<String>,
}

impl std::fmt::Display for GitHubErrorInfo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl GitHubErrorInfo {
    pub fn new(
        status: u16,
        code: UpdateSourceErrorCode,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retryable,
            retry_after_seconds: None,
            request_id: None,
            rate_limit_remaining: None,
            rate_limit_reset: None,
            endpoint: None,
            host: None,
        }
    }

    pub fn view(&self) -> UpdateSourceError {
        UpdateSourceError {
            code: self.code.clone(),
            message: self.message.clone(),
            retryable: self.retryable,
            retry_after_seconds: self.retry_after_seconds,
            request_id: self.request_id.clone(),
            status: Some(self.status),
            rate_limit_remaining: self.rate_limit_remaining,
            rate_limit_reset: self.rate_limit_reset,
            endpoint: self.endpoint.clone(),
            host: self.host.clone(),
        }
    }

    pub fn diagnostic_log(&self) -> String {
        format!(
            "status={} code={:?} endpoint={:?} host={:?} request_id={:?} retryable={} retry_after_seconds={:?} rate_limit_remaining={:?} rate_limit_reset={:?}",
            self.status,
            self.code,
            self.endpoint,
            self.host,
            self.request_id,
            self.retryable,
            self.retry_after_seconds,
            self.rate_limit_remaining,
            self.rate_limit_reset
        )
    }
}

#[derive(Clone, Debug, Error)]
pub enum UpdateError {
    #[error("等待用户在 GitHub 完成授权")]
    AuthorizationPending,
    #[error("GitHub 请求过快")]
    SlowDown(u64),
    #[error("GitHub 授权已失效，请重新连接")]
    AuthRequired,
    #[error("GitHub 授权被拒绝")]
    AccessDenied,
    #[error("GitHub 设备码已过期")]
    DeviceCodeExpired,
    #[error("GitHub 请求被限流")]
    RateLimited(Option<u64>),
    #[error("{0}")]
    GitHub(Box<GitHubErrorInfo>),
    #[error("GitHub 权限不足")]
    Forbidden,
    #[error("GitHub 资源不存在")]
    NotFound,
    #[error("网络不可达：{0}")]
    Network(String),
    #[error("更新清单无效：{0}")]
    InvalidManifest(String),
    #[error("签名验证失败：{0}")]
    InvalidSignature(String),
    #[error("SHA-256 校验失败")]
    HashMismatch,
    #[error("拒绝降级更新")]
    DowngradeRejected,
    #[error("更新包格式无效：{0}")]
    InvalidArchive(String),
    #[error("当前平台不支持自动更新")]
    UnsupportedPlatform,
    #[error("更新器正忙")]
    UpdaterBusy,
    #[error("本地更新状态错误：{0}")]
    Internal(String),
}

impl UpdateError {
    pub fn diagnostic_log(&self) -> String {
        match self {
            Self::GitHub(info) => info.diagnostic_log(),
            _ => {
                let view = self.view();
                format!(
                    "status={:?} code={:?} request_id={:?} retryable={} retry_after_seconds={:?}",
                    view.status,
                    view.code,
                    view.request_id,
                    view.retryable,
                    view.retry_after_seconds
                )
            }
        }
    }

    pub fn view(&self) -> UpdateSourceError {
        match self {
            Self::AuthorizationPending => UpdateSourceError::new(
                UpdateSourceErrorCode::AuthorizationPending,
                self.to_string(),
                true,
            ),
            Self::SlowDown(seconds) => {
                UpdateSourceError::new(UpdateSourceErrorCode::SlowDown, self.to_string(), true)
                    .with_retry_after(*seconds)
            }
            Self::AuthRequired => {
                UpdateSourceError::new(UpdateSourceErrorCode::Unauthorized, self.to_string(), true)
            }
            Self::AccessDenied => {
                UpdateSourceError::new(UpdateSourceErrorCode::AccessDenied, self.to_string(), false)
            }
            Self::DeviceCodeExpired => UpdateSourceError::new(
                UpdateSourceErrorCode::DeviceCodeExpired,
                self.to_string(),
                true,
            ),
            Self::RateLimited(seconds) => {
                let error = UpdateSourceError::new(
                    UpdateSourceErrorCode::RateLimited,
                    self.to_string(),
                    true,
                );
                seconds.map_or(error.clone(), |value| error.with_retry_after(value))
            }
            Self::GitHub(info) => info.view(),
            Self::Forbidden => {
                UpdateSourceError::new(UpdateSourceErrorCode::Forbidden, self.to_string(), false)
            }
            Self::NotFound => {
                UpdateSourceError::new(UpdateSourceErrorCode::NotFound, self.to_string(), false)
            }
            Self::Network(message) => UpdateSourceError::new(
                UpdateSourceErrorCode::NetworkUnreachable,
                message.clone(),
                true,
            ),
            Self::InvalidManifest(message) => UpdateSourceError::new(
                UpdateSourceErrorCode::InvalidManifest,
                message.clone(),
                false,
            ),
            Self::InvalidSignature(message) => UpdateSourceError::new(
                UpdateSourceErrorCode::InvalidSignature,
                message.clone(),
                false,
            ),
            Self::HashMismatch => {
                UpdateSourceError::new(UpdateSourceErrorCode::HashMismatch, self.to_string(), false)
            }
            Self::DowngradeRejected => UpdateSourceError::new(
                UpdateSourceErrorCode::DowngradeRejected,
                self.to_string(),
                false,
            ),
            Self::InvalidArchive(message) => UpdateSourceError::new(
                UpdateSourceErrorCode::InvalidArchive,
                message.clone(),
                false,
            ),
            Self::UnsupportedPlatform => UpdateSourceError::new(
                UpdateSourceErrorCode::UnsupportedPlatform,
                self.to_string(),
                false,
            ),
            Self::UpdaterBusy => {
                UpdateSourceError::new(UpdateSourceErrorCode::UpdaterBusy, self.to_string(), true)
            }
            Self::Internal(message) => {
                UpdateSourceError::new(UpdateSourceErrorCode::Internal, message.clone(), true)
            }
        }
    }
}

#[derive(Clone)]
pub struct UpdateSourceContext {
    pub source: Arc<dyn UpdateSource>,
}

#[async_trait]
pub trait UpdateSource: Send + Sync {
    fn kind(&self) -> UpdateSourceKind;
    fn configured(&self) -> bool;
    async fn resolve_latest_release(&self) -> Result<Option<ResolvedRelease>, UpdateError>;
    async fn fetch_manifest(
        &self,
        release: &ResolvedRelease,
    ) -> Result<ManifestEnvelope, UpdateError>;
    async fn fetch_artifact(&self, asset_id: u64, max_size: u64) -> Result<Vec<u8>, UpdateError>;
}

#[cfg(test)]
mod update_tests {
    use super::*;

    #[test]
    fn github_error_diagnostic_log_uses_only_safe_fields() {
        let info = GitHubErrorInfo {
            status: 403,
            code: UpdateSourceErrorCode::Forbidden,
            message: "GitHub secret-token".into(),
            retryable: false,
            retry_after_seconds: Some(17),
            request_id: Some("REQ-123".into()),
            rate_limit_remaining: Some(0),
            rate_limit_reset: Some(123),
            endpoint: Some("repo_latest_release".into()),
            host: Some("api.github.com".into()),
        };
        let rendered = info.diagnostic_log();
        assert!(rendered.contains("status=403"));
        assert!(rendered.contains("request_id=Some(\"REQ-123\")"));
        assert!(rendered.contains("retry_after_seconds=Some(17)"));
        assert!(!rendered.contains("secret-token"));
    }
}
