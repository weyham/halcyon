use crate::update::{
    GitHubErrorInfo, ManifestEnvelope, PlatformArtifact, ResolvedRelease, UpdateError,
    UpdateManifest, UpdateSource, UpdateSourceErrorCode, UpdateSourceKind,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

#[derive(Clone, Debug)]
pub struct GitHubSourceConfig {
    pub owner: String,
    pub repo: String,
    /// 站点基址（默认 https://github.com；测试指向 mock 服务器）。
    pub site_base: String,
    pub allow_insecure_http: bool,
    pub allow_any_host: bool,
    pub allowed_redirect_hosts: Vec<String>,
}

impl Default for GitHubSourceConfig {
    fn default() -> Self {
        Self {
            owner: "weyham".to_string(),
            repo: "halcyon".to_string(),
            site_base: "https://github.com".to_string(),
            allow_insecure_http: false,
            allow_any_host: false,
            allowed_redirect_hosts: vec![
                "github.com".into(),
                "objects.githubusercontent.com".into(),
                "release-assets.githubusercontent.com".into(),
            ],
        }
    }
}

// 零 API 更新源：全程走 github.com 的 release 下载路由
// （releases/latest/download/<资产名>），不触碰 api.github.com。
// 网页/CDN 路由不受匿名 API 限额（60 次/小时/IP）约束，公开仓无需任何凭据。
// 清单 latest.json 自带版本号与制品 URL，因此连 tag 解析都不需要 API。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointKind {
    ManifestLatest,
    ManifestSignature,
    Artifact,
}

impl EndpointKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ManifestLatest => "manifest_latest",
            Self::ManifestSignature => "manifest_signature",
            Self::Artifact => "artifact",
        }
    }

    fn is_asset(self) -> bool {
        true
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
    envelope_cache: std::sync::Mutex<Option<ManifestEnvelope>>,
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
            envelope_cache: std::sync::Mutex::new(None),
        })
    }

    /// `releases/latest/download/<资产名>` 下载路由。
    fn download_url(&self, asset_name: &str) -> String {
        format!(
            "{}/{}/{}/releases/latest/download/{}",
            self.config.site_base.trim_end_matches('/'),
            self.config.owner,
            self.config.repo,
            asset_name
        )
    }

    async fn fetch_bytes(
        &self,
        url: Url,
        max_size: u64,
        endpoint: EndpointKind,
    ) -> Result<Vec<u8>, UpdateError> {
        let mut url = url;
        for _ in 0..=5 {
            ensure_allowed_url(&url, &self.config)?;
            let response = self
                .http
                .get(url.clone())
                .send()
                .await
                .map_err(map_transport_error)?;
            let status = response.status();
            if matches!(
                status,
                StatusCode::FOUND | StatusCode::MOVED_PERMANENTLY | StatusCode::TEMPORARY_REDIRECT
            ) {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| UpdateError::Internal("重定向缺少 Location".into()))?;
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
        let manifest_url = Url::parse(&self.download_url("latest.json"))
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        let manifest_bytes = match self
            .fetch_bytes(manifest_url, 1024 * 1024, EndpointKind::ManifestLatest)
            .await
        {
            Ok(bytes) => bytes,
            // 还没有任何已发布 Release 时该路由返回 404，视为「无更新」
            Err(UpdateError::GitHub(info)) if info.status == 404 => return Ok(None),
            Err(error) => return Err(error),
        };
        let manifest = UpdateManifest::parse(&manifest_bytes)?;
        let signature_url = Url::parse(&self.download_url("latest.json.minisig"))
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        let signature_bytes = self
            .fetch_bytes(signature_url, 1024 * 1024, EndpointKind::ManifestSignature)
            .await
            .map_err(|error| match error {
                UpdateError::GitHub(info) if info.status == 404 => {
                    UpdateError::InvalidManifest("Release 缺少 latest.json.minisig".into())
                }
                other => other,
            })?;
        let release = ResolvedRelease {
            tag: format!("v{}", manifest.version),
            html_url: format!(
                "{}/{}/{}/releases/tag/v{}",
                self.config.site_base.trim_end_matches('/'),
                self.config.owner,
                self.config.repo,
                manifest.version
            ),
        };
        let envelope = ManifestEnvelope {
            manifest_bytes,
            signature_bytes,
            release: release.clone(),
        };
        if let Ok(mut cache) = self.envelope_cache.lock() {
            *cache = Some(envelope);
        }
        Ok(Some(release))
    }

    async fn fetch_manifest(
        &self,
        release: &ResolvedRelease,
    ) -> Result<ManifestEnvelope, UpdateError> {
        // 清单与签名在 resolve 阶段已一并下载，这里直接返回缓存
        self.envelope_cache
            .lock()
            .ok()
            .and_then(|cache| cache.clone())
            .filter(|envelope| envelope.release.tag == release.tag)
            .ok_or_else(|| UpdateError::Internal("resolve_latest_release 尚未完成".into()))
    }

    async fn fetch_artifact(
        &self,
        artifact: &PlatformArtifact,
        max_size: u64,
    ) -> Result<Vec<u8>, UpdateError> {
        let url = artifact
            .url
            .as_deref()
            .ok_or_else(|| UpdateError::InvalidManifest("制品缺少 url".into()))?;
        let url =
            Url::parse(url).map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
        self.fetch_bytes(url, max_size, EndpointKind::Artifact)
            .await
    }
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
                "GitHub 资产下载被主机 {} 拒绝（HTTP 403）",
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
            site_base: server.uri(),
            allow_insecure_http: true,
            allow_any_host: false,
            allowed_redirect_hosts: vec!["127.0.0.1".into(), "localhost".into()],
        }
    }

    async fn source(server: &MockServer) -> GitHubReleaseSource {
        GitHubReleaseSource::new(config_for(server)).unwrap()
    }

    fn manifest_json() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "protocol": 1,
            "version": "1.0.1",
            "platforms": {
                "windows-x86_64": {
                    "url": "https://github.com/weyham/halcyon/releases/download/v1.0.1/halcyon-v1.0.1-windows-x64-update.zip",
                    "signature": "sig",
                    "sha256": "0".repeat(64),
                    "size": 1
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn default_source_targets_halcyon_repository() {
        let config = GitHubSourceConfig::default();
        assert_eq!(config.owner, "weyham");
        assert_eq!(config.repo, "halcyon");
        assert_eq!(config.site_base, "https://github.com");
    }

    /// 零 API：清单与签名经 releases/latest/download 路由获取，tag 由清单版本构造。
    #[tokio::test]
    async fn resolves_manifest_from_download_route() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/weyham/halcyon/releases/latest/download/latest.json"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest_json()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/weyham/halcyon/releases/latest/download/latest.json.minisig",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"sig-bytes".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let source = source(&server).await;
        let resolved = source
            .resolve_latest_release()
            .await
            .unwrap()
            .expect("release 应可解析");
        assert_eq!(resolved.tag, "v1.0.1");
        assert!(resolved.html_url.ends_with("/releases/tag/v1.0.1"));

        let envelope = source.fetch_manifest(&resolved).await.unwrap();
        assert_eq!(
            UpdateManifest::parse(&envelope.manifest_bytes)
                .unwrap()
                .version,
            "1.0.1"
        );
        assert_eq!(envelope.signature_bytes, b"sig-bytes");
    }

    /// 没有任何已发布 Release 时（404）按「无更新」处理。
    #[tokio::test]
    async fn missing_manifest_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/weyham/halcyon/releases/latest/download/latest.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let source = source(&server).await;
        assert!(source.resolve_latest_release().await.unwrap().is_none());
    }

    /// 签名缺失是发布事故，必须报 InvalidManifest 而不是静默通过。
    #[tokio::test]
    async fn missing_signature_is_invalid_manifest() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/weyham/halcyon/releases/latest/download/latest.json"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest_json()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/weyham/halcyon/releases/latest/download/latest.json.minisig",
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let source = source(&server).await;
        let error = source.resolve_latest_release().await.unwrap_err();
        assert!(matches!(error, UpdateError::InvalidManifest(_)));
    }

    /// 制品下载：跟随到允许主机的重定向。
    #[tokio::test]
    async fn artifact_download_follows_allowed_redirect() {
        let server = MockServer::start().await;
        let base = server.uri();
        Mock::given(method("GET"))
            .and(path("/weyham/halcyon/releases/download/v1.0.1/update.zip"))
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
        let artifact = PlatformArtifact {
            asset_id: None,
            url: Some(format!(
                "{base}/weyham/halcyon/releases/download/v1.0.1/update.zip"
            )),
            signature: String::new(),
            sha256: String::new(),
            size: 0,
            helper: None,
            manual_only: false,
            allow_downgrade: false,
        };
        let bytes = source.fetch_artifact(&artifact, 1024).await.unwrap();
        assert_eq!(bytes, b"zip-bytes");
    }

    /// 制品没有 url 时 fail fast（清单由发布链保证必带 url）。
    #[tokio::test]
    async fn artifact_requires_url() {
        let server = MockServer::start().await;
        let source = source(&server).await;
        let artifact = PlatformArtifact {
            asset_id: Some(42),
            url: None,
            signature: String::new(),
            sha256: String::new(),
            size: 0,
            helper: None,
            manual_only: false,
            allow_downgrade: false,
        };
        let error = source.fetch_artifact(&artifact, 1024).await.unwrap_err();
        assert!(matches!(error, UpdateError::InvalidManifest(_)));
    }

    #[test]
    fn allowed_url_rejects_unknown_host() {
        let config = GitHubSourceConfig::default();
        let url = Url::parse("https://evil.example.com/blob").unwrap();
        assert!(ensure_allowed_url(&url, &config).is_err());
        let url = Url::parse("https://objects.githubusercontent.com/blob").unwrap();
        assert!(ensure_allowed_url(&url, &config).is_ok());
        let url =
            Url::parse("https://github.com/weyham/halcyon/releases/latest/download/latest.json")
                .unwrap();
        assert!(ensure_allowed_url(&url, &config).is_ok());
    }
}
