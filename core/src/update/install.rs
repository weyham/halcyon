use crate::update::verify::{default_install_allowlist, sha256_hex};
use crate::update::UpdateError;
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub fn validate_helper_hash(path: &Path, expected_sha256: &str) -> Result<(), UpdateError> {
    let bytes = fs::read(path).map_err(|error| UpdateError::Internal(error.to_string()))?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        return Err(UpdateError::HashMismatch);
    }
    Ok(())
}

pub fn prepare_rollback(
    app_dir: &Path,
    rollback_dir: &Path,
    names: &HashSet<String>,
) -> Result<Vec<String>, UpdateError> {
    fs::create_dir_all(rollback_dir).map_err(|error| UpdateError::Internal(error.to_string()))?;
    let mut backed_up = Vec::new();
    for name in names {
        validate_relative_name(name)?;
        let source = app_dir.join(name);
        if !source.is_file() {
            continue;
        }
        let destination = rollback_dir.join(name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        fs::copy(&source, &destination)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        backed_up.push(name.clone());
    }
    Ok(backed_up)
}

pub fn apply_staging_files(
    staging_dir: &Path,
    app_dir: &Path,
    rollback_dir: &Path,
    names: &HashSet<String>,
) -> Result<Vec<String>, UpdateError> {
    let mut replaced = Vec::new();
    for name in names {
        validate_relative_name(name)?;
        let source = staging_dir.join(name);
        if !source.is_file() {
            continue;
        }
        let destination = app_dir.join(name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        if destination.exists() {
            let backup = rollback_dir.join(name);
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| UpdateError::Internal(error.to_string()))?;
            }
            if backup.exists() {
                fs::remove_file(&backup)
                    .map_err(|error| UpdateError::Internal(error.to_string()))?;
            }
            fs::rename(&destination, &backup)
                .map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        fs::rename(&source, &destination)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
        replaced.push(name.clone());
    }
    Ok(replaced)
}

pub fn restore_rollback(
    app_dir: &Path,
    rollback_dir: &Path,
    names: &[String],
) -> Result<(), UpdateError> {
    for name in names {
        validate_relative_name(name)?;
        let source = rollback_dir.join(name);
        if !source.is_file() {
            continue;
        }
        let destination = app_dir.join(name);
        if destination.exists() {
            fs::remove_file(&destination)
                .map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
        }
        fs::rename(&source, &destination)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
    }
    Ok(())
}

pub fn cleanup_staging(staging_dir: &Path) -> Result<(), UpdateError> {
    if staging_dir.exists() {
        fs::remove_dir_all(staging_dir)
            .map_err(|error| UpdateError::Internal(error.to_string()))?;
    }
    Ok(())
}

/// Clean only the completed journal's current-version staging directory.
/// Return false while the helper is still completing; failed journals are retained.
pub fn cleanup_completed_update_once(
    app_dir: &Path,
    current_version: &str,
) -> Result<bool, String> {
    use crate::update::journal::{JournalState, UpdateJournal};

    let journal_path = app_dir.join("updates").join("halcyon-update.journal.json");
    if !journal_path.exists() {
        // 没有 journal = 没有在途更新：顺手清掉「无归属」的历史残留
        // （陈旧的 readiness 标记、早期版本遗留的 rollback\<版本>）。
        sweep_orphan_update_leftovers(app_dir)?;
        return Ok(true);
    }
    let journal = UpdateJournal::load(&journal_path).map_err(|error| error.to_string())?;
    if journal.state == JournalState::Failed {
        return Ok(true);
    }
    if journal.state != JournalState::Completed {
        return Ok(false);
    }
    if journal.to_version != current_version {
        return Ok(true);
    }

    let actual_app = app_dir.canonicalize().map_err(|error| error.to_string())?;
    let recorded_app = Path::new(&journal.app_dir)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if actual_app != recorded_app {
        return Err("journal app 目录与运行目录不匹配".into());
    }
    let token = uuid::Uuid::parse_str(&journal.readiness_token)
        .map_err(|error| format!("journal readiness token 无效：{error}"))?;
    let expected_staging = app_dir
        .join("updates")
        .join("staging")
        .join(current_version);
    let recorded_staging = Path::new(&journal.staging_dir);
    if expected_staging.exists() {
        let actual_staging = expected_staging
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let recorded_staging = recorded_staging
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if actual_staging != recorded_staging || !actual_staging.starts_with(&actual_app) {
            return Err("journal staging 不在当前 app 的版本目录内".into());
        }
        fs::remove_dir_all(&expected_staging).map_err(|error| error.to_string())?;
    } else if recorded_staging != expected_staging {
        return Err("journal staging 路径与当前版本不匹配".into());
    }
    let marker = app_dir
        .join("updates")
        .join("readiness")
        .join(format!("{token}.json"));
    if marker.exists() {
        fs::remove_file(marker).map_err(|error| error.to_string())?;
    }
    fs::remove_file(journal_path).map_err(|error| error.to_string())?;
    // 收尾：成功更新后彻底清除全部临时状态（含本次 rollback——旧版本可从
    // GitHub Releases 重新下载，本地无需保留）。失败 journal 不走这里，原样保留。
    let recorded_rollback = Path::new(&journal.rollback_dir);
    if recorded_rollback.exists() {
        let actual_rollback = recorded_rollback
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if !actual_rollback.starts_with(&actual_app) {
            return Err("journal rollback 不在当前 app 目录内".into());
        }
        fs::remove_dir_all(&actual_rollback).map_err(|error| error.to_string())?;
    }
    // 本次收尾之外，再清一次「无归属」残留（例如更早版本留下的 rollback\<版本>）。
    sweep_orphan_update_leftovers(app_dir)?;
    let lock = app_dir.join(".update.lock");
    if lock.exists() {
        fs::remove_file(&lock).map_err(|error| error.to_string())?;
    }
    Ok(true)
}

/// 清扫「没有 journal 归属」的历史残留。
///
/// - `updates\readiness\*.json`：只有在带 `--readiness-token` 启动（= 紧跟一次更新）
///   时才写入，正常由收尾逻辑删掉；历史上只删了 journal 里记着的那一个 token，
///   更早的标记会留下。没有 journal 时它们都是无主残留。
/// - `updates\rollback\<版本>\`：现行规则是「成功后整棵清除、失败保留」，
///   早期版本（现行规则之前）留下的 rollback 目录没有 journal 归属。
///
/// **只在确认没有在途更新时调用**（无 journal，或本次 journal 已 Completed 且
/// `to_version == 当前版本`）。失败 journal 的现场——staging、rollback、readiness——
/// 一律保留，绝不走这里。
fn sweep_orphan_update_leftovers(app_dir: &Path) -> Result<(), String> {
    let updates = app_dir.join("updates");
    let readiness = updates.join("readiness");
    if readiness.is_dir() {
        for entry in fs::read_dir(&readiness).map_err(|error| error.to_string())? {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path.is_file() {
                fs::remove_file(&path).map_err(|error| error.to_string())?;
            }
        }
    }
    let rollback = updates.join("rollback");
    if rollback.is_dir() {
        for entry in fs::read_dir(&rollback).map_err(|error| error.to_string())? {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path.is_dir() {
                fs::remove_dir_all(&path).map_err(|error| error.to_string())?;
            }
        }
    }
    for dir in [updates.join("staging"), readiness, rollback, updates] {
        let empty = dir.is_dir()
            && dir
                .read_dir()
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
        if empty {
            fs::remove_dir(&dir).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

pub fn write_readiness_after_startup(
    app_dir: &Path,
    token: &str,
    version: &str,
    initialized: bool,
    requires_webdav: bool,
    webdav_ok: bool,
) -> Result<PathBuf, UpdateError> {
    if !initialized {
        return Err(UpdateError::Internal("核心运行时尚未初始化完成".into()));
    }
    if requires_webdav && !webdav_ok {
        return Err(UpdateError::Internal("自动启动 WebDAV 尚未成功".into()));
    }
    write_readiness_marker(app_dir, token, version)
}

pub fn readiness_path(app_dir: &Path, token: &str) -> PathBuf {
    app_dir
        .join("updates")
        .join("readiness")
        .join(format!("{token}.json"))
}

pub fn write_readiness_marker(
    app_dir: &Path,
    token: &str,
    version: &str,
) -> Result<PathBuf, UpdateError> {
    let path = readiness_path(app_dir, token);
    let parent = path
        .parent()
        .ok_or_else(|| UpdateError::Internal("readiness 路径没有父目录".into()))?;
    fs::create_dir_all(parent).map_err(|error| UpdateError::Internal(error.to_string()))?;
    let value = serde_json::json!({
        "protocol": 1,
        "version": version,
        "ready": true,
    });
    fs::write(
        &path,
        serde_json::to_vec_pretty(&value)
            .map_err(|error| UpdateError::Internal(error.to_string()))?,
    )
    .map_err(|error| UpdateError::Internal(error.to_string()))?;
    Ok(path)
}

pub fn install_allowlist(helper_path: Option<&str>) -> HashSet<String> {
    default_install_allowlist(helper_path)
}

fn validate_relative_name(name: &str) -> Result<(), UpdateError> {
    let path = Path::new(name);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(UpdateError::InvalidArchive(format!("路径越界: {name}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::journal::{JournalState, UpdateJournal};

    fn completed_journal_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("app");
        let staging = app.join("updates/staging/1.0.2");
        let rollback = app.join("updates/rollback/1.0.1");
        fs::create_dir_all(&staging).unwrap();
        fs::create_dir_all(&rollback).unwrap();
        fs::write(staging.join("halcyon-updater.exe"), b"helper").unwrap();
        fs::write(rollback.join("halcyon.exe"), b"old").unwrap();
        let journal_path = app.join("updates/halcyon-update.journal.json");
        let token = uuid::Uuid::new_v4();
        let marker = app.join("updates/readiness").join(format!("{token}.json"));
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(marker, b"ready").unwrap();
        let mut journal = UpdateJournal::new(
            "1.0.1",
            "1.0.2",
            &app,
            &staging,
            &rollback,
            &staging.join("halcyon-updater.exe"),
            0,
            token.to_string(),
        );
        journal.transition(JournalState::Completed).unwrap();
        journal.save(&journal_path).unwrap();
        (root, app, journal_path)
    }

    #[test]
    fn completed_cleanup_removes_everything_including_rollback() {
        let (_root, app, journal_path) = completed_journal_fixture();
        let journal = UpdateJournal::load(&journal_path).unwrap();
        let marker = app
            .join("updates/readiness")
            .join(format!("{}.json", journal.readiness_token));
        write(&app.join(".update.lock"), "lock");
        assert!(cleanup_completed_update_once(&app, "1.0.2").unwrap());
        assert!(!journal_path.exists());
        assert!(!marker.exists());
        assert!(!app.join("updates/staging/1.0.2").exists());
        assert!(!app.join(".update.lock").exists(), "更新锁文件应清理");
        assert!(!app.join("updates/staging").exists(), "空 staging 壳应清理");
        assert!(
            !app.join("updates/readiness").exists(),
            "空 readiness 壳应清理"
        );
        assert!(
            !app.join("updates").exists(),
            "成功后 updates 目录应整体清除（rollback 可重新从 Release 下载）"
        );
    }

    #[test]
    fn completed_cleanup_refuses_path_outside_app_without_deleting_anything() {
        let (root, app, journal_path) = completed_journal_fixture();
        let outside = root.path().join("important");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        let mut journal = UpdateJournal::load(&journal_path).unwrap();
        journal.staging_dir = outside.to_string_lossy().into_owned();
        journal.save(&journal_path).unwrap();
        assert!(cleanup_completed_update_once(&app, "1.0.2").is_err());
        assert!(outside.join("keep.txt").exists());
        assert!(app.join("updates/staging/1.0.2").exists());
        assert!(journal_path.exists());
    }

    #[test]
    fn failed_cleanup_preserves_failed_journal_and_staging() {
        let (_root, app, journal_path) = completed_journal_fixture();
        let mut journal = UpdateJournal::load(&journal_path).unwrap();
        journal.transition(JournalState::Failed).unwrap();
        journal.save(&journal_path).unwrap();
        // 再放两份「历史残留」进去：失败现场必须原样保留，不许顺手清扫
        fs::create_dir_all(app.join("updates/rollback/1.0.2")).unwrap();
        fs::write(app.join("updates/rollback/1.0.2/halcyon.exe"), b"old").unwrap();
        let stale = app.join("updates/readiness/deadbeef.json");
        fs::write(&stale, b"stale").unwrap();
        assert!(cleanup_completed_update_once(&app, "1.0.2").unwrap());
        assert!(journal_path.exists());
        assert!(app.join("updates/staging/1.0.2").exists());
        assert!(app.join("updates/rollback/1.0.1").exists());
        assert!(
            app.join("updates/rollback/1.0.2").exists(),
            "失败现场不得清扫历史 rollback"
        );
        assert!(stale.exists(), "失败现场不得清扫 readiness 标记");
    }

    #[test]
    fn no_journal_sweep_removes_stale_readiness_and_historical_rollback() {
        // 场景：没有 journal，但留着 readiness\<token>.json 与历史 rollback\<版本>\
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("app");
        let stale = app.join("updates/readiness/1872eaad-b6c5-4878-8254-a01515ee526c.json");
        write(&stale, "ready");
        let historical = app.join("updates/rollback/1.0.2");
        fs::create_dir_all(&historical).unwrap();
        fs::write(historical.join("halcyon.exe"), vec![0u8; 64]).unwrap();
        assert!(cleanup_completed_update_once(&app, "1.0.8").unwrap());
        assert!(!stale.exists(), "陈旧 readiness 标记应被清掉");
        assert!(!historical.exists(), "历史 rollback 目录应被清掉");
        assert!(
            !app.join("updates").exists(),
            "清空后的 updates 空壳应整体移除"
        );
    }

    fn write(path: &Path, value: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, value).unwrap();
    }

    #[test]
    fn replaces_only_allowlisted_files_and_keeps_data() {
        let root = crate::test_util::TempWorkDir::new("install");
        let app = root.join("app");
        let staging = app.join("updates/staging/1.0.1");
        let rollback = app.join("updates/rollback/1.0.0");
        write(&app.join("halcyon.exe"), "old");
        write(&app.join("data/config.json"), "user-data");
        write(&staging.join("halcyon.exe"), "new");
        write(&staging.join("VERSION.txt"), "1.0.1");

        let names = install_allowlist(None);
        let backed_up = prepare_rollback(&app, &rollback, &names).unwrap();
        assert!(backed_up.contains(&"halcyon.exe".to_string()));
        let applied = apply_staging_files(&staging, &app, &rollback, &names).unwrap();
        assert!(applied.contains(&"halcyon.exe".to_string()));
        assert!(applied.contains(&"VERSION.txt".to_string()));
        assert_eq!(fs::read_to_string(app.join("halcyon.exe")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(app.join("data/config.json")).unwrap(),
            "user-data"
        );

        restore_rollback(&app, &rollback, &backed_up).unwrap();
        assert_eq!(fs::read_to_string(app.join("halcyon.exe")).unwrap(), "old");
    }

    #[test]
    fn rejects_path_traversal_names() {
        assert!(validate_relative_name("../halcyon.exe").is_err());
        assert!(validate_relative_name("halcyon.exe").is_ok());
    }
    #[test]
    fn readiness_gate_requires_init_and_optional_webdav() {
        let root = crate::test_util::TempWorkDir::new("readiness");
        assert!(write_readiness_after_startup(&root, "token", "1.0.1", true, false, false).is_ok());
        assert!(readiness_path(&root, "token").is_file());

        let root = crate::test_util::TempWorkDir::new("readiness");
        assert!(
            write_readiness_after_startup(&root, "token", "1.0.1", false, false, false).is_err()
        );
        assert!(!readiness_path(&root, "token").exists());

        let root = crate::test_util::TempWorkDir::new("readiness");
        assert!(write_readiness_after_startup(&root, "token", "1.0.1", true, true, false).is_err());
        assert!(!readiness_path(&root, "token").exists());
    }
}
