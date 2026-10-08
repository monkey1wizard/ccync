//! Cross-platform filesystem primitives.
//!
//! OS-divergent operations shared by the render and projection layers:
//! directory links (NTFS junction on Windows / symlink on Unix),
//! link removal, link detection, and the canonical-root atomic swap. Behavior is
//! byte-identical to the prior inline implementations; callers keep their own
//! error-message prefixes by mapping the returned error.

use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Create a directory link `link` -> `target`.
///
/// Windows: NTFS directory junction via `mklink /J`. Unix: `symlink`.
pub fn create_dir_link(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(stderr.into_owned()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link)
    }
}

/// Remove a directory link.
///
/// Windows: `rmdir` (removes a junction without touching the target). Unix:
/// `remove_file` (removes the symlink). Callers keep their own existence guards.
pub fn remove_dir_link(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd")
            .args(["/C", "rmdir", &path.to_string_lossy()])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(stderr.into_owned()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_file(path)
    }
}

/// The on-disk shape of a directory-link-capable path, as told apart by
/// [`classify_link`] and reported by [`create_symlink_dir`].
///
/// `is_symlink_or_junction` deliberately collapses `Symlink` and `Junction`
/// into one boolean; this vocabulary exists for the callers that must not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkShape {
    /// A symbolic link (Windows `symlink_dir` / Unix `symlink`).
    Symlink,
    /// An NTFS directory junction (Windows-only; see `create_dir_link`).
    Junction,
    /// A real directory — not a link of either shape.
    Directory,
    /// Nothing exists at the path.
    Absent,
}

/// Create a directory *symbolic* link `link` -> `target`, reporting the shape
/// that landed instead of silently falling back to another shape itself.
///
/// Windows: `std::os::windows::fs::symlink_dir`, which requires
/// `SeCreateSymbolicLinkPrivilege` (Developer Mode or an elevated process); a
/// caller lacking it gets `Err` (typically `ERROR_PRIVILEGE_NOT_HELD`), not a
/// silently-created junction. Unix: `std::os::unix::fs::symlink`. Callers that
/// want a junction fallback on refusal compose this with `create_dir_link`
/// themselves — this function never falls back on its own.
pub fn create_symlink_dir(target: &Path, link: &Path) -> io::Result<LinkShape> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)?;
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link)?;
    }
    Ok(LinkShape::Symlink)
}

/// Classify the on-disk shape at `path`: symbolic link, NTFS junction, real
/// directory, or absent.
///
/// Windows: `FileType::is_symlink` is true for *any* reparse point (symlink
/// or junction alike) on this toolchain — it does not read the reparse tag,
/// so it cannot tell the two apart. Distinguishing them requires the raw
/// reparse tag, which [`windows_reparse_tag`] reads via `FindFirstFileW`
/// (`dwReserved0`) rather than pulling in a Windows-API crate for one field.
/// Unix has no junction concept, so a non-symlink is always `Directory` once
/// it is known to exist.
pub fn classify_link(path: &Path) -> LinkShape {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

        let Ok(metadata) = path.symlink_metadata() else {
            return LinkShape::Absent;
        };
        if (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) == 0 {
            return LinkShape::Directory;
        }
        match windows_reparse_tag(path) {
            Some(IO_REPARSE_TAG_SYMLINK) => LinkShape::Symlink,
            // Any other reparse tag (notably `IO_REPARSE_TAG_MOUNT_POINT`,
            // which is what `create_dir_link`'s `mklink /J` produces) or an
            // unreadable tag is treated as a junction rather than a symlink.
            _ => LinkShape::Junction,
        }
    }
    #[cfg(not(windows))]
    {
        let Ok(metadata) = path.symlink_metadata() else {
            return LinkShape::Absent;
        };
        if metadata.file_type().is_symlink() {
            LinkShape::Symlink
        } else {
            LinkShape::Directory
        }
    }
}

/// Read the raw reparse tag for `path` via `FindFirstFileW`, whose
/// `WIN32_FIND_DATAW::dwReserved0` field carries the reparse tag
/// (`IO_REPARSE_TAG_SYMLINK`, `IO_REPARSE_TAG_MOUNT_POINT`, ...) when
/// `dwFileAttributes` has `FILE_ATTRIBUTE_REPARSE_POINT` set. `std` exposes no
/// safe accessor for this field, and pulling in a Windows-API crate for one
/// field is out of scope here, so this binds the two `kernel32` functions
/// directly. `None` when the path cannot be found.
#[cfg(windows)]
fn windows_reparse_tag(path: &Path) -> Option<u32> {
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct RawFiletime {
        low: u32,
        high: u32,
    }

    // Layout matches `WIN32_FIND_DATAW`; only `dwFileAttributes` and
    // `dwReserved0` are read, the rest exist to keep the struct's size and
    // field offsets correct for `FindFirstFileW` to write into.
    #[repr(C)]
    struct Win32FindDataW {
        file_attributes: u32,
        creation_time: RawFiletime,
        last_access_time: RawFiletime,
        last_write_time: RawFiletime,
        file_size_high: u32,
        file_size_low: u32,
        reserved0: u32,
        reserved1: u32,
        file_name: [u16; 260],
        alternate_file_name: [u16; 14],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn FindFirstFileW(
            file_name: *const u16,
            find_file_data: *mut Win32FindDataW,
        ) -> *mut std::ffi::c_void;
        fn FindClose(find_file: *mut std::ffi::c_void) -> i32;
    }

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    const INVALID_HANDLE_VALUE: isize = -1;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut data: Win32FindDataW = unsafe { std::mem::zeroed() };

    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if handle as isize == INVALID_HANDLE_VALUE {
        return None;
    }
    unsafe {
        FindClose(handle);
    }

    if (data.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT) == 0 {
        None
    } else {
        Some(data.reserved0)
    }
}

/// Return `true` if `path` is a symlink or NTFS junction, even when the target is absent.
pub fn is_symlink_or_junction(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

        path.symlink_metadata()
            .map(|metadata| {
                metadata.file_type().is_symlink()
                    || (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            })
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        path.is_symlink()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(windows)]
    #[test]
    fn ordinary_directory_is_not_a_junction() {
        let temp = TempDir::new().unwrap();
        let ordinary_dir = temp.path().join("plain-dir");
        std::fs::create_dir(&ordinary_dir).unwrap();
        assert!(!is_symlink_or_junction(&ordinary_dir));
    }

    #[test]
    fn classify_link_distinguishes_directory_and_absent() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("plain-dir");
        std::fs::create_dir(&dir).unwrap();
        let absent = temp.path().join("does-not-exist");

        assert_eq!(classify_link(&dir), LinkShape::Directory);
        assert_eq!(classify_link(&absent), LinkShape::Absent);
    }

    /// A junction (created via the existing `create_dir_link`, which needs no
    /// elevated privilege) must classify as `Junction`, distinct from
    /// `Directory` and `Absent`. When the environment also grants
    /// `SeCreateSymbolicLinkPrivilege`, the symlink shape is exercised too and
    /// all four `LinkShape` variants are asserted pairwise distinct; when it
    /// does not, `create_symlink_dir_reports_shape_or_typed_refusal` below
    /// covers the refusal path instead.
    #[cfg(windows)]
    #[test]
    fn classify_link_gives_four_distinct_shapes() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let plain_dir = temp.path().join("plain-dir");
        std::fs::create_dir(&plain_dir).unwrap();
        let absent = temp.path().join("does-not-exist");

        let junction = temp.path().join("junction-link");
        create_dir_link(&target, &junction).unwrap();
        let junction_shape = classify_link(&junction);
        assert_eq!(junction_shape, LinkShape::Junction);
        assert_eq!(classify_link(&plain_dir), LinkShape::Directory);
        assert_eq!(classify_link(&absent), LinkShape::Absent);

        let symlink = temp.path().join("symlink-link");
        if let Ok(shape) = create_symlink_dir(&target, &symlink) {
            assert_eq!(shape, LinkShape::Symlink);
            let symlink_shape = classify_link(&symlink);
            assert_eq!(symlink_shape, LinkShape::Symlink);

            let shapes = [
                symlink_shape,
                junction_shape,
                LinkShape::Directory,
                LinkShape::Absent,
            ];
            for i in 0..shapes.len() {
                for j in (i + 1)..shapes.len() {
                    assert_ne!(
                        shapes[i], shapes[j],
                        "LinkShape variants must be pairwise distinct"
                    );
                }
            }
        }
    }

    /// `create_symlink_dir` must report the shape it created, and a refusal
    /// (typically `ERROR_PRIVILEGE_NOT_HELD`, OS code 1314, when the caller
    /// lacks `SeCreateSymbolicLinkPrivilege`) must surface as a typed `Err`
    /// rather than silently creating a junction at `link`.
    #[cfg(windows)]
    #[test]
    fn create_symlink_dir_reports_shape_or_typed_refusal() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");

        match create_symlink_dir(&target, &link) {
            Ok(shape) => {
                assert_eq!(shape, LinkShape::Symlink);
                assert_eq!(classify_link(&link), LinkShape::Symlink);
            }
            Err(_) => {
                assert_eq!(
                    classify_link(&link),
                    LinkShape::Absent,
                    "a refused symlink creation must not leave any link behind"
                );
            }
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn create_symlink_dir_reports_symlink_shape() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = temp.path().join("link");

        let shape = create_symlink_dir(&target, &link).unwrap();
        assert_eq!(shape, LinkShape::Symlink);
        assert_eq!(classify_link(&link), LinkShape::Symlink);
    }

    #[test]
    fn sync_dir_in_place_overwrites_and_prunes() {
        use std::fs;
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");
        // src: new content + a nested file.
        fs::create_dir_all(src.join("commands")).unwrap();
        fs::write(src.join("plugin.json"), "NEW").unwrap();
        fs::write(
            src.join("commands").join("ccync.md"),
            "ccync dispatch-script",
        )
        .unwrap();
        // dst: stale content (old file value + an extraneous file to prune).
        fs::create_dir_all(dst.join("commands")).unwrap();
        fs::write(dst.join("plugin.json"), "OLD").unwrap();
        fs::write(
            dst.join("commands").join("ccync.md"),
            "scripts/ccync.ps1 dispatch",
        )
        .unwrap();
        fs::write(dst.join("stale.txt"), "remove me").unwrap();

        sync_dir_in_place(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("plugin.json")).unwrap(), "NEW");
        assert_eq!(
            fs::read_to_string(dst.join("commands").join("ccync.md")).unwrap(),
            "ccync dispatch-script"
        );
        assert!(
            !dst.join("stale.txt").exists(),
            "extraneous dst entry must be pruned"
        );
    }

    #[test]
    fn atomic_swap_normal_path_replaces_root() {
        use std::fs;
        let temp = TempDir::new().unwrap();
        let temp_dir = temp.path().join("render");
        let canonical = temp.path().join("canonical");
        fs::create_dir_all(&temp_dir).unwrap();
        fs::write(temp_dir.join("f.txt"), "new").unwrap();
        fs::create_dir_all(&canonical).unwrap();
        fs::write(canonical.join("old.txt"), "old").unwrap();

        atomic_swap(&temp_dir, &canonical).unwrap();
        assert_eq!(fs::read_to_string(canonical.join("f.txt")).unwrap(), "new");
        assert!(!canonical.join("old.txt").exists(), "old root replaced");
    }

    #[test]
    fn atomic_write_bytes_round_trips_and_overwrites() {
        let temp = TempDir::new().unwrap();
        let dest = temp.path().join("config.toml");

        atomic_write_bytes(&dest, b"hello = 1\n").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello = 1\n");

        // Overwrite an existing target.
        atomic_write_bytes(&dest, b"hello = 2\n").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello = 2\n");

        assert!(
            !tmp_sibling(&dest).exists(),
            "temp sibling must not linger after a successful write"
        );
    }

    #[test]
    fn atomic_write_bytes_writes_a_fresh_path() {
        let temp = TempDir::new().unwrap();
        let dest = temp.path().join("fresh.json");
        assert!(!dest.exists());

        atomic_write_bytes(&dest, b"{}").unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"{}");
        assert!(!tmp_sibling(&dest).exists());
    }

    /// Force a failure at the injected pre-rename seam — after the temp file
    /// is durably written, before the rename that would publish it — and
    /// assert the original `dest` survives byte-identical, with no stray
    /// temp file left behind. Non-trivial original content is used so a
    /// truncation bug would be caught.
    #[test]
    fn atomic_write_bytes_pre_rename_failure_leaves_original_untouched() {
        let temp = TempDir::new().unwrap();
        let dest = temp.path().join("projection.txn.json");
        let original = b"{\"landed\": true, \"marker\": \"do-not-truncate-me\"}".to_vec();
        std::fs::write(&dest, &original).unwrap();

        let result = atomic_write_bytes_inner(&dest, b"new content that must never land", || {
            Err(io::Error::other("injected pre-rename failure"))
        });

        assert!(result.is_err(), "injected pre-rename failure must surface");
        assert!(matches!(
            result.unwrap_err(),
            AtomicWriteError::PreRename(_)
        ));

        // The rename never ran, so the original bytes must be untouched.
        assert_eq!(std::fs::read(&dest).unwrap(), original);

        // The durably-written temp file must be cleaned up, not left behind.
        assert!(
            !tmp_sibling(&dest).exists(),
            "temp file must be removed on injected pre-rename failure"
        );
    }

    /// A target the user hardened with `chmod 600` must not be silently
    /// relaxed to world-readable by the publish-via-rename writer.
    #[cfg(unix)]
    #[test]
    fn atomic_write_bytes_preserves_an_existing_targets_mode() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let dest = temp.path().join("config.json");
        std::fs::write(&dest, b"{\"secrets\":{}}").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).unwrap();

        atomic_write_bytes(&dest, b"{\"secrets\":{\"k\":\"v\"}}").unwrap();

        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "hardened mode must survive the rewrite");
    }

    /// A file this writer creates carries secrets often enough that owner-only
    /// is the right default; it must not land at `0o666 & !umask`.
    #[cfg(unix)]
    #[test]
    fn atomic_write_bytes_creates_a_fresh_target_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let dest = temp.path().join("fresh.json");

        atomic_write_bytes(&dest, b"{}").unwrap();

        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a fresh target must be owner-only");
    }

    /// Writing a symlinked path must follow the link and rewrite its target,
    /// not replace the link with a regular file.
    #[cfg(unix)]
    #[test]
    fn atomic_write_bytes_writes_through_a_symlink() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("real-config.json");
        let link = temp.path().join("config.json");
        std::fs::write(&target, b"old").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        atomic_write_bytes(&link, b"new").unwrap();

        assert!(link.is_symlink(), "the symlink itself must survive");
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
    }
}

/// Error from [`atomic_swap`]; callers map this to their own domain error type
/// (and re-add any message prefix) to preserve existing error text.
#[derive(Debug, Error)]
pub enum AtomicSwapError {
    #[error("canonical root has no parent")]
    NoParent,
    #[error("{0}")]
    BackupFailed(String),
    #[error("{0}")]
    MoveFailed(String),
}

/// Atomic swap: move `temp_dir` onto `canonical_root`.
///
/// Uses a backup-move-restore pattern for kill-mid-swap recovery. Platform-
/// agnostic (`fs::rename`), grouped here as the canonical-root swap primitive.
pub fn atomic_swap(temp_dir: &Path, canonical_root: &Path) -> Result<(), AtomicSwapError> {
    use std::fs;

    let parent = canonical_root.parent().ok_or(AtomicSwapError::NoParent)?;

    // Create backup path.
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let backup_name = format!(".ccync-plugin-backup-{}", uuid);
    let backup_path = parent.join(backup_name);

    let had_existing_root = canonical_root.exists();

    // Backup existing root if it exists.
    if had_existing_root {
        match fs::rename(canonical_root, &backup_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                // The canonical root cannot be renamed because a process holds an open
                // handle on the directory (e.g. a running plugin host keeps its
                // marketplace-source dir open while `ccync sync` runs from inside Claude Code).
                // The atomic backup-move-restore path is impossible, so fall back to a
                // NON-atomic in-place content sync: overwrite files and prune stale
                // entries. File-level writes succeed where a whole-dir rename fails.
                sync_dir_in_place(temp_dir, canonical_root)
                    .map_err(|e| AtomicSwapError::MoveFailed(e.to_string()))?;
                let _ = fs::remove_dir_all(temp_dir);
                return Ok(());
            }
            Err(e) => return Err(AtomicSwapError::BackupFailed(e.to_string())),
        }
    }

    // Move temp to canonical.
    match fs::rename(temp_dir, canonical_root) {
        Ok(()) => {
            // Success: remove backup.
            if backup_path.exists() {
                let _ = fs::remove_dir_all(&backup_path);
            }
            Ok(())
        }
        Err(e) => {
            // Failure: restore backup.
            if had_existing_root && backup_path.exists() && !canonical_root.exists() {
                let _ = fs::rename(&backup_path, canonical_root);
            }
            Err(AtomicSwapError::MoveFailed(e.to_string()))
        }
    }
}

/// Recursively sync `src` content into `dst` in place: overwrite every `src` file/dir
/// into `dst`, then remove `dst` entries absent from `src`. Non-atomic — the fallback
/// for [`atomic_swap`] when the canonical root is held by an open handle and cannot be
/// renamed. Only needs file-level writes, which succeed while a dir rename is denied.
fn sync_dir_in_place(src: &Path, dst: &Path) -> io::Result<()> {
    use std::fs;
    fs::create_dir_all(dst)?;
    // 1. Copy/overwrite every src entry into dst.
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            sync_dir_in_place(&from, &to)?;
        } else {
            // A directory currently at `to` must go before copying a file over it.
            if to.is_dir() && !is_symlink_or_junction(&to) {
                fs::remove_dir_all(&to)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    // 2. Prune dst entries not present in src (best-effort; ignore locked entries).
    for entry in fs::read_dir(dst)? {
        let entry = entry?;
        if src.join(entry.file_name()).exists() {
            continue;
        }
        let p = entry.path();
        let _ = if is_symlink_or_junction(&p) {
            remove_dir_link(&p).or_else(|_| fs::remove_file(&p))
        } else if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        };
    }
    Ok(())
}

/// Sibling temp path `<dest>.tmp` in the same directory (so the rename in
/// [`atomic_write_bytes`] stays on one filesystem). Appends `.tmp` to the full
/// file name rather than replacing the extension, so `config.toml` becomes
/// `config.toml.tmp`.
fn tmp_sibling(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    dest.with_file_name(name)
}

/// Error from [`atomic_write_bytes`], naming the stage at which the write
/// failed so callers/tests can tell "never touched `dest`" apart from "touched
/// `dest`, but the rename itself failed."
#[derive(Debug, Error)]
pub enum AtomicWriteError {
    #[error("failed to write temp file: {0}")]
    TempWrite(io::Error),
    #[error("failed before rename could run: {0}")]
    PreRename(io::Error),
    #[error("failed to rename temp file into place: {0}")]
    Rename(io::Error),
}

/// Write `contents` to `dest` atomically and durably: write a sibling
/// `<dest>.tmp` in the same directory, flush it to the storage device, then
/// `rename` it over `dest`. The rename is atomic on a single filesystem (on
/// Windows it overwrites the target). A failure at any stage removes the temp
/// file and leaves `dest` byte-identical to whatever it held before the call.
///
/// A symlinked `dest` is written *through*: the link is resolved first, so the
/// rename replaces the file the link points at rather than the link itself.
///
/// On Unix the temp file is created `0o600` — it must never be world-readable
/// while it holds the bytes of a secret-bearing file such as `config.json` —
/// and, when `dest` already exists, its mode is copied onto the temp before the
/// rename. A target the user hardened with `chmod 600` therefore keeps that
/// mode across a rewrite; a target created fresh by this writer lands `0o600`.
pub fn atomic_write_bytes(dest: &Path, contents: &[u8]) -> Result<(), AtomicWriteError> {
    atomic_write_bytes_inner(dest, contents, || Ok(()))
}

/// Core of [`atomic_write_bytes`], parameterized by a `before_rename` hook run
/// after the temp file has been written and durably synced, but before the
/// `rename` that publishes it over `dest`.
///
/// The hook is not incidental test plumbing: it is the only way to prove the
/// durability ordering this writer promises — that a failure occurring at the
/// exact boundary between "temp file is safely on disk" and "rename makes it
/// visible as `dest`" leaves the original `dest` byte-identical and does not
/// leak a stray temp file. That boundary cannot be forced by any external
/// means (e.g. pre-creating the temp path to break the write, as
/// `crates/mcp/src/io.rs`'s own test does, only forces an earlier and
/// different failure point). [`atomic_write_bytes`] always passes a no-op
/// hook; only tests in this module reach for the seam directly.
fn atomic_write_bytes_inner(
    dest: &Path,
    contents: &[u8],
    before_rename: impl FnOnce() -> io::Result<()>,
) -> Result<(), AtomicWriteError> {
    // Resolve a symlinked target to the file it points at, so the rename below
    // writes through the link instead of replacing it with a regular file. A
    // `dest` that does not exist yet cannot be canonicalized and is used as-is.
    let resolved = std::fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
    let dest = resolved.as_path();
    let tmp = tmp_sibling(dest);

    let written = (|| -> io::Result<()> {
        let mut file = create_temp_file(&tmp)?;
        file.write_all(contents)?;
        // Flush this file's data+metadata before it is published by the rename.
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(AtomicWriteError::TempWrite(e));
    }

    // Carry the existing target's mode onto the temp, so publishing by rename
    // does not silently relax permissions the user set on `dest` itself.
    if let Err(e) = inherit_dest_mode(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AtomicWriteError::TempWrite(e));
    }

    if let Err(e) = before_rename() {
        let _ = std::fs::remove_file(&tmp);
        return Err(AtomicWriteError::PreRename(e));
    }

    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AtomicWriteError::Rename(e));
    }

    // Flush the directory entry the rename created, so the rename itself
    // survives a power loss and not just the bytes it points at. Unix-only and
    // best-effort: `std` exposes no directory handle on Windows, and NTFS does
    // not offer an equivalent through this API. The failure it would guard
    // against (rename lost, temp file orphaned) leaves `dest` at its previous
    // valid content, so a failure here must not fail the write.
    #[cfg(unix)]
    if let Some(parent) = dest.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

/// Create the temp file the atomic writer publishes from.
///
/// Unix: `0o600` rather than `File::create`'s `0o666 & !umask`, because the
/// temp briefly holds the full contents of files that carry secrets, and
/// because a `dest` that does not exist yet inherits this mode through the
/// rename. Windows has no Unix mode bits, so it keeps `File::create`.
#[cfg(unix)]
fn create_temp_file(tmp: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)?;
    // `mode` only applies when the open creates the file; a leftover temp from
    // an earlier crash would keep its old, possibly looser mode.
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
fn create_temp_file(tmp: &Path) -> io::Result<std::fs::File> {
    std::fs::File::create(tmp)
}

/// Copy an existing `dest`'s mode onto `tmp` so the rename preserves it. No-op
/// when `dest` does not exist yet (the temp's `0o600` then stands) and on
/// non-Unix platforms, which have no equivalent mode bits.
#[cfg(unix)]
fn inherit_dest_mode(tmp: &Path, dest: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(dest) else {
        return Ok(());
    };
    let mode = meta.permissions().mode() & 0o7777;
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn inherit_dest_mode(_tmp: &Path, _dest: &Path) -> io::Result<()> {
    Ok(())
}
