use crate::update::manifest::{compare_versions, platform_key, VersionOrder};
use crate::update::verify::{
    default_install_allowlist, extract_zip_safe, sha256_hex, verify_artifact, verify_manifest,
    verify_sha256, verify_size,
};
use crate::update::{
    ManifestEnvelope, PlatformArtifact, ResolvedRelease, UpdateError, UpdateManifest, UpdateSource,
};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct UpdateOffer {
    pub release: ResolvedRelease,
    pub manifest: UpdateManifest,
    pub artifact: PlatformArtifact,
    pub platform: String,
}

#[derive(Clone)]
pub struct StagedUpdate {
    pub version: String,
    pub staging_dir: PathBuf,
    pub helper_path: PathBuf,
    pub allowlist: Vec<String>,
}

pub async fn check_for_update(
    source: &dyn UpdateSource,
    current_version: &str,
    public_key: &str,
) -> Result<Option<UpdateOffer>, UpdateError> {
    check_for_update_for_platform(source, current_version, public_key, platform_key()).await
}

pub async fn check_for_update_for_platform(
    source: &dyn UpdateSource,
    current_version: &str,
    public_key: &str,
    platform: &str,
) -> Result<Option<UpdateOffer>, UpdateError> {
    let Some(release) = source.resolve_latest_release().await? else {
        return Ok(None);
    };
    let envelope = source.fetch_manifest(&release).await?;
    let manifest = verify_manifest(
        public_key,
        &envelope.manifest_bytes,
        std::str::from_utf8(&envelope.signature_bytes)
            .map_err(|error| UpdateError::InvalidSignature(error.to_string()))?,
    )?;
    let platform = platform.to_string();
    // 缺键容错：清单签名有效但未包含本平台键（该平台本次未发布），按「无更新」处理而非报错。
    let artifact = match manifest.select_platform(&platform) {
        Ok(artifact) => artifact,
        Err(UpdateError::UnsupportedPlatform) => {
            log::info!("更新清单未包含平台 {platform} 的键（该平台本次未发布），按「无更新」处理");
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    // P2：检查阶段「清单版本 ≤ 本地版本」一律算「已是最新」。
    // 检查阶段只回答「有没有更新」：清单版本低于本地即视为已是最新，
    // 不做降级拒绝判断——否则 `/releases/latest` 停在旧版本（其清单没跟着升）时
    // 每次检查都会踩到。降级保护仍在安装阶段由 ensure_not_downgrade 兜住。
    match compare_versions(current_version, &manifest.version)? {
        VersionOrder::RemoteNewer => {}
        VersionOrder::Same => return Ok(None),
        VersionOrder::LocalNewer => {
            log::info!(
                "更新检查：本地版本 {current_version} 高于清单版本 {}（更新源尚未跟进），按「已是最新」处理",
                manifest.version
            );
            return Ok(None);
        }
    }
    Ok(Some(UpdateOffer {
        release,
        manifest,
        artifact,
        platform,
    }))
}

pub async fn stage_update(
    source: &dyn UpdateSource,
    offer: &UpdateOffer,
    public_key: &str,
    staging_root: &Path,
) -> Result<StagedUpdate, UpdateError> {
    if offer.artifact.manual_only {
        return Err(UpdateError::UnsupportedPlatform);
    }
    if offer.artifact.url.is_none() {
        return Err(UpdateError::InvalidManifest("制品缺少 url".into()));
    }
    let artifact_bytes = source
        .fetch_artifact(&offer.artifact, offer.artifact.size)
        .await?;
    verify_size(artifact_bytes.len(), offer.artifact.size)?;
    verify_artifact(public_key, &artifact_bytes, &offer.artifact.signature)?;
    verify_sha256(&artifact_bytes, &offer.artifact.sha256)?;

    let staging_dir = staging_root.join(&offer.manifest.version);
    if staging_dir.exists() {
        std::fs::remove_dir_all(&staging_dir)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
    }
    let helper_path = offer
        .artifact
        .helper
        .as_ref()
        .map(|helper| helper.path.clone())
        .ok_or_else(|| UpdateError::InvalidManifest("Windows 更新缺少 helper".into()))?;
    let allowlist = default_install_allowlist(Some(&helper_path));
    extract_zip_safe(&artifact_bytes, &staging_dir, &allowlist)?;

    let helper_artifact = offer
        .artifact
        .helper
        .as_ref()
        .ok_or_else(|| UpdateError::InvalidManifest("Windows 更新缺少 helper".into()))?;
    let helper_path = staging_dir.join(&helper_artifact.path);
    let helper_bytes =
        std::fs::read(&helper_path).map_err(|error| UpdateError::Internal(error.to_string()))?;
    if helper_artifact.size != 0 && helper_bytes.len() as u64 != helper_artifact.size {
        return Err(UpdateError::InvalidArchive("helper 大小不匹配".into()));
    }
    if !sha256_hex(&helper_bytes).eq_ignore_ascii_case(&helper_artifact.sha256) {
        return Err(UpdateError::HashMismatch);
    }

    Ok(StagedUpdate {
        version: offer.manifest.version.clone(),
        staging_dir,
        helper_path,
        allowlist: allowlist.into_iter().collect(),
    })
}

pub fn manifest_from_envelope(
    envelope: &ManifestEnvelope,
    public_key: &str,
) -> Result<UpdateManifest, UpdateError> {
    verify_manifest(
        public_key,
        &envelope.manifest_bytes,
        std::str::from_utf8(&envelope.signature_bytes)
            .map_err(|error| UpdateError::InvalidSignature(error.to_string()))?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use minisign::KeyPair;
    use serde_json::json;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    struct FakeSource {
        manifest: Vec<u8>,
        manifest_signature: Vec<u8>,
        artifact: Vec<u8>,
        has_release: bool,
    }

    impl FakeSource {
        fn sign(sk: &minisign::SecretKey, data: &[u8]) -> String {
            String::from(minisign::sign(None, sk, Cursor::new(data), None, None).unwrap())
        }

        fn new() -> (Self, String) {
            let keys = KeyPair::generate_unencrypted_keypair().unwrap();
            let mut zip = Vec::new();
            {
                let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
                writer
                    .start_file("halcyon.exe", SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(b"new-app").unwrap();
                writer
                    .start_file("halcyon-updater.exe", SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(b"new-helper").unwrap();
                writer.finish().unwrap();
            }
            let artifact_signature = Self::sign(&keys.sk, &zip);
            let sha256 = sha256_hex(&zip);
            let manifest = serde_json::to_vec(&json!({
                "schema": 1,
                "protocol": 1,
                "version": "1.0.1",
                "notes": "test release",
                "platforms": {
                    "windows-x86_64": {
                        "assetId": 7,
                        "url": "https://example.invalid/update.zip",
                        "signature": artifact_signature,
                        "sha256": sha256,
                        "size": zip.len(),
                        "helper": {
                            "path": "halcyon-updater.exe",
                            "sha256": sha256_hex(b"new-helper"),
                            "size": 10
                        },
                        "manualOnly": false,
                        "allowDowngrade": false
                    }
                }
            }))
            .unwrap();
            let manifest_signature = Self::sign(&keys.sk, &manifest).into_bytes();
            (
                Self {
                    manifest,
                    manifest_signature,
                    artifact: zip,
                    has_release: true,
                },
                keys.pk.to_base64(),
            )
        }
    }

    #[async_trait]
    impl UpdateSource for FakeSource {
        fn kind(&self) -> crate::update::UpdateSourceKind {
            crate::update::UpdateSourceKind::Github
        }

        fn configured(&self) -> bool {
            true
        }

        async fn resolve_latest_release(&self) -> Result<Option<ResolvedRelease>, UpdateError> {
            if !self.has_release {
                return Ok(None);
            }
            Ok(Some(ResolvedRelease {
                tag: "v1.0.1".into(),
                html_url: "https://example.invalid/release".into(),
            }))
        }

        async fn fetch_manifest(
            &self,
            release: &ResolvedRelease,
        ) -> Result<ManifestEnvelope, UpdateError> {
            Ok(ManifestEnvelope {
                manifest_bytes: self.manifest.clone(),
                signature_bytes: self.manifest_signature.clone(),
                release: release.clone(),
            })
        }

        async fn fetch_artifact(
            &self,
            _artifact: &PlatformArtifact,
            _max_size: u64,
        ) -> Result<Vec<u8>, UpdateError> {
            Ok(self.artifact.clone())
        }
    }

    #[tokio::test]
    async fn same_or_older_manifest_is_up_to_date() {
        let (source, public_key) = FakeSource::new();
        // 本地 == 清单
        assert!(
            check_for_update_for_platform(&source, "1.0.1", &public_key, "windows-x86_64")
                .await
                .unwrap()
                .is_none()
        );
        // 本地 > 清单：P2 —— 必须是「已是最新」，不是 DowngradeRejected
        assert!(
            check_for_update_for_platform(&source, "1.1.0", &public_key, "windows-x86_64")
                .await
                .unwrap()
                .is_none(),
            "清单比本地旧时检查更新必须返回「已是最新」，不能报错"
        );
        let offer = check_for_update_for_platform(&source, "1.0.0", &public_key, "windows-x86_64")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.manifest.version, "1.0.1");
    }

    #[tokio::test]
    async fn checks_and_stages_signed_update() {
        let (source, public_key) = FakeSource::new();
        let offer = check_for_update_for_platform(&source, "1.0.0", &public_key, "windows-x86_64")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(offer.manifest.version, "1.0.1");
        let root = tempfile::tempdir().unwrap();
        let staged = stage_update(&source, &offer, &public_key, root.path())
            .await
            .unwrap();
        assert!(staged.staging_dir.join("halcyon.exe").is_file());
        assert!(staged.staging_dir.join("halcyon-updater.exe").is_file());
        assert!(staged.helper_path.is_file());
    }

    #[tokio::test]
    async fn rejects_tampered_artifact_signature() {
        let (mut source, public_key) = FakeSource::new();
        source.artifact.extend_from_slice(b"tampered");
        let offer = check_for_update_for_platform(&source, "1.0.0", &public_key, "windows-x86_64")
            .await
            .unwrap()
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        assert!(stage_update(&source, &offer, &public_key, root.path())
            .await
            .is_err());
    }
    #[tokio::test]
    async fn stage_rejects_invalid_and_manual_only_offers() {
        let (source, public_key) = FakeSource::new();
        let offer = check_for_update_for_platform(&source, "1.0.0", &public_key, "windows-x86_64")
            .await
            .unwrap()
            .unwrap();
        let root = tempfile::tempdir().unwrap();

        let mut manual = offer.clone();
        manual.artifact.manual_only = true;
        assert!(matches!(
            stage_update(&source, &manual, &public_key, root.path()).await,
            Err(UpdateError::UnsupportedPlatform)
        ));

        let mut missing_asset = offer.clone();
        missing_asset.artifact.url = None;
        assert!(matches!(
            stage_update(&source, &missing_asset, &public_key, root.path()).await,
            Err(UpdateError::InvalidManifest(_))
        ));

        let mut missing_helper = offer.clone();
        missing_helper.artifact.helper = None;
        assert!(matches!(
            stage_update(&source, &missing_helper, &public_key, root.path()).await,
            Err(UpdateError::InvalidManifest(_))
        ));

        let mut wrong_size = offer.clone();
        wrong_size.artifact.size += 1;
        assert!(matches!(
            stage_update(&source, &wrong_size, &public_key, root.path()).await,
            Err(UpdateError::InvalidArchive(_))
        ));

        let mut wrong_helper_hash = offer.clone();
        wrong_helper_hash.artifact.helper.as_mut().unwrap().sha256 = "0".repeat(64);
        assert!(matches!(
            stage_update(&source, &wrong_helper_hash, &public_key, root.path()).await,
            Err(UpdateError::HashMismatch)
        ));

        let envelope = source.fetch_manifest(&offer.release).await.unwrap();
        assert_eq!(
            manifest_from_envelope(&envelope, &public_key)
                .unwrap()
                .version,
            "1.0.1"
        );
    }
    #[tokio::test]
    async fn missing_platform_key_is_no_update() {
        // 缺键容错：清单有效但未包含本平台（如 macOS 清单缺 darwin-*），按「无更新」处理
        let (source, public_key) = FakeSource::new();
        let result = check_for_update_for_platform(&source, "1.0.0", &public_key, "darwin-aarch64")
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "清单缺少本平台键时必须返回「无更新」而非报错"
        );
    }

    #[tokio::test]
    async fn wrapper_reports_no_update_when_release_is_missing() {
        let (mut source, public_key) = FakeSource::new();
        source.has_release = false;
        assert!(check_for_update(&source, "1.0.0", &public_key)
            .await
            .unwrap()
            .is_none());
    }
}
