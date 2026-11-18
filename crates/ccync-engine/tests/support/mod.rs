use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

/// An isolated home that owns the environment lock. `Drop` restores the
/// environment before the lock is released, so the next test never sees a
/// late restore overwrite its own variables.
pub struct IsolatedHome {
    temp: TempDir,
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: MutexGuard<'static, ()>,
}
impl IsolatedHome {
    fn new(lock: MutexGuard<'static, ()>) -> Self {
        let temp = tempfile::tempdir().expect("isolated home");
        let root = temp.path();
        let paths = [
            ("HOME", root.to_path_buf()),
            ("USERPROFILE", root.to_path_buf()),
            ("APPDATA", root.join("AppData/Roaming")),
            ("CCYNC_HOME", root.join(".ccync")),
            ("XDG_CONFIG_HOME", root.join("xdg")),
            ("CODEX_HOME", root.join(".codex")),
            ("CLAUDE_CONFIG_DIR", root.join(".claude")),
        ];
        let prior = paths
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in paths {
            std::env::set_var(key, value);
        }
        for target in [
            ".claude",
            ".codex",
            ".copilot",
            ".config/opencode",
            ".gemini",
        ] {
            std::fs::create_dir_all(root.join(target)).expect("target root");
        }
        Self {
            temp,
            prior,
            _lock: lock,
        }
    }
    pub fn root(&self) -> &Path {
        self.temp.path()
    }
}
impl Drop for IsolatedHome {
    fn drop(&mut self) {
        for (key, value) in self.prior.drain(..) {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
/// Acquire the environment lock and an isolated home. The second tuple
/// element is kept for existing `let (home, _lock)` call sites; the lock
/// itself lives inside `IsolatedHome`.
pub fn isolated() -> (IsolatedHome, ()) {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    (IsolatedHome::new(lock), ())
}
