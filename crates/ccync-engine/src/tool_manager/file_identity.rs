//! Stable identity and content hashing for an opened executable.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    #[cfg(unix)]
    pub dev: u64,
    #[cfg(unix)]
    pub ino: u64,
    #[cfg(windows)]
    pub volume_serial_number: u32,
    #[cfg(windows)]
    pub file_index: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum HashError {
    #[error("file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("file identity changed while hashing, or target is not a regular file")]
    UnstableIdentity,
}

#[cfg(unix)]
fn identity(file: &File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

#[cfg(windows)]
fn identity(file: &File) -> io::Result<FileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, info.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok(FileIdentity {
        volume_serial_number: info.dwVolumeSerialNumber,
        file_index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    })
}

/// True when the path itself is a symlink or, on Windows, a reparse point.
pub fn is_reparse_or_link(path: &Path) -> io::Result<bool> {
    let metadata = std::fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0)
    }
    #[cfg(not(windows))]
    {
        Ok(metadata.file_type().is_symlink())
    }
}

/// Hash bytes through one opened handle and verify its identity remained stable.
pub fn hash_same_handle(path: &Path) -> Result<String, HashError> {
    let mut file = File::open(path)?;
    let before = identity(&file)?;
    if !file.metadata()?.is_file() || is_reparse_or_link(path)? {
        return Err(HashError::UnstableIdentity);
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let after = identity(&file)?;
    if before != after || !file.metadata()?.is_file() {
        return Err(HashError::UnstableIdentity);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn equal_content_hashes_match_and_identity_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"same").unwrap();
        fs::write(&b, b"same").unwrap();
        assert_eq!(hash_same_handle(&a).unwrap(), hash_same_handle(&b).unwrap());
        let f = File::open(&a).unwrap();
        assert_eq!(identity(&f).unwrap(), identity(&f).unwrap());
    }

    #[test]
    fn same_size_same_mtime_change_changes_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, b"abcd").unwrap();
        let old = fs::metadata(&path).unwrap().modified().unwrap();
        let first = hash_same_handle(&path).unwrap();
        fs::write(&path, b"wxyz").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        assert_ne!(first, hash_same_handle(&path).unwrap());
    }

    #[test]
    fn directory_and_link_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(hash_same_handle(dir.path()).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = dir.path().join("target");
            let link = dir.path().join("link");
            fs::write(&target, b"x").unwrap();
            symlink(&target, &link).unwrap();
            assert!(matches!(
                hash_same_handle(&link),
                Err(HashError::UnstableIdentity)
            ));
        }
    }
}
