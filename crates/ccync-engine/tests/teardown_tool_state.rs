use ccync_engine::install::run_uninstall;
use std::{
    fs,
    path::Path,
    sync::{Mutex, MutexGuard, OnceLock},
};
use tempfile::TempDir;

fn env_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}

fn set_home(home: &Path) -> EnvRestore {
    let paths = [
        ("HOME", home.to_path_buf()),
        ("USERPROFILE", home.to_path_buf()),
        ("APPDATA", home.join("AppData/Roaming")),
        ("LOCALAPPDATA", home.join("AppData/Local")),
        ("XDG_CONFIG_HOME", home.join("xdg-config")),
        ("XDG_STATE_HOME", home.join("xdg-state")),
        ("XDG_CACHE_HOME", home.join("xdg-cache")),
    ];
    let saved = paths
        .iter()
        .map(|(key, _)| (*key, std::env::var_os(key)))
        .collect();
    for (key, value) in paths {
        unsafe { std::env::set_var(key, value) };
    }
    EnvRestore(saved)
}
#[cfg(unix)]
fn file_identity(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let m = fs::metadata(path).unwrap();
    (m.dev(), m.ino())
}
#[cfg(windows)]
fn file_identity(path: &Path) -> (u32, u64) {
    use std::os::windows::io::AsRawHandle;
    let file = fs::File::open(path).unwrap();
    let mut info = unsafe {
        std::mem::zeroed::<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION>()
    };
    assert_ne!(
        unsafe {
            windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
                file.as_raw_handle() as _,
                &mut info,
            )
        },
        0
    );
    (
        info.dwVolumeSerialNumber,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    )
}

#[test]
fn uninstall_removes_tool_state_but_preserves_lifecycle_lock_and_inputs() {
    let _guard = env_guard();
    let home = TempDir::new().unwrap();
    let _env_restore = set_home(home.path());
    let ccync = home.path().join(".ccync");
    let build = ccync.join("build");
    fs::create_dir_all(build.join("tools/artifacts/snapshot")).unwrap();
    fs::create_dir_all(ccync.join("install-receipts")).unwrap();
    for name in ["config.json", "plugins.json", "AGENTS.md"] {
        fs::write(ccync.join(name), "input").unwrap();
    }
    fs::write(ccync.join("tools.json"), "{}").unwrap();
    fs::write(ccync.join("install-receipts/ccync.json"), "{}").unwrap();
    fs::write(build.join("tools/state.json"), "{}").unwrap();
    fs::write(build.join("tools/artifacts/snapshot/item"), "artifact").unwrap();
    fs::write(build.join("tools/operation.txn.json"), "{}").unwrap();
    fs::write(build.join("lifecycle.lock"), "").unwrap();
    let lock = build.join("lifecycle.lock");
    let before = file_identity(&lock);
    let prior_path = std::env::var_os("PATH");
    unsafe {
        std::env::set_var("PATH", "");
    }
    run_uninstall().unwrap();
    if let Some(value) = prior_path {
        unsafe {
            std::env::set_var("PATH", value);
        }
    }
    assert!(lock.is_file());
    assert_eq!(
        file_identity(&lock),
        before,
        "uninstall must retain the same lock file"
    );
    for name in ["config.json", "plugins.json", "AGENTS.md"] {
        assert!(ccync.join(name).is_file());
    }
    for path in [
        ccync.join("tools.json"),
        ccync.join("install-receipts/ccync.json"),
        build.join("tools/state.json"),
        build.join("tools/artifacts"),
        build.join("tools/operation.txn.json"),
    ] {
        assert!(
            !path.exists(),
            "tool state must be removed: {}",
            path.display()
        );
    }
}
