use std::{
    fs::{self, File, OpenOptions},
    io,
    path::PathBuf,
};

#[derive(Debug, thiserror::Error)]
pub enum LifecycleLockError {
    #[error("lifecycle lock path is unavailable")]
    PathUnavailable,
    #[error("lifecycle lock I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub struct LifecycleLock;
pub struct LifecycleGuard {
    _file: File,
}

impl LifecycleLock {
    pub fn acquire() -> Result<LifecycleGuard, LifecycleLockError> {
        let path: PathBuf = ccync_foundation::paths::lifecycle_lock_path()
            .ok_or(LifecycleLockError::PathUnavailable)?;
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
        Ok(LifecycleGuard { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::test_support::env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("USERPROFILE", dir.path());
        (dir, guard)
    }

    #[test]
    fn lock_excludes_second_handle_and_releases_on_drop() {
        let (_home, _env) = setup();
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
        let (_home, _env) = setup();
        let path = ccync_foundation::paths::lifecycle_lock_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(&path).unwrap();
        let guard = LifecycleLock::acquire().unwrap();
        drop(guard);
    }
}
