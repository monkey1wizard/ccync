//! Deterministic, link-free manifests for tool artifact trees.
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeFile {
    pub relative_path: String,
    pub length: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeManifest {
    pub files: Vec<TreeFile>,
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("artifact tree I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("artifact tree contains a link, reparse point, or path outside its root: {0}")]
    UnsafePath(PathBuf),
    #[error("artifact tree path is not valid UTF-8: {0}")]
    NonUtf8(PathBuf),
    #[error("artifact manifest is invalid: {0}")]
    InvalidManifest(String),
    #[error("artifact hash differs after copying: {0}")]
    HashMismatch(String),
}

pub fn scan_tree(root: impl AsRef<Path>) -> Result<TreeManifest, TreeError> {
    let root = root.as_ref();
    if !root.is_dir() || super::file_identity::is_reparse_or_link(root)? {
        return Err(TreeError::UnsafePath(root.into()));
    }
    let canonical = fs::canonicalize(root)?;
    let mut files = Vec::new();
    walk(root, &canonical, root, &mut files)?;
    files.sort_by(|a: &TreeFile, b| a.relative_path.cmp(&b.relative_path));
    let sha256 = aggregate(&files);
    Ok(TreeManifest { files, sha256 })
}

fn walk(
    root: &Path,
    canonical: &Path,
    dir: &Path,
    out: &mut Vec<TreeFile>,
) -> Result<(), TreeError> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if !path.starts_with(root)
            || !fs::canonicalize(&path)?.starts_with(canonical)
            || super::file_identity::is_reparse_or_link(&path)?
        {
            return Err(TreeError::UnsafePath(path));
        }
        let ty = entry.file_type()?;
        if ty.is_dir() {
            walk(root, canonical, &path, out)?;
        } else if ty.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| TreeError::UnsafePath(path.clone()))?;
            let mut parts = Vec::new();
            for component in rel.components() {
                match component {
                    Component::Normal(v) => {
                        parts.push(v.to_str().ok_or_else(|| TreeError::NonUtf8(path.clone()))?)
                    }
                    _ => return Err(TreeError::UnsafePath(path.clone())),
                }
            }
            let relative_path = parts.join("/");
            let hash = super::file_identity::hash_same_handle(&path)
                .map_err(|e| TreeError::Io(io::Error::other(e.to_string())))?;
            out.push(TreeFile {
                relative_path,
                length: fs::metadata(&path)?.len(),
                sha256: hash,
            });
        } else {
            return Err(TreeError::UnsafePath(path));
        }
    }
    Ok(())
}

fn aggregate(files: &[TreeFile]) -> String {
    let mut h = Sha256::new();
    for f in files {
        h.update(f.relative_path.as_bytes());
        h.update([0]);
        h.update(f.length.to_be_bytes());
        h.update([0]);
        h.update(f.sha256.as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize())
}

pub fn materialize(
    manifest: &TreeManifest,
    src: impl AsRef<Path>,
    dest: impl AsRef<Path>,
) -> Result<(), TreeError> {
    let src = src.as_ref();
    let dest = dest.as_ref();
    if !src.is_dir() || super::file_identity::is_reparse_or_link(src)? {
        return Err(TreeError::UnsafePath(src.to_path_buf()));
    }
    let canonical_src = fs::canonicalize(src)?;
    let mut prior = String::new();
    for file in &manifest.files {
        if file.relative_path.is_empty()
            || file.relative_path.starts_with('/')
            || file.relative_path.contains('\\')
            || file
                .relative_path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            || (!prior.is_empty() && file.relative_path <= prior)
        {
            return Err(TreeError::InvalidManifest(file.relative_path.clone()));
        }
        prior.clone_from(&file.relative_path);
        let from = src.join(&file.relative_path);
        let to = dest.join(&file.relative_path);
        let mut component_path = src.to_path_buf();
        for component in file.relative_path.split('/') {
            component_path.push(component);
            if super::file_identity::is_reparse_or_link(&component_path)? {
                return Err(TreeError::UnsafePath(component_path));
            }
        }
        if !from.starts_with(src) || !fs::canonicalize(&from)?.starts_with(&canonical_src) {
            return Err(TreeError::UnsafePath(from));
        }
        fs::create_dir_all(to.parent().unwrap())?;
        fs::copy(&from, &to)?;
        if fs::metadata(&to)?.len() != file.length
            || super::file_identity::hash_same_handle(&to)
                .map_err(|e| TreeError::Io(io::Error::other(e.to_string())))?
                != file.sha256
        {
            return Err(TreeError::HashMismatch(file.relative_path.clone()));
        }
    }
    let copied = scan_tree(dest)?;
    if copied != *manifest {
        return Err(TreeError::InvalidManifest(
            "materialized tree differs".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sorted_manifest_is_order_independent_and_roundtrips() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        fs::create_dir(a.path().join("d")).unwrap();
        fs::create_dir(b.path().join("d")).unwrap();
        fs::write(a.path().join("z"), b"z").unwrap();
        fs::write(a.path().join("d/a"), b"a").unwrap();
        fs::write(b.path().join("d/a"), b"a").unwrap();
        fs::write(b.path().join("z"), b"z").unwrap();
        let m = scan_tree(a.path()).unwrap();
        assert_eq!(m, scan_tree(b.path()).unwrap());
        let out = tempfile::tempdir().unwrap();
        materialize(&m, a.path(), out.path()).unwrap();
        assert_eq!(scan_tree(out.path()).unwrap(), m);
    }
    #[test]
    fn same_size_byte_change_changes_aggregate() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        fs::write(&p, b"abcd").unwrap();
        let t = fs::metadata(&p).unwrap().modified().unwrap();
        let a = scan_tree(d.path()).unwrap();
        fs::write(&p, b"wxyz").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(t))
            .unwrap();
        assert_ne!(a.sha256, scan_tree(d.path()).unwrap().sha256);
    }
    #[test]
    fn rejects_links_and_parent_escapes() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("x"), b"x").unwrap();

        // Manifest with parent escape in relative path
        let bad_manifest = TreeManifest {
            files: vec![TreeFile {
                relative_path: "../outside".into(),
                length: 1,
                sha256: "0".repeat(64),
            }],
            sha256: "0".repeat(64),
        };
        let dest = tempfile::tempdir().unwrap();
        assert!(materialize(&bad_manifest, d.path(), dest.path()).is_err());

        let dot_dot_manifest = TreeManifest {
            files: vec![TreeFile {
                relative_path: "sub/../../outside".into(),
                length: 1,
                sha256: "0".repeat(64),
            }],
            sha256: "0".repeat(64),
        };
        assert!(materialize(&dot_dot_manifest, d.path(), dest.path()).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(outside.path().join("x"), d.path().join("link")).unwrap();
            assert!(scan_tree(d.path()).is_err());
        }
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_file(
                outside.path().join("x"),
                d.path().join("link"),
            ) {
                Ok(()) => {
                    assert!(scan_tree(d.path()).is_err());
                }
                Err(e) => {
                    eprintln!("skipping Windows symlink test (unprivileged): {e}");
                }
            }
        }
    }
}
