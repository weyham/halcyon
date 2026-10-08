use crate::update::{
    GitHubErrorInfo, ManifestEnvelope, ResolvedRelease, UpdateError, UpdateSource,
    UpdateSourceErrorCode, UpdateSourceKind,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, ETAG, IF_NONE_MATCH};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

#[derive(Clone, Debug)]
pub struct GitHubSourceConfig {
    pub owner: String,
    pub repo: String,
    pub api_base: String,
    pub allow_insecure_http: bool,
    pub allow_any_host: bool,
    pub include_prerelease: bool,
    pub allowed_redirect_hosts: Vec<String>,
}

impl Default for GitHubSourceConfig {
    fn default() -> Self {
        Self {
            owner: "weyham".to_string(),
            repo: "halcyon".to_string(),
            api_base: "https://api.github.com".to_string(),
            allow_insecure_http: false,
            allow_any_host: false,
            include_prerelease: option_env!("HALCYON_UPDATE_INCLUDE_PRERELEASE") == Some("1"),
            allowed_redirect_hosts: vec![
                "api.github.com".into(),
                "github.com".into(),
                "objects.githubusercontent.com".into(),
                "release-assets.githubusercontent.com".into(),
            ],
        }
    }
}

// 公开仓的 release 匿名可读：更新检查不再携带任何凭据，也没有授权流程。
// 仅保留传输与状态映射；GitHub 匿名 API 限额 60 次/小时/IP，更新检查远用不满。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointKind {
    RepoLatestRelease,
    ManifestAsset,
    ManifestSignatureAsset,
    ArtifactAsset,
}

impl EndpointKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::RepoLatestRelease => "repo_latest_release",
            Self::ManifestAsset => "manifest_asset",
            Self::ManifestSignatureAsset => "manifest_signature_asset",
            Self::ArtifactAsset => "artifact_asset",
        }
    }

    fn is_asset(self) -> bool {
        matches!(
            self,
            Self::ManifestAsset | Self::ManifestSignatureAsset | Self::ArtifactAsset
        )
    }
}

fn url_host(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

#[allow(clippy::too_many_arguments)]
fn format_http_trace(
    endpoint: EndpointKind,
    method: &str,
    host: &str,
    had_authorization: bool,
    status: Option<u16>,
    request_id: Option<&str>,
    redirect_host: Option<&str>,
    error_code: Option<&str>,
) -> String {
    format!(
        "endpoint={} method={} host={} authorization={} status={:?} request_id={:?} redirect_host={:?} error_code={:?}",
        endpoint.as_str(),
        method,
        host,
        had_authorization,
        status,
        request_id,
        redirect_host,
        error_code,
    )
}

#[allow(clippy::too_many_arguments)]
fn trace_http_response(
    endpoint: EndpointKind,
    method: &str,
    host: &str,
    had_authorization: bool,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    redirect_host: Option<&str>,
    error: Option<&UpdateError>,
) {
    let request_id = headers
        .get("x-github-request-id")
        .and_then(|value| value.to_str().ok());
    let error_code = error.map(|value| format!("{:?}", value.view().code));
    let rendered = format_http_trace(
        endpoint,
        method,
        host,
        had_authorization,
        Some(status.as_u16()),
        request_id,
        redirect_host,
        error_code.as_deref(),
    );
    if error.is_some() {
        log::warn!(target: "halcyon::update::http", "{rendered}");
    } else {
        log::debug!(target: "halcyon::update::http", "{rendered}");
    }
}

pub struct GitHubReleaseSource {
    config: GitHubSourceConfig,
    http: Client,
    release_cache: std::sync::Mutex<Option<ResolvedRelease>>,
}

impl GitHubReleaseSource {
    pub fn new(config: GitHubSourceConfig) -> Result<Self, UpdateError> {
        let http = Client::builder()
            .user_agent(concat!("Halcyon/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        Ok(Self {
            config,
            http,
            release_cache: std::sync::Mutex::new(None),
        })
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}", self.config.api_base.trim_end_matches('/'), path)
    }

    async fn get_json<T>(
        &self,
        url: &str,
        etag: Option<&str>,
    ) -> Result<(Option<String>, T), UpdateError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let host = url_host(url);
        let mut request = self
            .http
            .get(url)
            .header(ACCEPT, "application/vnd.github+json");
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(map_transport_error)?;
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return Err(UpdateError::Internal("not_modified".into()));
        }
        if !status.is_success() {
            let headers = response.headers().clone();
            let error_body = if status == StatusCode::FORBIDDEN {
                response.text().await.ok()
            } else {
                None
            };
            let error = map_status(
                status,
                Some(&headers),
                error_body.as_deref(),
                EndpointKind::RepoLatestRelease,
                Some(&host),
            );
            trace_http_response(
                EndpointKind::RepoLatestRelease,
                "GET",
                &host,
                false,
                status,
                &headers,
                None,
                Some(&error),
            );
            return Err(error);
        }
        trace_http_response(
            EndpointKind::RepoLatestRelease,
            "GET",
            &host,
            false,
            status,
            response.headers(),
            None,
            None,
        );
        let next_etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let value = response.json().await.map_err(map_transport_error)?;
        Ok((next_etag, value))
    }

    async fn fetch_asset_bytes(
        &self,
        asset_id: u64,
        max_size: u64,
        endpoint: EndpointKind,
    ) -> Result<Vec<u8>, UpdateError> {
        let mut url = Url::parse(&self.api_url(&format!(
            "/repos/{}/{}/releases/assets/{}",
            self.config.owner, self.config.repo, asset_id
        )))
        .map_err(|error| UpdateError::Internal(error.to_string()))?;

        for _ in 0..=5 {
            ensure_allowed_url(&url, &self.config)?;
            let request = self
                .http
                .get(url.clone())
                .header(ACCEPT, "application/octet-stream");
            let response = request.send().await.map_err(map_transport_error)?;
            let status = response.status();
            if status == StatusCode::FOUND {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| UpdateError::Internal("302 缺少 Location".into()))?;
                let next = url
                    .join(location)
                    .map_err(|error| UpdateError::Internal(error.to_string()))?;
                trace_http_response(
                    endpoint,
                    "GET",
                    &url_host(url.as_str()),
                    false,
                    status,
                    response.headers(),
                    next.host_str(),
                    None,
                );
                url = next;
                continue;
            }
            if !status.is_success() {
                let headers = response.headers().clone();
                let error_body = if status == StatusCode::FORBIDDEN {
                    response.text().await.ok()
                } else {
                    None
                };
                let host = url_host(url.as_str());
                let error = map_status(
                    status,
                    Some(&headers),
                    error_body.as_deref(),
                    endpoint,
                    Some(&host),
                );
                trace_http_response(
                    endpoint,
                    "GET",
                    &host,
                    false,
                    status,
                    &headers,
                    None,
                    Some(&error),
                );
                return Err(error);
            }
            trace_http_response(
                endpoint,
                "GET",
                &url_host(url.as_str()),
                false,
                status,
                response.headers(),
                None,
                None,
            );
            let content_length = response.content_length();
            if content_length.is_some_and(|length| length > max_size) {
                return Err(UpdateError::InvalidArchive(format!(
                    "Content-Length {length} 超过上限 {max_size}",
                    length = content_length.unwrap_or_default()
                )));
            }
            let mut bytes = Vec::new();
            let mut total = 0u64;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(map_transport_error)?;
                total = total.saturating_add(chunk.len() as u64);
                if total > max_size {
                    return Err(UpdateError::InvalidArchive(format!(
                        "下载流超过上限 {max_size}"
                    )));
                }
                bytes.extend_from_slice(&chunk);
            }
            if content_length.is_some_and(|length| length != total) {
                return Err(UpdateError::InvalidArchive(format!(
                    "Content-Length {length} 与实际下载大小 {total} 不一致",
                    length = content_length.unwrap_or_default()
                )));
            }
            return Ok(bytes);
        }
        Err(UpdateError::Network("GitHub 重定向次数过多".into()))
    }
}

#[async_trait]
impl UpdateSource for GitHubReleaseSource {
    fn kind(&self) -> UpdateSourceKind {
        UpdateSourceKind::Github
    }

    fn configured(&self) -> bool {
        // 公开仓无需任何配置即可读取
        true
    }

    async fn resolve_latest_release(&self) -> Result<Option<ResolvedRelease>, UpdateError> {
        let cached_etag = self
            .release_cache
            .lock()
            .ok()
            .and_then(|cache| cache.as_ref().and_then(|release| release.etag.clone()));
        let (etag, release) = if self.config.include_prerelease {
            let url = self.api_url(&format!(
                "/repos/{}/{}/releases?per_page=20",
                self.config.owner, self.config.repo
            ));
            let (etag, releases) = self
                .get_json::<Vec<GitHubRelease>>(&url, cached_etag.as_deref())
                .await?;
            let release = releases.into_iter().find(|release| {
                release
                    .assets
                    .iter()
                    .any(|asset| asset.name == "latest.json")
                    && release
                        .assets
                        .iter()
                        .any(|asset| asset.name == "latest.json.minisig")
            });
            let Some(release) = release else {
                return Ok(None);
            };
            (etag, release)
        } else {
            let url = self.api_url(&format!(
                "/repos/{}/{}/releases/latest",
                self.config.owner, self.config.repo
            ));
            match self
                .get_json::<GitHubRelease>(&url, cached_etag.as_deref())
                .await
            {
                Ok(value) => value,
                Err(UpdateError::Internal(message)) if message == "not_modified" => {
                    return self
                        .release_cache
                        .lock()
                        .ok()
                        .and_then(|cache| cache.clone())
                        .map(Some)
                        .ok_or_else(|| {
                            UpdateError::InvalidManifest("304 响应缺少本地缓存".into())
                        });
                }
                Err(error) => return Err(error),
            }
        };
        let manifest = release
            .assets
            .iter()
            .find(|asset| asset.name == "latest.json")
            .ok_or_else(|| UpdateError::InvalidManifest("Release 缺少 latest.json".into()))?;
        let signature = release
            .assets
            .iter()
            .find(|asset| asset.name == "latest.json.minisig")
            .ok_or_else(|| {
                UpdateError::InvalidManifest("Release 缺少 latest.json.minisig".into())
            })?;
        let resolved = ResolvedRelease {
            tag: release.tag_name,
            manifest_asset_id: manifest.id,
            manifest_signature_asset_id: signature.id,
            html_url: release.html_url,
            etag,
        };
        if let Ok(mut cache) = self.release_cache.lock() {
            *cache = Some(resolved.clone());
        }
        Ok(Some(resolved))
    }

    async fn fetch_manifest(
        &self,
        release: &ResolvedRelease,
    ) -> Result<ManifestEnvelope, UpdateError> {
        let manifest_bytes = self
            .fetch_asset_bytes(
                release.manifest_asset_id,
                1024 * 1024,
                EndpointKind::ManifestAsset,
            )
            .await?;
        let signature_bytes = self
            .fetch_asset_bytes(
                release.manifest_signature_asset_id,
                1024 * 1024,
                EndpointKind::ManifestSignatureAsset,
            )
            .await?;
        Ok(ManifestEnvelope {
            manifest_bytes,
            signature_bytes,
            release: release.clone(),
        })
    }

    async fn fetch_artifact(&self, asset_id: u64, max_size: u64) -> Result<Vec<u8>, UpdateError> {
        self.fetch_asset_bytes(asset_id, max_size, EndpointKind::ArtifactAsset)
            .await
    }
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    id: u64,
    name: String,
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn map_transport_error(error: reqwest::Error) -> UpdateError {
    if error.is_timeout() || error.is_connect() || error.is_request() {
        UpdateError::Network(error.to_string())
    } else {
        UpdateError::Internal(error.to_string())
    }
}

fn map_status(
    status: StatusCode,
    headers: Option<&reqwest::header::HeaderMap>,
    body: Option<&str>,
    endpoint: EndpointKind,
    host: Option<&str>,
) -> UpdateError {
    let retry_after = headers.and_then(|headers| {
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
    });
    let remaining = headers.and_then(|headers| {
        headers
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    });
    let reset = headers.and_then(|headers| {
        headers
            .get("x-ratelimit-reset")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
    });
    let request_id = headers.and_then(|headers| {
        headers
            .get("x-github-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    });
    let reset_retry = reset.map(|reset| (reset - now_epoch()).max(0) as u64);
    let retry = retry_after.or(reset_retry);
    let is_saml = body
        .map(|body| {
            let body = body.to_lowercase();
            body.contains("saml") || body.contains("sso") || body.contains("single sign-on")
        })
        .unwrap_or(false);

    let (code, message, retryable) = match status {
        StatusCode::UNAUTHORIZED => (
            UpdateSourceErrorCode::Unauthorized,
            "GitHub 拒绝请求（HTTP 401）".to_string(),
            true,
        ),
        StatusCode::FORBIDDEN if is_saml => (
            UpdateSourceErrorCode::SamlSso,
            "GitHub SSO/组织权限限制，请完成 SAML SSO 后重新授权".to_string(),
            true,
        ),
        StatusCode::FORBIDDEN if remaining == Some(0) || retry_after.is_some() => (
            UpdateSourceErrorCode::RateLimited,
            "GitHub 请求被限流".to_string(),
            true,
        ),
        StatusCode::FORBIDDEN if endpoint.is_asset() => (
            UpdateSourceErrorCode::AssetDownloadForbidden,
            format!(
                "GitHub 资产下载被主机 {} 拒绝（HTTP 403），这不是 GitHub App 仓库权限问题",
                host.unwrap_or("unknown")
            ),
            true,
        ),
        StatusCode::FORBIDDEN => (
            UpdateSourceErrorCode::Forbidden,
            "GitHub 权限不足或资源不可见".to_string(),
            false,
        ),
        StatusCode::NOT_FOUND => (
            UpdateSourceErrorCode::NotFound,
            "GitHub 资源不存在或当前账号不可见".to_string(),
            false,
        ),
        StatusCode::TOO_MANY_REQUESTS => (
            UpdateSourceErrorCode::RateLimited,
            "GitHub 请求被限流".to_string(),
            true,
        ),
        _ if status.is_server_error() => (
            UpdateSourceErrorCode::NetworkUnreachable,
            format!("GitHub 返回 {status}"),
            true,
        ),
        _ => (
            UpdateSourceErrorCode::Internal,
            format!("GitHub 返回 {status}"),
            true,
        ),
    };

    let mut info = GitHubErrorInfo::new(status.as_u16(), code, message, retryable);
    info.endpoint = Some(endpoint.as_str().to_string());
    info.host = host.map(str::to_string);
    info.request_id = request_id;
    info.retry_after_seconds = retry;
    info.rate_limit_remaining = remaining;
    info.rate_limit_reset = reset;
    UpdateError::GitHub(Box::new(info))
}

fn ensure_allowed_url(url: &Url, config: &GitHubSourceConfig) -> Result<(), UpdateError> {
    if url.scheme() != "https" && !(config.allow_insecure_http && url.scheme() == "http") {
        return Err(UpdateError::Network("GitHub 重定向到了不允许的协议".into()));
    }
    if url.scheme() == "https" && url.port().is_some_and(|port| port != 443) {
        return Err(UpdateError::Network("GitHub 重定向到了非标准端口".into()));
    }
    let host = url
        .host_str()
        .ok_or_else(|| UpdateError::Network("GitHub 重定向缺少主机名".into()))?;
    if config.allow_any_host
        || config
            .allowed_redirect_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        Ok(())
    } else {
        Err(UpdateError::Network(format!(
            "GitHub 重定向主机不在允许列表: {host}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config_for(server: &MockServer) -> GitHubSourceConfig {
        GitHubSourceConfig {
            owner: "weyham".to_string(),
            repo: "halcyon".to_string(),
            api_base: server.uri(),
            allow_insecure_http: true,
            allow_any_host: false,
            include_prerelease: false,
            allowed_redirect_hosts: vec!["127.0.0.1".into(), "localhost".into()],
        }
    }

    async fn source(server: &MockServer) -> GitHubReleaseSource {
        GitHubReleaseSource::new(config_for(server)).unwrap()
    }

    #[test]
    fn default_source_targets_halcyon_repository() {
        let config = GitHubSourceConfig::default();
        assert_eq!(config.owner, "weyham");
        assert_eq!(config.repo, "halcyon");
        assert_eq!(config.api_base, "https://api.github.com");
    }

    /// 公开仓：不带任何凭据即可解析 latest release（响应不含 Authorization 头）。
    #[tokio::test]
    async fn resolves_latest_release_without_credentials() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/weyham/halcyon/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v1.0.0",
                "html_url": "https://github.com/weyham/halcyon/releases/tag/v1.0.0",
                "assets": [
                    {"id": 10, "name": "latest.json"},
                    {"id": 11, "name": "latest.json.minisig"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let source = source(&server).await;
        let resolved = source
            .resolve_latest_release()
            .await
            .unwrap()
            .expect("release 应可解析");
        assert_eq!(resolved.tag, "v1.0.0");
        assert_eq!(resolved.manifest_asset_id, 10);
        assert_eq!(resolved.manifest_signature_asset_id, 11);
    }

    #[tokio::test]
    async fn missing_latest_json_is_rejected_as_invalid_manifest() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/weyham/halcyon/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag_name": "v1.0.0",
                "html_url": "https://example.com",
                "assets": [{"id": 10, "name": "halcyon-v1.0.0-windows-x64-update.zip"}]
            })))
            .mount(&server)
            .await;

        let source = source(&server).await;
        let error = source.resolve_latest_release().await.unwrap_err();
        assert!(matches!(error, UpdateError::InvalidManifest(_)));
    }

    #[tokio::test]
    async fn release_not_found_maps_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/weyham/halcyon/releases/latest"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let source = source(&server).await;
        let error = source.resolve_latest_release().await.unwrap_err();
        let view = error.view();
        assert_eq!(view.code, UpdateSourceErrorCode::NotFound);
    }

    /// 资产下载：跟随到允许主机的重定向，不带凭据。
    #[tokio::test]
    async fn artifact_download_follows_allowed_redirect_without_credentials() {
        let server = MockServer::start().await;
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/repos/weyham/halcyon/releases/assets/42"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", format!("{base}/cdn/blob")),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/cdn/blob"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"zip-bytes".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let source = source(&server).await;
        let bytes = source.fetch_artifact(42, 1024).await.unwrap();
        assert_eq!(bytes, b"zip-bytes");
    }

    #[test]
    fn allowed_url_rejects_unknown_host() {
        let config = GitHubSourceConfig::default();
        let url = Url::parse("https://evil.example.com/blob").unwrap();
        assert!(ensure_allowed_url(&url, &config).is_err());
        let url = Url::parse("https://objects.githubusercontent.com/blob").unwrap();
        assert!(ensure_allowed_url(&url, &config).is_ok());
    }
}
