//! Bounded resolution of executable symlink and junction chains.

use ccync_foundation::platform::{classify_link, LinkShape};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct LinkChainPolicy {
    pub containment_roots: Vec<PathBuf>,
    pub max_hops: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkHop {
    pub link: PathBuf,
    pub target: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLinkChain {
    pub target: PathBuf,
    pub hops: Vec<LinkHop>,
}

#[derive(Debug, thiserror::Error)]
pub enum LinkChainError {
    #[error("link chain contains a cycle")]
    Cycle,
    #[error("link chain contains a broken link")]
    BrokenLink,
    #[error("final target is not a regular file")]
    NotRegularFile,
    #[error("link chain escaped the allowed roots")]
    OutsideRoots,
    #[error("link chain exceeds the hop limit")]
    TooManyHops,
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

fn within(path: &Path, roots: &[PathBuf]) -> bool {
    #[cfg(windows)]
    {
        fn normalized(path: &Path) -> String {
            let value = path.to_string_lossy().replace('/', "\\");
            let value = if let Some(rest) = value.strip_prefix("\\\\?\\UNC\\") {
                format!("\\\\{rest}")
            } else if let Some(rest) = value.strip_prefix("\\\\?\\") {
                rest.to_owned()
            } else {
                value
            };
            value.trim_end_matches('\\').to_lowercase()
        }
        let candidate = normalized(path);
        roots.iter().any(|root| {
            let root = normalized(root);
            candidate == root
                || candidate
                    .strip_prefix(&root)
                    .is_some_and(|suffix| suffix.starts_with('\\'))
        })
    }
    #[cfg(not(windows))]
    {
        roots
            .iter()
            .any(|root| path == root || path.starts_with(root))
    }
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, std::io::Error> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

/// Resolve links incrementally, recording each link and enforcing containment.
pub fn resolve_link_chain(
    start: &Path,
    policy: &LinkChainPolicy,
) -> Result<ResolvedLinkChain, LinkChainError> {
    let roots = policy
        .containment_roots
        .iter()
        .map(std::fs::canonicalize)
        .collect::<Result<Vec<_>, _>>()?;
    let mut current = start.to_path_buf();
    let mut seen = HashSet::new();
    let mut hops = Vec::new();
    loop {
        let lexical = normalize_absolute(&current)?;
        if !within(&lexical, &roots) {
            return Err(LinkChainError::OutsideRoots);
        }
        if !seen.insert(lexical.clone()) {
            return Err(LinkChainError::Cycle);
        }
        let metadata = match std::fs::symlink_metadata(&lexical) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !hops.is_empty() => {
                return Err(LinkChainError::BrokenLink)
            }
            Err(e) => return Err(e.into()),
        };
        let shape = classify_link(&lexical);
        if metadata.file_type().is_symlink()
            || matches!(shape, LinkShape::Symlink | LinkShape::Junction)
        {
            if hops.len() >= policy.max_hops {
                return Err(LinkChainError::TooManyHops);
            }
            let raw = std::fs::read_link(&lexical)?;
            let target = if raw.is_absolute() {
                raw
            } else {
                lexical.parent().unwrap_or(Path::new(".")).join(raw)
            };
            let target_norm = normalize_absolute(&target)?;
            if !within(&target_norm, &roots) {
                return Err(LinkChainError::OutsideRoots);
            }
            hops.push(LinkHop {
                link: lexical,
                target: target_norm.clone(),
            });
            current = target_norm;
            continue;
        }
        let final_path = std::fs::canonicalize(&lexical).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound && !hops.is_empty() {
                LinkChainError::BrokenLink
            } else {
                LinkChainError::Io(e)
            }
        })?;
        let final_meta = std::fs::metadata(&final_path)?;
        if !final_meta.is_file() {
            return Err(LinkChainError::NotRegularFile);
        }
        if !within(&final_path, &roots) {
            return Err(LinkChainError::OutsideRoots);
        }
        return Ok(ResolvedLinkChain {
            target: final_path,
            hops,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn policy(root: &Path, max_hops: usize) -> LinkChainPolicy {
        LinkChainPolicy {
            containment_roots: vec![root.to_path_buf()],
            max_hops,
        }
    }

    #[cfg(unix)]
    fn link(target: &Path, path: &Path) {
        std::os::unix::fs::symlink(target, path).unwrap();
    }
    #[cfg(windows)]
    fn link(target: &Path, path: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(target, path)
    }

    #[test]
    fn two_hops_resolve_and_record() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real.exe");
        let one = d.path().join("one");
        let two = d.path().join("two");
        fs::write(&real, b"exe").unwrap();
        #[cfg(unix)]
        {
            link(&real, &one);
            link(&one, &two);
        }
        #[cfg(windows)]
        {
            if let Err(e) = link(&real, &one) {
                eprintln!("SKIP: cannot create symlink: {e}");
                return;
            }
            if let Err(e) = link(&one, &two) {
                eprintln!("SKIP: cannot create symlink: {e}");
                return;
            }
        }
        let resolved = resolve_link_chain(&two, &policy(d.path(), 2)).unwrap();
        assert_eq!(resolved.target, fs::canonicalize(real).unwrap());
        assert_eq!(resolved.hops.len(), 2);
    }

    #[test]
    fn rejects_cycle_broken_directory_escape_and_excess_hops() {
        let d = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        {
            let probe_target = d.path().join("probe_target");
            let probe_link = d.path().join("probe_link");
            fs::write(&probe_target, b"probe").unwrap();
            if let Err(e) = link(&probe_target, &probe_link) {
                eprintln!("SKIP: cannot create symlink: {e}");
                return;
            }
        }
        let a = d.path().join("a");
        let b = d.path().join("b");
        let _ = link(&b, &a);
        let _ = link(&a, &b);
        assert!(matches!(
            resolve_link_chain(&a, &policy(d.path(), 8)),
            Err(LinkChainError::Cycle)
        ));
        let broken = d.path().join("broken");
        let _ = link(&d.path().join("missing"), &broken);
        assert!(matches!(
            resolve_link_chain(&broken, &policy(d.path(), 8)),
            Err(LinkChainError::BrokenLink)
        ));
        let dir = d.path().join("directory");
        fs::create_dir(&dir).unwrap();
        let ld = d.path().join("ld");
        let _ = link(&dir, &ld);
        assert!(matches!(
            resolve_link_chain(&ld, &policy(d.path(), 8)),
            Err(LinkChainError::NotRegularFile)
        ));
        let outside = tempfile::tempdir().unwrap();
        let f = outside.path().join("f");
        fs::write(&f, b"x").unwrap();
        let escape = d.path().join("escape");
        let _ = link(&f, &escape);
        assert!(matches!(
            resolve_link_chain(&escape, &policy(d.path(), 8)),
            Err(LinkChainError::OutsideRoots)
        ));
        let target = d.path().join("target");
        let l1 = d.path().join("l1");
        let l2 = d.path().join("l2");
        fs::write(&target, b"x").unwrap();
        let _ = link(&target, &l1);
        let _ = link(&l1, &l2);
        assert!(matches!(
            resolve_link_chain(&l2, &policy(d.path(), 1)),
            Err(LinkChainError::TooManyHops)
        ));
    }

    #[test]
    fn direct_file_containment_and_type_without_links() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real.exe");
        fs::write(&real, b"exe").unwrap();
        let pol = policy(d.path(), 2);
        let resolved = resolve_link_chain(&real, &pol).unwrap();
        assert_eq!(resolved.target, fs::canonicalize(&real).unwrap());
        assert_eq!(resolved.hops.len(), 0);

        let outside = tempfile::tempdir().unwrap();
        let out_file = outside.path().join("out.exe");
        fs::write(&out_file, b"exe").unwrap();
        assert!(matches!(
            resolve_link_chain(&out_file, &pol),
            Err(LinkChainError::OutsideRoots)
        ));

        let sub_dir = d.path().join("subdir");
        fs::create_dir(&sub_dir).unwrap();
        assert!(matches!(
            resolve_link_chain(&sub_dir, &pol),
            Err(LinkChainError::NotRegularFile)
        ));
    }

    #[test]
    fn within_handles_windows_verbatim_prefixes() {
        #[cfg(windows)]
        {
            let root = PathBuf::from(r"C:\tools\bin");
            let verbatim_root = PathBuf::from(r"\\?\C:\tools\bin");
            let child = PathBuf::from(r"C:\tools\bin\app.exe");
            let verbatim_child = PathBuf::from(r"\\?\C:\tools\bin\app.exe");
            let outside = PathBuf::from(r"C:\other\app.exe");

            assert!(within(&child, &[verbatim_root.clone()]));
            assert!(within(&verbatim_child, &[root.clone()]));
            assert!(within(&verbatim_child, &[verbatim_root]));
            assert!(!within(&outside, &[root]));
        }
        #[cfg(not(windows))]
        {
            let root = PathBuf::from("/tools/bin");
            let child = PathBuf::from("/tools/bin/app");
            let outside = PathBuf::from("/other/app");
            assert!(within(&child, &[root.clone()]));
            assert!(!within(&outside, &[root]));
        }
    }
}
