use crate::update::{UpdateError, UpdateManifest};
use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::ZipArchive;

#[derive(Clone, Copy)]
pub struct SignedPayload<'a> {
    pub public_key: &'a str,
    pub data: &'a [u8],
    pub signature: &'a str,
}

impl<'a> SignedPayload<'a> {
    pub fn new(public_key: &'a str, data: &'a [u8], signature: &'a str) -> Self {
        Self {
            public_key,
            data,
            signature,
        }
    }

    pub fn verify(self) -> Result<(), UpdateError> {
        verify_minisign(self.public_key, self.data, self.signature)
    }
}

pub fn verify_minisign(
    public_key_b64: &str,
    data: &[u8],
    signature_text: &str,
) -> Result<(), UpdateError> {
    if public_key_b64.trim().is_empty() {
        return Err(UpdateError::InvalidSignature("未配置更新公钥".into()));
    }
    let public_key = PublicKey::from_base64(public_key_b64.trim())
        .map_err(|error| UpdateError::InvalidSignature(error.to_string()))?;
    let signature = Signature::decode(signature_text)
        .map_err(|error| UpdateError::InvalidSignature(error.to_string()))?;
    public_key
        .verify(data, &signature, false)
        .map_err(|error| UpdateError::InvalidSignature(error.to_string()))
}

pub fn verify_manifest(
    public_key_b64: &str,
    manifest_bytes: &[u8],
    signature_text: &str,
) -> Result<UpdateManifest, UpdateError> {
    SignedPayload::new(public_key_b64, manifest_bytes, signature_text).verify()?;
    let manifest = UpdateManifest::parse(manifest_bytes)?;
    manifest.validate()?;
    Ok(manifest)
}

pub fn verify_artifact(
    public_key_b64: &str,
    artifact_bytes: &[u8],
    signature_text: &str,
) -> Result<(), UpdateError> {
    SignedPayload::new(public_key_b64, artifact_bytes, signature_text).verify()
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

pub fn verify_sha256(data: &[u8], expected_hex: &str) -> Result<(), UpdateError> {
    let actual = sha256_hex(data);
    if !actual.eq_ignore_ascii_case(expected_hex.trim()) {
        return Err(UpdateError::HashMismatch);
    }
    Ok(())
}

pub fn verify_size(data_len: usize, expected_size: u64) -> Result<(), UpdateError> {
    if expected_size != 0 && data_len as u64 != expected_size {
        return Err(UpdateError::InvalidArchive(format!(
            "下载大小 {} 与清单大小 {} 不一致",
            data_len, expected_size
        )));
    }
    Ok(())
}

pub fn default_install_allowlist(helper_path: Option<&str>) -> HashSet<String> {
    let mut allowed = HashSet::from([
        "halcyon.exe".to_string(),
        "WebView2Loader.dll".to_string(),
        "VERSION.txt".to_string(),
        "LICENSE.txt".to_string(),
    ]);
    if let Some(helper_path) = helper_path {
        allowed.insert(helper_path.replace('\\', "/"));
    }
    allowed
}

#[derive(Clone, Copy)]
pub struct ZipLimits {
    pub max_entries: usize,
    pub max_file_size: u64,
    pub max_total_size: u64,
    pub max_compression_ratio: u64,
}

impl Default for ZipLimits {
    fn default() -> Self {
        Self {
            max_entries: 64,
            max_file_size: 256 * 1024 * 1024,
            max_total_size: 512 * 1024 * 1024,
            max_compression_ratio: 1000,
        }
    }
}

pub fn extract_zip_safe(
    archive_bytes: &[u8],
    destination: &Path,
    allowed_names: &HashSet<String>,
) -> Result<Vec<PathBuf>, UpdateError> {
    extract_zip_safe_with_limits(
        archive_bytes,
        destination,
        allowed_names,
        ZipLimits::default(),
    )
}

pub fn extract_zip_safe_with_limits(
    archive_bytes: &[u8],
    destination: &Path,
    allowed_names: &HashSet<String>,
    limits: ZipLimits,
) -> Result<Vec<PathBuf>, UpdateError> {
    let mut archive = ZipArchive::new(Cursor::new(archive_bytes))
        .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
    std::fs::create_dir_all(destination)
        .map_err(|error| UpdateError::Internal(error.to_string()))?;
    if archive.len() > limits.max_entries {
        return Err(UpdateError::InvalidArchive("更新包条目数超过限制".into()));
    }
    let mut extracted = Vec::new();
    let mut total_size = 0u64;

    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
        let Some(relative) = file.enclosed_name() else {
            return Err(UpdateError::InvalidArchive(format!(
                "zip 条目越界: {}",
                file.name()
            )));
        };
        if relative.is_absolute() || has_parent_component(&relative) {
            return Err(UpdateError::InvalidArchive(format!(
                "zip 条目包含非法路径: {}",
                file.name()
            )));
        }
        let normalized = normalize_zip_name(&relative);
        if !allowed_names.contains(&normalized) {
            return Err(UpdateError::InvalidArchive(format!(
                "zip 条目不在白名单: {normalized}"
            )));
        }

        if file.is_dir() {
            return Err(UpdateError::InvalidArchive("更新包不允许目录条目".into()));
        }
        if file
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(UpdateError::InvalidArchive("更新包不允许符号链接".into()));
        }
        if file.size() > limits.max_file_size {
            return Err(UpdateError::InvalidArchive("单文件解压大小超过限制".into()));
        }
        total_size = total_size.saturating_add(file.size());
        if total_size > limits.max_total_size {
            return Err(UpdateError::InvalidArchive("总解压大小超过限制".into()));
        }
        if file.compressed_size() > 0
            && file.size() / file.compressed_size() > limits.max_compression_ratio
        {
            return Err(UpdateError::InvalidArchive("压缩比超过限制".into()));
        }

        let output = destination.join(&relative);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        let mut bytes = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut bytes)
            .map_err(|error| UpdateError::InvalidArchive(error.to_string()))?;
        let mut output_file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&output)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        output_file
            .write_all(&bytes)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        extracted.push(output);
    }

    if extracted.is_empty() {
        return Err(UpdateError::InvalidArchive("更新包为空".into()));
    }
    Ok(extracted)
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn normalize_zip_name(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    #[test]
    fn temporary_minisign_key_verifies_manifest() {
        let key_pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let manifest = br#"{"schema":1,"protocol":1,"version":"1.0.1","platforms":{}}"#;
        let signature =
            minisign::sign(None, &key_pair.sk, Cursor::new(manifest), None, None).unwrap();
        let signature_text = String::from(signature);
        let public_key = key_pair.pk.to_base64();
        assert!(verify_minisign(&public_key, manifest, &signature_text).is_ok());
        assert!(verify_minisign(&public_key, b"tampered", &signature_text).is_err());
    }

    #[test]
    fn sha256_and_size_checks_are_enforced() {
        let data = b"halcyon";
        let digest = sha256_hex(data);
        assert!(verify_sha256(data, &digest).is_ok());
        assert!(verify_sha256(data, &"0".repeat(64)).is_err());
        assert!(verify_size(data.len(), data.len() as u64).is_ok());
        assert!(verify_size(data.len(), data.len() as u64 + 1).is_err());
    }

    #[test]
    fn zip_slip_and_allowlist_are_rejected() {
        let mut zip = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
            writer
                .start_file("../halcyon.exe", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"bad").unwrap();
            writer.finish().unwrap();
        }
        let root = std::env::temp_dir().join(format!("halcyon-zip-{}", uuid::Uuid::new_v4()));
        let allowed = default_install_allowlist(None);
        assert!(extract_zip_safe(&zip, &root, &allowed).is_err());

        let mut zip = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
            writer
                .start_file("not-allowed.txt", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"bad").unwrap();
            writer.finish().unwrap();
        }
        assert!(extract_zip_safe(&zip, &root, &allowed).is_err());
    }
    #[test]
    fn rejects_missing_key_and_malformed_signature() {
        assert!(verify_minisign("", b"data", "bad").is_err());
        assert!(verify_minisign("not-base64", b"data", "bad").is_err());
    }

    #[test]
    fn extracts_allowed_file_and_rejects_empty_archive() {
        let root = std::env::temp_dir().join(format!("halcyon-zip-ok-{}", uuid::Uuid::new_v4()));
        let mut zip = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
            writer
                .start_file("halcyon.exe", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"app").unwrap();
            writer.finish().unwrap();
        }
        let allowed = default_install_allowlist(Some("helper.exe"));
        let files = extract_zip_safe(&zip, &root, &allowed).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(root.join("halcyon.exe")).unwrap(), b"app");
        let empty_zip = [];
        assert!(extract_zip_safe(&empty_zip, &root, &allowed).is_err());
    }
    #[test]
    fn zip_resource_limits_are_enforced() {
        let root =
            std::env::temp_dir().join(format!("halcyon-zip-limits-{}", uuid::Uuid::new_v4()));
        let allowed = default_install_allowlist(None);
        let mut zip = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
            for name in ["halcyon.exe", "VERSION.txt"] {
                writer
                    .start_file(name, SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(b"x").unwrap();
            }
            writer.finish().unwrap();
        }
        let limits = ZipLimits {
            max_entries: 1,
            ..ZipLimits::default()
        };
        assert!(extract_zip_safe_with_limits(&zip, &root, &allowed, limits).is_err());

        let mut zip = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
            writer
                .start_file("halcyon.exe", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"too-large").unwrap();
            writer.finish().unwrap();
        }
        let limits = ZipLimits {
            max_file_size: 1,
            ..ZipLimits::default()
        };
        assert!(extract_zip_safe_with_limits(&zip, &root, &allowed, limits).is_err());

        let mut zip = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
            writer
                .start_file("halcyon.exe", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&vec![0u8; 100_000]).unwrap();
            writer.finish().unwrap();
        }
        let limits = ZipLimits {
            max_compression_ratio: 2,
            ..ZipLimits::default()
        };
        assert!(extract_zip_safe_with_limits(&zip, &root, &allowed, limits).is_err());

        let mut zip = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut zip));
            writer
                .add_directory("folder/", SimpleFileOptions::default())
                .unwrap();
            writer.finish().unwrap();
        }
        assert!(extract_zip_safe(&zip, &root, &allowed).is_err());
    }
}
