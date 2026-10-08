//! 一次性本地集成探针：用 FileSource 指向本地 vpk 产物，验证 Velopack
//! locator + check + download 链路能否在 portable 布局下跑通。
//! 用法：cargo run -p halcyon-core --example velopack_local_probe -- <portable_root> <release_dir>
//!
//! **只在 Windows 上有意义**：`velopack::locator::create_config_from_root_dir()`
//! 在 velopack 1.2.161 里是 `#[cfg(target_os = "windows")]` 的。
//! 但 `cargo test` 默认会编译 examples，所以非 Windows 平台必须留下一个能编过的
//! `main` —— 否则 macOS CI 的 `cargo test -p halcyon-core` 会以
//! `error[E0425]: cannot find function create_config_from_root_dir` 整片失败（J0）。

#[cfg(target_os = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let portable_root = args.get(1).expect("usage: <portable_root> <release_dir>");
    let release_dir = args.get(2).expect("usage: <portable_root> <release_dir>");

    let config = velopack::locator::create_config_from_root_dir(portable_root);
    println!("locator root: {}", config.RootAppDir.display());
    println!("is_portable: {}", config.IsPortable);

    let source = velopack::sources::FileSource::new(release_dir);
    let manager = match velopack::UpdateManager::new(source, None, Some(config)) {
        Ok(m) => m,
        Err(e) => {
            println!("PROBE_FAIL: UpdateManager::new error: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "current_version: {}",
        manager.get_current_version_as_string()
    );
    println!("app_id: {}", manager.get_app_id());
    println!("is_portable: {}", manager.get_is_portable());

    match manager.check_for_updates() {
        Ok(velopack::UpdateCheck::UpdateAvailable(info)) => {
            println!(
                "PROBE_OK: update available -> {}",
                info.TargetFullRelease.Version
            );
            match manager.download_updates(&info, None) {
                Ok(()) => println!("PROBE_OK: download_updates succeeded"),
                Err(e) => println!("PROBE_DOWNLOAD_FAIL: {e}"),
            }
        }
        Ok(velopack::UpdateCheck::NoUpdateAvailable) => {
            println!("PROBE_OK: NoUpdateAvailable (feed read succeeded)");
        }
        Ok(velopack::UpdateCheck::RemoteIsEmpty) => {
            println!("PROBE_OK: RemoteIsEmpty (feed read succeeded)");
        }
        Err(e) => println!("PROBE_CHECK_FAIL: {e}"),
    }
}

/// 非 Windows：探针无意义，Windows-only 的调用已在编译期被整体摘掉。
#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!(
        "velopack_local_probe 只在 Windows 上有意义：\
         velopack::locator::create_config_from_root_dir() 是 Windows-only API。"
    );
}
