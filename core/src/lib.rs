pub mod appserver;
pub mod auto_repair;
pub mod autostart;
pub mod balance;
pub mod config;
pub mod data_dir;
pub mod logging;
pub mod model_repair;
pub mod repair;
pub mod rewrite;
pub mod roots_repair;
pub mod server;
pub mod unified_scan;
pub mod update;
pub mod velopack_runtime;

#[cfg(test)]
pub(crate) mod test_util {
    use std::ops::Deref;
    use std::path::PathBuf;

    /// 测试临时目录守卫：测试成功（未 panic）即删除，失败保留现场
    ///（与 updater journal 的「失败保留」策略一致）。
    pub(crate) struct TempWorkDir(PathBuf);

    impl TempWorkDir {
        pub(crate) fn new(tag: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "halcyon-{tag}-{}",
                uuid::Uuid::new_v4()
            )))
        }
    }

    impl Deref for TempWorkDir {
        type Target = PathBuf;
        fn deref(&self) -> &PathBuf {
            &self.0
        }
    }

    impl Drop for TempWorkDir {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}