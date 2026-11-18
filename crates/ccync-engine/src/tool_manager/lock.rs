use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io,
    marker::PhantomData,
    path::{Path, PathBuf},
};

thread_local! {
    /// Number of live lifecycle guards acquired on this thread. The CLI holds
    /// its guard on the thread that runs the lifecycle command, so the engine
    /// can prove the lock is held without receiving the guard itself.
    static HELD_GUARDS: Cell<usize> = const { Cell::new(0) };
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleLockError {
    #[error("lifecycle lock path is unavailable")]
    PathUnavailable,
    #[error("lifecycle lock I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("restore is pending or its marker is corrupt; resume the matching restore")]
    RestorePending,
}

pub struct LifecycleLock;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestoreMarker {
    pub version: u32,
    pub profile_digest: String,
    pub allowed_keys: Vec<String>,
    pub destinations: Vec<RestoreDestination>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreDestination {
    pub key: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreMarkerState {
    Absent,
    Pending(RestoreMarker),
    Corrupt,
}

pub struct RestoreLock {
    _guard: LifecycleGuard,
    profile_digest: String,
    allowed_keys: Vec<String>,
}

fn marker_path() -> Result<PathBuf, LifecycleLockError> {
    Ok(ccync_foundation::paths::lifecycle_lock_path()
        .ok_or(LifecycleLockError::PathUnavailable)?
        .with_file_name("restore-pending.json"))
}

fn fingerprint(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

pub fn read_restore_marker() -> Result<RestoreMarkerState, LifecycleLockError> {
    let bytes = match fs::read(marker_path()?) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RestoreMarkerState::Absent)
        }
        Err(_) => return Ok(RestoreMarkerState::Corrupt),
    };
    let Ok(marker) = serde_json::from_slice::<RestoreMarker>(&bytes) else {
        return Ok(RestoreMarkerState::Corrupt);
    };
    let mut keys = marker.allowed_keys.clone();
    keys.sort();
    keys.dedup();
    let mut destinations: Vec<_> = marker.destinations.iter().map(|d| d.key.clone()).collect();
    destinations.sort();
    if marker.version != 1
        || !valid_digest(&marker.profile_digest)
        || keys != marker.allowed_keys
        || keys != destinations
        || keys
            .iter()
            .any(|key| key.is_empty() || key.contains('/') || key.contains('\\'))
        || marker.destinations.iter().any(|d| {
            d.before.as_deref().is_some_and(|v| !valid_digest(v))
                || d.after.as_deref().is_some_and(|v| !valid_digest(v))
        })
    {
        return Ok(RestoreMarkerState::Corrupt);
    }
    Ok(RestoreMarkerState::Pending(marker))
}

pub fn check_restore_pending() -> Result<(), LifecycleLockError> {
    match read_restore_marker()? {
        RestoreMarkerState::Absent => Ok(()),
        _ => Err(LifecycleLockError::RestorePending),
    }
}

impl RestoreLock {
    pub fn record_marker(
        &self,
        archive: &Path,
        destinations: &[(&str, Option<&[u8]>, Option<&[u8]>)],
    ) -> Result<(), LifecycleLockError> {
        let digest = fingerprint(&fs::read(archive)?);
        if digest != self.profile_digest {
            return Err(LifecycleLockError::RestorePending);
        }
        self.record_destinations(destinations)
    }

    pub fn record_destinations(
        &self,
        destinations: &[(&str, Option<&[u8]>, Option<&[u8]>)],
    ) -> Result<(), LifecycleLockError> {
        let mut rows: Vec<_> = destinations
            .iter()
            .map(|(key, before, after)| RestoreDestination {
                key: (*key).to_owned(),
                before: before.map(fingerprint),
                after: after.map(fingerprint),
            })
            .collect();
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        let keys: Vec<_> = rows.iter().map(|row| row.key.clone()).collect();
        if keys != self.allowed_keys
            || !matches!(read_restore_marker()?, RestoreMarkerState::Absent)
        {
            return Err(LifecycleLockError::RestorePending);
        }
        let marker = RestoreMarker {
            version: 1,
            profile_digest: self.profile_digest.clone(),
            allowed_keys: keys,
            destinations: rows,
        };
        let bytes = serde_json::to_vec(&marker).map_err(io::Error::other)?;
        ccync_foundation::platform::atomic_write_bytes(&marker_path()?, &bytes)
            .map_err(io::Error::other)?;
        Ok(())
    }

    pub fn clear_marker(&self) -> Result<(), LifecycleLockError> {
        match read_restore_marker()? {
            RestoreMarkerState::Pending(marker)
                if marker.profile_digest == self.profile_digest
                    && marker.allowed_keys == self.allowed_keys =>
            {
                fs::remove_file(marker_path()?).map_err(Into::into)
            }
            _ => Err(LifecycleLockError::RestorePending),
        }
    }
}
pub struct LifecycleGuard {
    _files: Vec<File>,
    trace_paths: Vec<(&'static str, PathBuf)>,
    // The held-guard count is per thread, so a guard must be released on the
    // thread that acquired it.
    _not_send: PhantomData<*const ()>,
}

impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        for (kind, path) in self.trace_paths.iter().rev() {
            drop(self._files.pop());
            trace_lock("release", kind, path);
        }
        HELD_GUARDS.with(|held| held.set(held.get().saturating_sub(1)));
    }
}

/// Proof that the current thread holds the lifecycle lock. It can only be
/// obtained from a live guard or while one is held, so coordinators that take
/// it never need to acquire the lock themselves.
pub struct HeldLifecycleLock {
    _not_send: PhantomData<*const ()>,
}

impl LifecycleGuard {
    /// Produce a proof usable by lifecycle coordinators without reacquiring.
    pub fn held_lock_proof(&self) -> HeldLifecycleLock {
        HeldLifecycleLock {
            _not_send: PhantomData,
        }
    }
}

impl LifecycleLock {
    /// Return a proof when this thread already holds a lifecycle guard, for
    /// example the guard the CLI acquires before running a lifecycle command.
    pub fn current_proof() -> Option<HeldLifecycleLock> {
        HELD_GUARDS.with(|held| {
            (held.get() > 0).then_some(HeldLifecycleLock {
                _not_send: PhantomData,
            })
        })
    }

    pub fn acquire() -> Result<LifecycleGuard, LifecycleLockError> {
        let guard = Self::acquire_raw()?;
        check_restore_pending()?;
        Ok(guard)
    }

    pub fn acquire_for_restore(
        archive: &Path,
        allowed_keys: &[&str],
    ) -> Result<RestoreLock, LifecycleLockError> {
        let guard = Self::acquire_raw()?;
        let profile_digest = fingerprint(&fs::read(archive)?);
        Self::finish_restore_acquire(guard, profile_digest, allowed_keys)
    }

    pub fn acquire_for_restore_sources(
        sources: &[(&str, Option<&[u8]>)],
        allowed_keys: &[&str],
    ) -> Result<RestoreLock, LifecycleLockError> {
        let guard = Self::acquire_raw()?;
        let mut entries = sources.to_vec();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0)
            || entries
                .iter()
                .any(|(name, _)| name.contains(['\t', '\n', '\r']))
        {
            return Err(LifecycleLockError::RestorePending);
        }
        let mut manifest = String::new();
        for (name, bytes) in entries {
            manifest.push_str(name);
            manifest.push('\t');
            manifest.push_str(
                &bytes
                    .map(fingerprint)
                    .unwrap_or_else(|| "absent".to_owned()),
            );
            manifest.push('\n');
        }
        Self::finish_restore_acquire(guard, fingerprint(manifest.as_bytes()), allowed_keys)
    }

    fn finish_restore_acquire(
        guard: LifecycleGuard,
        profile_digest: String,
        allowed_keys: &[&str],
    ) -> Result<RestoreLock, LifecycleLockError> {
        let mut keys: Vec<String> = allowed_keys.iter().map(|key| (*key).to_owned()).collect();
        keys.sort();
        if keys.windows(2).any(|pair| pair[0] == pair[1])
            || keys
                .iter()
                .any(|key| key.is_empty() || key.contains('/') || key.contains('\\'))
        {
            return Err(LifecycleLockError::RestorePending);
        }
        match read_restore_marker()? {
            RestoreMarkerState::Absent => {}
            RestoreMarkerState::Pending(marker)
                if marker.profile_digest == profile_digest && marker.allowed_keys == keys => {}
            _ => return Err(LifecycleLockError::RestorePending),
        }
        Ok(RestoreLock {
            _guard: guard,
            profile_digest,
            allowed_keys: keys,
        })
    }

    pub fn acquire_if_needed() -> Result<Option<LifecycleGuard>, LifecycleLockError> {
        if Self::current_proof().is_some() {
            check_restore_pending()?;
            Ok(None)
        } else {
            Self::acquire().map(Some)
        }
    }

    fn acquire_raw() -> Result<LifecycleGuard, LifecycleLockError> {
        let rendezvous = ccync_foundation::paths::lifecycle_lock_path()
            .ok_or(LifecycleLockError::PathUnavailable)?;
        let mut files = Vec::new();
        let mut trace_paths = Vec::new();
        files.push(acquire_file(&rendezvous)?);
        trace_paths.push(("rendezvous", rendezvous));
        trace_lock("acquire", "rendezvous", &trace_paths[0].1);

        if let Ok(roots) = ccync_foundation::paths::StorageRoots::resolve() {
            let state = roots.state.join("lifecycle.lock");
            match acquire_file(&state) {
                Ok(file) => {
                    files.push(file);
                    trace_paths.push(("state", state));
                    trace_lock("acquire", "state", &trace_paths[1].1);
                }
                Err(error) => {
                    while let Some((kind, path)) = trace_paths.pop() {
                        drop(files.pop());
                        trace_lock("release", kind, &path);
                    }
                    return Err(error);
                }
            }
        }

        HELD_GUARDS.with(|held| held.set(held.get() + 1));
        Ok(LifecycleGuard {
            _files: files,
            trace_paths,
            _not_send: PhantomData,
        })
    }
}

fn acquire_file(path: &PathBuf) -> Result<File, LifecycleLockError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    if let Err(error) = file.try_lock() {
        if matches!(error, std::fs::TryLockError::WouldBlock) {
            eprintln!("ccync is waiting for another ccync operation");
            file.lock()?;
        } else if let std::fs::TryLockError::Error(error) = error {
            return Err(error.into());
        }
    }
    Ok(file)
}

#[cfg(debug_assertions)]
fn trace_lock(action: &str, kind: &str, path: &std::path::Path) {
    use std::io::Write;
    let Some(trace) = std::env::var_os("CCYNC_LOCK_TRACE") else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(trace) {
        let _ = writeln!(file, "{action} {kind} {}", path.display());
    }
}

#[cfg(not(debug_assertions))]
fn trace_lock(_: &str, _: &str, _: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    fn setup() -> crate::test_support::TestHome {
        crate::test_support::TestHome::new()
    }

    #[test]
    fn restore_lock_sources_digest_matches_rerun_and_refuses_changed_source() {
        let _home = setup();
        let guard = LifecycleLock::acquire_for_restore_sources(
            &[("b", None), ("a", Some(b"original"))],
            &["config"],
        )
        .unwrap();
        guard
            .record_destinations(&[("config", Some(b"before"), Some(b"after"))])
            .unwrap();
        let RestoreMarkerState::Pending(marker) = read_restore_marker().unwrap() else {
            panic!("valid marker expected")
        };
        let manifest = format!("a\t{}\nb\tabsent\n", fingerprint(b"original"));
        assert_eq!(marker.profile_digest, fingerprint(manifest.as_bytes()));
        drop(guard);
        assert!(matches!(
            LifecycleLock::acquire_for_restore_sources(
                &[("a", Some(b"changed")), ("b", None)],
                &["config"]
            ),
            Err(LifecycleLockError::RestorePending)
        ));
        assert!(matches!(
            LifecycleLock::acquire_for_restore_sources(&[("a", None), ("b", None)], &["config"]),
            Err(LifecycleLockError::RestorePending)
        ));
        LifecycleLock::acquire_for_restore_sources(
            &[("a", Some(b"original")), ("b", None)],
            &["config"],
        )
        .unwrap()
        .clear_marker()
        .unwrap();
    }

    #[test]
    fn restore_marker_blocks_mutations_and_only_matches_same_restore() {
        let home = setup();
        let archive = home.path().join("backup");
        fs::write(&archive, b"archive").unwrap();
        let other = home.path().join("other");
        fs::write(&other, b"different").unwrap();
        let restore = LifecycleLock::acquire_for_restore(&archive, &["config"]).unwrap();
        restore
            .record_marker(&archive, &[("config", Some(b"before"), Some(b"after"))])
            .unwrap();
        assert!(LifecycleLock::acquire_if_needed().is_err());
        drop(restore);
        assert!(matches!(
            read_restore_marker().unwrap(),
            RestoreMarkerState::Pending(_)
        ));
        assert!(LifecycleLock::acquire().is_err());
        assert!(LifecycleLock::acquire_for_restore(&other, &["config"]).is_err());
        assert!(LifecycleLock::acquire_for_restore(&archive, &["other"]).is_err());
        let restore = LifecycleLock::acquire_for_restore(&archive, &["config"]).unwrap();
        restore.clear_marker().unwrap();
        drop(restore);
        assert!(LifecycleLock::acquire().is_ok());
    }

    #[test]
    fn held_restore_guard_blocks_lifecycle_entries_before_writes() {
        use crate::{
            adopt, config::CcyncConfig, delivery_apply::UnavailableNativeRunner, install, migrate,
        };
        use ccync_foundation::paths::EffectiveLayout;

        fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
            fn visit(root: &Path, path: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
                for entry in fs::read_dir(path).unwrap().flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        visit(root, &path, files);
                    } else if path.file_name().unwrap() != "lifecycle.lock" {
                        files.push((
                            path.strip_prefix(root).unwrap().to_owned(),
                            fs::read(path).unwrap(),
                        ));
                    }
                }
            }
            let mut files = Vec::new();
            visit(root, root, &mut files);
            files.sort();
            files
        }

        for committed in [false, true] {
            let home = setup();
            let layout = EffectiveLayout::load_or_legacy().unwrap();
            let layout = if committed {
                migrate::commit_fresh_layout(&layout).unwrap()
            } else {
                layout
            };
            let archive = home.path().join("backup");
            fs::write(&archive, b"archive").unwrap();
            let config_path = layout.config_path();
            fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            fs::write(&config_path, b"{}").unwrap();
            let config = CcyncConfig::load_from_path(&config_path);
            let lock_path = layout.build_lock_path();
            fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
            fs::write(&lock_path, b"{}").unwrap();
            let restore = LifecycleLock::acquire_for_restore(&archive, &["config"]).unwrap();
            restore
                .record_marker(&archive, &[("config", Some(b"before"), Some(b"after"))])
                .unwrap();

            for corrupt in [false, true] {
                if corrupt {
                    fs::write(marker_path().unwrap(), b"{").unwrap();
                }
                let before = snapshot(home.path());
                let update =
                    install::run_update_with_handoff_using(&config, &UnavailableNativeRunner)
                        .err()
                        .unwrap();
                assert!(update.to_string().contains("restore is pending"));
                let uninstall = install::run_uninstall_using(&UnavailableNativeRunner)
                    .err()
                    .unwrap();
                assert!(uninstall.to_string().contains("restore is pending"));
                let store = serde_json::from_str(r#"{"probe":{"command":"probe"}}"#).unwrap();
                let adoption = adopt::adopt_mcp_definitions(store, &lock_path)
                    .err()
                    .unwrap();
                assert!(adoption.to_string().contains("restore is pending"));
                let migration = migrate::commit_portable_migration(&layout).err().unwrap();
                assert!(migration.contains("restore is pending"));
                assert_eq!(snapshot(home.path()), before);
            }
            drop(restore);
        }
    }

    #[test]
    fn corrupt_restore_marker_fails_closed() {
        let _home = setup();
        let path = marker_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"{").unwrap();
        assert_eq!(read_restore_marker().unwrap(), RestoreMarkerState::Corrupt);
        assert!(LifecycleLock::acquire().is_err());
    }

    fn set_home_env(home: &crate::test_support::TestHome) -> Vec<SavedEnv> {
        [
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ]
        .into_iter()
        .map(|key| SavedEnv::set(key, home.path().join(key)))
        .collect()
    }

    struct SavedEnv(&'static str, Option<std::ffi::OsString>);

    impl SavedEnv {
        fn set(key: &'static str, value: PathBuf) -> Self {
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self(key, prior)
        }
    }

    impl Drop for SavedEnv {
        fn drop(&mut self) {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }

    #[test]
    fn lock_excludes_second_handle_and_releases_on_drop() {
        let _home = setup();
        let first = LifecycleLock::acquire().unwrap();
        let path = ccync_foundation::paths::lifecycle_lock_path().unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        second.try_lock().unwrap();
    }

    #[test]
    fn existing_unheld_file_does_not_block_acquisition() {
        let _home = setup();
        let path = ccync_foundation::paths::lifecycle_lock_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(&path).unwrap();
        let guard = LifecycleLock::acquire().unwrap();
        drop(guard);
    }

    #[test]
    fn current_proof_exists_only_while_this_thread_holds_a_guard() {
        let _home = setup();
        assert!(LifecycleLock::current_proof().is_none());
        let guard = LifecycleLock::acquire().unwrap();
        assert!(LifecycleLock::current_proof().is_some());
        let seen_elsewhere = std::thread::spawn(|| LifecycleLock::current_proof().is_some())
            .join()
            .unwrap();
        assert!(!seen_elsewhere, "another thread must not borrow this hold");
        drop(guard);
        assert!(LifecycleLock::current_proof().is_none());
    }

    #[test]
    fn ordered_lifecycle_locks() {
        const ROLE: &str = "CCYNC_ORDERED_LOCKS_CHILD";
        if let Ok(role) = std::env::var(ROLE) {
            assert_eq!(role, "hold");
            let _guard = LifecycleLock::acquire().unwrap();
            let ready = PathBuf::from(std::env::var_os("CCYNC_LOCK_READY").unwrap());
            File::create(ready).unwrap();
            let release = PathBuf::from(std::env::var_os("CCYNC_LOCK_RELEASE").unwrap());
            let deadline = Instant::now() + Duration::from_secs(10);
            while !release.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(release.exists(), "parent did not release child");
            return;
        }

        let home = setup();
        let _home_env = set_home_env(&home);
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let release = temp.path().join("release");
        let trace = temp.path().join("trace.log");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tool_manager::lock::tests::ordered_lifecycle_locks",
                "--nocapture",
            ])
            .env(ROLE, "hold")
            .env("CCYNC_LOCK_READY", &ready)
            .env("CCYNC_LOCK_RELEASE", &release)
            .env("CCYNC_LOCK_TRACE", &trace)
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("APPDATA", home.path().join("APPDATA"))
            .env("LOCALAPPDATA", home.path().join("LOCALAPPDATA"))
            .env("XDG_CONFIG_HOME", home.path().join("XDG_CONFIG_HOME"))
            .env("XDG_STATE_HOME", home.path().join("XDG_STATE_HOME"))
            .env("XDG_CACHE_HOME", home.path().join("XDG_CACHE_HOME"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "child did not acquire lifecycle locks");

        let rendezvous = OpenOptions::new()
            .read(true)
            .write(true)
            .open(ccync_foundation::paths::lifecycle_lock_path().unwrap())
            .unwrap();
        assert!(matches!(
            rendezvous.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        File::create(&release).unwrap();
        assert!(child.wait().unwrap().success());

        let trace = fs::read_to_string(trace).unwrap();
        let rendezvous_acquire = trace.find("acquire rendezvous").unwrap();
        let state_acquire = trace.find("acquire state").unwrap();
        assert!(rendezvous_acquire < state_acquire, "{trace}");
        assert!(trace.contains("release state"), "{trace}");
        assert!(trace.contains("release rendezvous"), "{trace}");

        let fresh = ccync_foundation::paths::EffectiveLayout::load().unwrap();
        assert_eq!(
            fresh.authority(),
            ccync_foundation::paths::EffectiveLayoutAuthority::Fresh
        );
    }

    #[test]
    fn ordered_lifecycle_locks_failure_releases_rendezvous() {
        let home = setup();
        let _home_env = set_home_env(&home);
        let roots = ccync_foundation::paths::StorageRoots::resolve().unwrap();
        fs::create_dir_all(roots.state.parent().unwrap()).unwrap();
        File::create(&roots.state).unwrap();
        assert!(LifecycleLock::acquire().is_err());

        let rendezvous = OpenOptions::new()
            .read(true)
            .write(true)
            .open(ccync_foundation::paths::lifecycle_lock_path().unwrap())
            .unwrap();
        rendezvous.try_lock().unwrap();
        drop(home);
    }

    #[test]
    fn invalid_storage_roots_keep_only_rendezvous_lock() {
        let home = setup();
        let _home_env = set_home_env(&home);
        let roots = ccync_foundation::paths::StorageRoots::resolve().unwrap();
        #[cfg(windows)]
        let _invalid_root = SavedEnv::set("LOCALAPPDATA", home.path().join(".agents"));
        #[cfg(unix)]
        let _invalid_root = SavedEnv::set("XDG_STATE_HOME", home.path().join(".agents"));
        assert!(ccync_foundation::paths::StorageRoots::resolve().is_err());
        let guard = LifecycleLock::acquire().unwrap();
        let state_path = roots.state.join("lifecycle.lock");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let state_lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(state_path)
            .unwrap();
        state_lock.try_lock().unwrap();
        drop(guard);
    }
}
