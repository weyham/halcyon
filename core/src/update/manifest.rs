use crate::update::{UpdateError, UPDATE_PROTOCOL, UPDATE_SCHEMA};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateManifest {
    pub schema: u32,
    pub protocol: u32,
    pub version: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub platforms: BTreeMap<String, PlatformArtifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformArtifact {
    #[serde(default)]
    pub asset_id: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub signature: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub helper: Option<HelperArtifact>,
    #[serde(default)]
    pub manual_only: bool,
    #[serde(default)]
    pub allow_downgrade: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelperArtifact {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRelease {
    pub tag: String,
    pub html_url: String,
}

#[derive(Clone)]
pub struct ManifestEnvelope {
    pub manifest_bytes: Vec<u8>,
    pub signature_bytes: Vec<u8>,
    pub release: ResolvedRelease,
}

impl UpdateManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, UpdateError> {
        serde_json::from_slice(bytes)
            .map_err(|error| UpdateError::InvalidManifest(error.to_string()))
    }

    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.schema != UPDATE_SCHEMA {
            return Err(UpdateError::InvalidManifest(format!(
                "schema {} 不受支持",
                self.schema
            )));
        }
        if self.protocol != UPDATE_PROTOCOL {
            return Err(UpdateError::InvalidManifest(format!(
                "protocol {} 不受支持",
                self.protocol
            )));
        }
        Version::parse(&self.version)
            .map_err(|error| UpdateError::InvalidManifest(format!("version 无效: {error}")))?;
        if self.platforms.is_empty() {
            return Err(UpdateError::InvalidManifest("platforms 为空".into()));
        }
        for (name, artifact) in &self.platforms {
            artifact.validate(name)?;
        }
        Ok(())
    }

    pub fn select_platform(&self, platform: &str) -> Result<PlatformArtifact, UpdateError> {
        self.platforms
            .get(platform)
            .cloned()
            .ok_or(UpdateError::UnsupportedPlatform)
    }
}

impl PlatformArtifact {
    fn validate(&self, platform: &str) -> Result<(), UpdateError> {
        if self.asset_id.is_none() && self.url.is_none() {
            return Err(UpdateError::InvalidManifest(format!(
                "平台 {platform} 缺少 assetId/url"
            )));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(UpdateError::InvalidManifest(format!(
                "平台 {platform} 的 sha256 无效"
            )));
        }
        if self.signature.trim().is_empty() {
            return Err(UpdateError::InvalidManifest(format!(
                "平台 {platform} 缺少签名"
            )));
        }
        if self.size == 0 {
            return Err(UpdateError::InvalidManifest(format!(
                "平台 {platform} 的 size 无效"
            )));
        }
        if let Some(helper) = &self.helper {
            validate_helper_path(&helper.path).map_err(|message| {
                UpdateError::InvalidManifest(format!("平台 {platform}: {message}"))
            })?;
            if helper.sha256.len() != 64
                || !helper.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(UpdateError::InvalidManifest(format!(
                    "平台 {platform} 的 helper sha256 无效"
                )));
            }
        }
        Ok(())
    }
}

fn validate_helper_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("helper 路径为空".into());
    }
    if path.contains(['/', '\\']) {
        return Err("helper 路径不允许包含分隔符".into());
    }
    let mut components = Path::new(path).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err("helper 路径必须是 staging 根目录下的普通文件名".into()),
    }
}

pub fn platform_key() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        return "windows-x86_64";
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return "darwin-aarch64";
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        return "darwin-x86_64";
    }
    #[allow(unreachable_code)]
    "unsupported"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionCheck {
    UpToDate,
    UpdateAvailable,
}

pub fn evaluate_candidate_version(
    current_version: &str,
    candidate_version: &str,
    allow_downgrade: bool,
) -> Result<VersionCheck, UpdateError> {
    let current = Version::parse(current_version)
        .map_err(|error| UpdateError::InvalidManifest(format!("current version 无效: {error}")))?;
    let candidate = Version::parse(candidate_version).map_err(|error| {
        UpdateError::InvalidManifest(format!("candidate version 无效: {error}"))
    })?;
    match candidate.cmp(&current) {
        std::cmp::Ordering::Equal => Ok(VersionCheck::UpToDate),
        std::cmp::Ordering::Greater => Ok(VersionCheck::UpdateAvailable),
        std::cmp::Ordering::Less if allow_downgrade => Ok(VersionCheck::UpdateAvailable),
        std::cmp::Ordering::Less => Err(UpdateError::DowngradeRejected),
    }
}

/// 检查阶段的版本比较结果（P2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionOrder {
    /// 本地版本 > 清单版本（更新源还没跟上）
    LocalNewer,
    /// 本地版本 == 清单版本
    Same,
    /// 清单版本 > 本地版本（有更新）
    RemoteNewer,
}

/// 检查阶段的版本比较：**只有 [`VersionOrder::RemoteNewer`] 才算有更新**。
///
/// 与 [`evaluate_candidate_version`]（安装阶段的降级保护）刻意分开：
/// 「清单版本 ≤ 本地版本」在检查阶段是正常的「无更新」，不是错误。
/// 检查阶段只回答「有没有更新」：`LocalNewer` 直接视为已是最新，
/// 不做降级拒绝判断——否则 `/releases/latest` 停在旧版本（清单没跟着升）时，
/// **每次检查都会踩到**。真正的降级保护仍在安装阶段由
/// [`ensure_not_downgrade`] 兜住。
pub fn compare_versions(
    current_version: &str,
    candidate_version: &str,
) -> Result<VersionOrder, UpdateError> {
    let current = Version::parse(current_version)
        .map_err(|error| UpdateError::InvalidManifest(format!("current version 无效: {error}")))?;
    let candidate = Version::parse(candidate_version).map_err(|error| {
        UpdateError::InvalidManifest(format!("candidate version 无效: {error}"))
    })?;
    Ok(match candidate.cmp(&current) {
        std::cmp::Ordering::Less => VersionOrder::LocalNewer,
        std::cmp::Ordering::Equal => VersionOrder::Same,
        std::cmp::Ordering::Greater => VersionOrder::RemoteNewer,
    })
}

pub fn ensure_not_downgrade(
    current_version: &str,
    candidate_version: &str,
    allow_downgrade: bool,
) -> Result<(), UpdateError> {
    evaluate_candidate_version(current_version, candidate_version, allow_downgrade)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str, sha256: &str) -> UpdateManifest {
        UpdateManifest {
            schema: UPDATE_SCHEMA,
            protocol: UPDATE_PROTOCOL,
            version: version.into(),
            published_at: None,
            notes: None,
            platforms: BTreeMap::from([(
                "windows-x86_64".into(),
                PlatformArtifact {
                    asset_id: Some(7),
                    url: None,
                    signature: "signature".into(),
                    sha256: sha256.into(),
                    size: 10,
                    helper: Some(HelperArtifact {
                        path: "halcyon-updater.exe".into(),
                        sha256: "a".repeat(64),
                        size: 5,
                    }),
                    manual_only: false,
                    allow_downgrade: false,
                },
            )]),
        }
    }

    #[test]
    fn rejects_downgrade_by_default() {
        assert!(matches!(
            ensure_not_downgrade("1.1.0", "1.0.0", false),
            Err(UpdateError::DowngradeRejected)
        ));
        assert!(ensure_not_downgrade("1.0.0", "1.0.1", false).is_ok());
        assert!(ensure_not_downgrade("1.0.0", "0.9.0", true).is_ok());
    }

    #[test]
    fn evaluates_candidate_version_relation() {
        assert_eq!(
            evaluate_candidate_version("1.0.0", "1.0.0", false).unwrap(),
            VersionCheck::UpToDate
        );
        assert!(ensure_not_downgrade("1.0.0", "1.0.0", false).is_ok());
        assert_eq!(
            evaluate_candidate_version("1.0.0", "1.0.1", false).unwrap(),
            VersionCheck::UpdateAvailable
        );
        assert!(matches!(
            evaluate_candidate_version("1.1.0", "1.0.0", false),
            Err(UpdateError::DowngradeRejected)
        ));
        assert_eq!(
            evaluate_candidate_version("1.1.0", "1.0.0", true).unwrap(),
            VersionCheck::UpdateAvailable
        );
    }

    #[test]
    fn compares_local_and_manifest_versions_for_check() {
        // 本地 > 清单：更新源还没跟上 → 不是错误，按「已是最新」处理（P2）
        assert_eq!(
            compare_versions("1.0.7", "1.0.4").unwrap(),
            VersionOrder::LocalNewer
        );
        // 本地 == 清单
        assert_eq!(
            compare_versions("1.0.7", "1.0.7").unwrap(),
            VersionOrder::Same
        );
        // 本地 < 清单
        assert_eq!(
            compare_versions("1.0.6", "1.0.7").unwrap(),
            VersionOrder::RemoteNewer
        );
        // 版本号非法仍然报错（不能被当成 UpToDate 吞掉）
        assert!(compare_versions("not-a-version", "1.0.7").is_err());
        assert!(compare_versions("1.0.7", "not-a-version").is_err());
    }

    #[test]
    fn validates_manifest_shape() {
        assert!(manifest("1.1.0", &"b".repeat(64)).validate().is_ok());
        let mut invalid = manifest("1.1.0", &"b".repeat(64));
        invalid.platforms.clear();
        assert!(invalid.validate().is_err());
    }
    #[test]
    fn rejects_every_invalid_manifest_field() {
        let mut invalid = manifest("1.1.0", &"b".repeat(64));
        invalid.schema = 2;
        assert!(invalid.validate().is_err());
        invalid.schema = UPDATE_SCHEMA;
        invalid.protocol = 2;
        assert!(invalid.validate().is_err());
        invalid.protocol = UPDATE_PROTOCOL;
        invalid.version = "not-semver".into();
        assert!(invalid.validate().is_err());
        invalid.version = "1.1.0".into();
        invalid
            .platforms
            .get_mut("windows-x86_64")
            .unwrap()
            .asset_id = None;
        assert!(invalid.validate().is_err());
        invalid
            .platforms
            .get_mut("windows-x86_64")
            .unwrap()
            .asset_id = Some(7);
        invalid.platforms.get_mut("windows-x86_64").unwrap().sha256 = "bad".into();
        assert!(invalid.validate().is_err());
        invalid.platforms.get_mut("windows-x86_64").unwrap().sha256 = "b".repeat(64);
        invalid
            .platforms
            .get_mut("windows-x86_64")
            .unwrap()
            .signature
            .clear();
        assert!(invalid.validate().is_err());
        invalid
            .platforms
            .get_mut("windows-x86_64")
            .unwrap()
            .signature = "sig".into();
        invalid.platforms.get_mut("windows-x86_64").unwrap().size = 0;
        assert!(invalid.validate().is_err());
        invalid.platforms.get_mut("windows-x86_64").unwrap().size = 10;
        invalid
            .platforms
            .get_mut("windows-x86_64")
            .unwrap()
            .helper
            .as_mut()
            .unwrap()
            .path
            .clear();
        assert!(invalid.validate().is_err());
        invalid
            .platforms
            .get_mut("windows-x86_64")
            .unwrap()
            .helper
            .as_mut()
            .unwrap()
            .path = "helper".into();
        invalid
            .platforms
            .get_mut("windows-x86_64")
            .unwrap()
            .helper
            .as_mut()
            .unwrap()
            .sha256 = "bad".into();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn rejects_unknown_platform() {
        let item = manifest("1.1.0", &"b".repeat(64));
        assert!(item.select_platform("windows-x86_64").is_ok());
        assert!(matches!(
            item.select_platform("linux"),
            Err(UpdateError::UnsupportedPlatform)
        ));
    }
}
