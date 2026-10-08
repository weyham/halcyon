//! 运行数据目录分离（Velopack 前置）：Windows 四级判定 + legacy 迁移。
//!
//! 四级优先级判定（自实现）：
//! 1. `--data-dir <path>` 命令行参数
//! 2. `HALCYON_DATA_DIR` 环境变量
//! 3. exe 旁 `data\` 目录存在 → portable
//! 4. `%APPDATA%\Halcyon`（安装版）
//!
//! macOS 不走此判定，由调用方直接返回固定目录。
//!
//! legacy 迁移：旧便携用户 exe 旁只有 `config.json`、无 `data\`。严格按四级
//! 会误判成安装版。resolve 后检测并自动迁移到 exe 旁 `data\`（旧文件保留）。

use std::path::{Path, PathBuf};

/// 数据目录解析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct DataDirResult {
    pub path: PathBuf,
    /// true = exe 旁 data\（便携模式）；false = 安装版 / macOS 固定目录。
    pub is_portable: bool,
    /// legacy 迁移是否发生（旧 config.json 被复制到 data\config.json）。
    pub migrated: bool,
}

/// 纯函数：四级优先级判定 + legacy 迁移；不涉及文件系统读取（由调用方提供状态）。
///
/// - `cli_data_dir`: `--data-dir` 命令行参数值
/// - `env_data_dir`: `HALCYON_DATA_DIR` 环境变量值
/// - `exe_dir`: 当前 exe 所在目录
/// - `roaming_dir`: `%APPDATA%\Halcyon`（安装版目录）
/// - `exe_side_data_exists`: exe 旁 `data\` 目录是否存在
/// - `exe_side_config_exists`: exe 旁旧版 `config.json` 是否存在
/// - `data_config_exists`: exe 旁 `data\config.json` 是否存在
pub fn select_data_dir(
    cli_data_dir: Option<&str>,
    env_data_dir: Option<&str>,
    exe_dir: &Path,
    roaming_dir: &Path,
    exe_side_data_exists: bool,
    exe_side_config_exists: bool,
    data_config_exists: bool,
) -> DataDirResult {
    // 1. 命令行参数
    if let Some(dir) = cli_data_dir.filter(|s| !s.trim().is_empty()) {
        return DataDirResult {
            path: PathBuf::from(dir),
            is_portable: false,
            migrated: false,
        };
    }
    // 2. 环境变量
    if let Some(dir) = env_data_dir.filter(|s| !s.trim().is_empty()) {
        return DataDirResult {
            path: PathBuf::from(dir),
            is_portable: false,
            migrated: false,
        };
    }
    // 3. exe 旁 data\ 存在或旧 config.json 存在 → portable
    // 统一分支覆盖四种场景：
    //   全新便携（data 有、无任何 config）→ portable 不迁移
    //   legacy 旧版（data 无、exe config 有）→ portable + 迁移
    //   覆盖解压（data 有 + exe config 有 + data config 无）→ portable + 迁移
    //   既有便携（data 有 + data config 有）→ portable 不迁移
    let data_dir = exe_dir.join("data");
    if exe_side_data_exists || exe_side_config_exists {
        return DataDirResult {
            path: data_dir,
            is_portable: true,
            migrated: exe_side_config_exists && !data_config_exists,
        };
    }
    // 4. 安装版默认目录
    DataDirResult {
        path: roaming_dir.to_path_buf(),
        is_portable: false,
        migrated: false,
    }
}

/// 运行时解析：读取命令行 / 环境变量 / 文件系统状态，执行 legacy 迁移。
/// macOS / Linux 直接返回固定目录。
/// From `std::env::args` parse `--data-dir <path>` or `--data-dir=<path>`;
/// ignore empty strings; do not match other startup args (`--autostart` etc).
pub fn parse_cli_data_dir(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--data-dir" {
            if let Some(val) = iter.next() {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        } else if let Some(val) = arg.strip_prefix("--data-dir=") {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn resolve(cli_data_dir: Option<&str>) -> DataDirResult {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let path = home
            .join("Library")
            .join("Application Support")
            .join("Halcyon");
        return DataDirResult {
            path,
            is_portable: false,
            migrated: false,
        };
    }
    #[cfg(not(target_os = "macos"))]
    {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
            .unwrap_or_default();
        let roaming_dir = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Halcyon");
        let env_data_dir = std::env::var("HALCYON_DATA_DIR").ok();
        let cli = cli_data_dir.map(String::from);

        let exe_side_data = exe_dir.join("data");
        let exe_side_config = exe_dir.join("config.json");
        let data_config = exe_side_data.join("config.json");

        let mut result = select_data_dir(
            cli.as_deref(),
            env_data_dir.as_deref(),
            &exe_dir,
            &roaming_dir,
            exe_side_data.is_dir(),
            exe_side_config.is_file(),
            data_config.is_file(),
        );

        // legacy 迁移：实际执行文件操作
        if result.migrated {
            let data_dir = exe_dir.join("data");
            let _ = std::fs::create_dir_all(&data_dir);
            let src = exe_dir.join("config.json");
            let dst = data_dir.join("config.json");
            if src.is_file() && !dst.is_file() {
                match std::fs::copy(&src, &dst) {
                    Ok(_) => {
                        log::info!(
                            "legacy config migrated: {} -> {}",
                            src.display(),
                            dst.display()
                        );
                    }
                    Err(e) => {
                        log::error!("legacy config migration failed: {e}");
                        // 迁移失败时回退到安装版目录
                        result = DataDirResult {
                            path: roaming_dir,
                            is_portable: false,
                            migrated: false,
                        };
                    }
                }
            }
        }

        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn exe_dir() -> PathBuf {
        PathBuf::from("C:\\app")
    }
    fn roaming() -> PathBuf {
        PathBuf::from("C:\\Users\\u\\AppData\\Roaming\\Halcyon")
    }

    // ---- 四场景判定矩阵 ----

    #[test]
    fn fresh_portable_data_marker_only_is_portable() {
        // 全新便携：data\ 存在（含 README）、无任何 config
        let r = select_data_dir(None, None, &exe_dir(), &roaming(), true, false, false);
        assert_eq!(r.path, exe_dir().join("data"));
        assert!(r.is_portable);
        assert!(!r.migrated);
    }

    #[test]
    fn legacy_old_version_no_data_dir() {
        // legacy 旧版：无 data\、exe 旁有旧 config.json
        let r = select_data_dir(None, None, &exe_dir(), &roaming(), false, true, false);
        assert_eq!(r.path, exe_dir().join("data"));
        assert!(r.is_portable);
        assert!(r.migrated);
    }

    #[test]
    fn overwrite_extract_data_exists_old_config_exists_no_data_config() {
        // 覆盖解压：data\ 存在 + 旧 config 存在 + data config 缺失
        let r = select_data_dir(None, None, &exe_dir(), &roaming(), true, true, false);
        assert_eq!(r.path, exe_dir().join("data"));
        assert!(r.is_portable);
        assert!(r.migrated);
    }

    #[test]
    fn existing_portable_data_and_config() {
        // 既有便携：data\ 存在 + data config 存在
        let r = select_data_dir(None, None, &exe_dir(), &roaming(), true, false, true);
        assert_eq!(r.path, exe_dir().join("data"));
        assert!(r.is_portable);
        assert!(!r.migrated);
    }

    #[test]
    fn priority_4_installed_when_no_portable_markers() {
        let r = select_data_dir(None, None, &exe_dir(), &roaming(), false, false, false);
        assert_eq!(r.path, roaming());
        assert!(!r.is_portable);
        assert!(!r.migrated);
    }

    #[test]
    fn priority_1_cli_arg() {
        let r = select_data_dir(
            Some("D:\\custom"),
            Some("E:\\env"),
            &exe_dir(),
            &roaming(),
            true,
            true,
            true,
        );
        assert_eq!(r.path, Path::new("D:\\custom"));
        assert!(!r.is_portable);
        assert!(!r.migrated);
    }

    #[test]
    fn priority_2_env_var() {
        let r = select_data_dir(
            None,
            Some("E:\\env"),
            &exe_dir(),
            &roaming(),
            true,
            true,
            true,
        );
        assert_eq!(r.path, Path::new("E:\\env"));
        assert!(!r.is_portable);
    }

    // ---- parse_cli_data_dir ----

    #[test]
    fn parse_cli_data_dir_space_form() {
        let args = vec![
            "halcyon".to_string(),
            "--data-dir".to_string(),
            "D:\\custom".to_string(),
        ];
        assert_eq!(parse_cli_data_dir(&args), Some("D:\\custom".to_string()));
    }

    #[test]
    fn parse_cli_data_dir_equals_form() {
        let args = vec!["halcyon".to_string(), "--data-dir=D:\\custom".to_string()];
        assert_eq!(parse_cli_data_dir(&args), Some("D:\\custom".to_string()));
    }

    #[test]
    fn parse_cli_data_dir_empty_value_ignored() {
        let args = vec![
            "halcyon".to_string(),
            "--data-dir".to_string(),
            "".to_string(),
        ];
        assert_eq!(parse_cli_data_dir(&args), None);
    }

    #[test]
    fn parse_cli_data_dir_ignores_other_args() {
        let args = vec![
            "halcyon".to_string(),
            "--autostart".to_string(),
            "--data-dir".to_string(),
            "D:\\custom".to_string(),
        ];
        assert_eq!(parse_cli_data_dir(&args), Some("D:\\custom".to_string()));
    }

    #[test]
    fn parse_cli_data_dir_none_when_absent() {
        let args = vec!["halcyon".to_string(), "--autostart".to_string()];
        assert_eq!(parse_cli_data_dir(&args), None);
    }
}
