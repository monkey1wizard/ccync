//! Path location resolution for CCYNC state directories.
//!
//! All functions return `Option<PathBuf>` — `None` when home is unavailable.
//! No file I/O; callers decide whether to create directories.
//! Cross-platform: `USERPROFILE` on Windows, `HOME` on Unix.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::provider::{GenerationId, ProviderSourceKey, StorageLayoutCommit};
use crate::target::ProjectionTargetId;

// ── home ─────────────────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn personal_mcp_input_path(home: &Path) -> PathBuf {
    home.join(".agents").join("mcp.json")
}

/// The effective user home directory used by CCYNC path resolution.
pub fn user_home() -> Option<PathBuf> {
    home_dir()
}

/// The CCYNC home directory (`~/.ccync`).
pub fn ccync_home() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ccync"))
}

/// The effective platform config directory, env-based and semantically aligned
/// with `dirs::config_dir()`: Windows `%APPDATA%` (fallback `home/AppData/Roaming`),
/// Unix `$XDG_CONFIG_HOME` (fallback `home/.config`).
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join("AppData").join("Roaming")))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".config")))
    }
}

/// Portable data root (`~/.agents/ccync`).
pub fn portable_root() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".agents").join("ccync"))
}

/// Machine configuration root (`%APPDATA%/ccync` or `$XDG_CONFIG_HOME/ccync`).
pub fn machine_config_root() -> Option<PathBuf> {
    StorageRoots::resolve().ok().map(|roots| roots.config)
}

/// Durable state root (`%LOCALAPPDATA%/ccync/state` or `$XDG_STATE_HOME/ccync`).
pub fn machine_state_root() -> Option<PathBuf> {
    StorageRoots::resolve().ok().map(|roots| roots.state)
}

/// Disposable cache root (`%LOCALAPPDATA%/ccync/cache` or `$XDG_CACHE_HOME/ccync`).
pub fn machine_cache_root() -> Option<PathBuf> {
    StorageRoots::resolve().ok().map(|roots| roots.cache)
}

/// Validated, write-free storage roots for portable and machine-local data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRoots {
    pub portable: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum RootValidationError {
    #[error("storage root is missing")]
    MissingRoot,
    #[error("storage root must be absolute and nonempty: {0:?}")]
    InvalidRoot(PathBuf),
    #[error("storage roots overlap or alias: {0:?} and {1:?}")]
    OverlappingRoots(PathBuf, PathBuf),
    #[error("cannot inspect existing ancestor of root {0:?}: {1}")]
    AncestorIo(PathBuf, #[source] std::io::Error),
    #[error("scoped destination escapes its validated root: {0:?}")]
    EscapingDestination(PathBuf),
}

/// Read authority selected without creating or changing any storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveLayoutAuthority {
    /// Legacy layout remains active with today's ownership.
    Legacy,
    /// No committed state or legacy data exists yet.
    Fresh,
    /// A valid committed layout selects the new roots.
    Committed,
    /// Conflicting, missing, or corrupt committed evidence blocks mutation.
    MutationBlocked,
}

/// Select effective-layout authority from already-read marker and state data.
/// This function performs no I/O and never upgrades historical ownership.
pub fn effective_layout_authority(
    marker: Option<&StorageLayoutCommit>,
    committed: Option<&StorageLayoutCommit>,
    current_roots: &[(String, PathBuf)],
    committed_state_corrupt: bool,
    legacy_data_present: bool,
) -> EffectiveLayoutAuthority {
    if committed_state_corrupt {
        return EffectiveLayoutAuthority::MutationBlocked;
    }
    match (marker, committed) {
        (None, None) if legacy_data_present => EffectiveLayoutAuthority::Legacy,
        (None, None) => EffectiveLayoutAuthority::Fresh,
        (Some(_), None) | (None, Some(_)) => EffectiveLayoutAuthority::MutationBlocked,
        (Some(marker), Some(state)) => {
            if marker != state || !state.matches_roots(current_roots) {
                EffectiveLayoutAuthority::MutationBlocked
            } else {
                EffectiveLayoutAuthority::Committed
            }
        }
    }
}

/// Read-only storage layout selected for the current machine.
#[derive(Debug)]
pub struct EffectiveLayout {
    authority: EffectiveLayoutAuthority,
    roots: StorageRoots,
    root_error: Option<RootValidationError>,
    home: PathBuf,
}

/// Errors that prevent layout loading from identifying its inspection roots.
#[derive(Debug, thiserror::Error)]
pub enum LayoutLoadError {
    #[error(transparent)]
    RootValidation(#[from] RootValidationError),
    #[error("cannot inspect storage layout at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl EffectiveLayout {
    /// Resolve the current roots and load layout authority without writing.
    pub fn load() -> Result<Self, LayoutLoadError> {
        let (platform, env) = current_root_environment();
        Self::load_for(platform, &env)
    }

    /// Load the selected layout, falling back to legacy paths when root validation fails.
    pub fn load_or_legacy() -> Result<Self, LayoutLoadError> {
        let (platform, env) = current_root_environment();
        match Self::load_for(platform, &env) {
            Ok(layout) => Ok(layout),
            Err(LayoutLoadError::RootValidation(error)) => {
                let roots = match StorageRoots::candidates_for(platform, &env) {
                    Ok(roots) => roots,
                    Err(_) => return Err(error.into()),
                };
                let Some(home) = roots
                    .portable
                    .parent()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
                else {
                    return Err(error.into());
                };
                Ok(Self {
                    authority: EffectiveLayoutAuthority::Legacy,
                    roots,
                    root_error: Some(error),
                    home,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Load using injected platform environment values, primarily for fixtures.
    pub(crate) fn load_for(
        platform: RootPlatform,
        env: &HashMap<String, PathBuf>,
    ) -> Result<Self, LayoutLoadError> {
        let candidates = StorageRoots::candidates_for(platform, env)?;
        let home = candidates
            .portable
            .parent()
            .and_then(Path::parent)
            .expect("portable root derives from home")
            .to_path_buf();
        let sentinel = home.join(".ccync").join("migration.json");
        let state_layout = candidates.state.join("layout.json");
        let root_result = candidates.validate(platform);
        let marker = read_layout_commit(&sentinel);
        let committed = read_layout_commit(&state_layout);
        let corrupt = marker.is_err() || committed.is_err();
        let marker = marker.ok().flatten();
        let committed = committed.ok().flatten();
        let legacy_data_present = legacy_data_present(&home, &candidates)?;

        let (authority, root_error) = match root_result {
            Ok(()) => (
                effective_layout_authority(
                    marker.as_ref(),
                    committed.as_ref(),
                    &layout_root_identities(&candidates),
                    corrupt,
                    legacy_data_present,
                ),
                None,
            ),
            Err(error) => {
                let sentinel_exists = path_exists(&sentinel)?;
                if sentinel_exists {
                    (EffectiveLayoutAuthority::MutationBlocked, Some(error))
                } else if legacy_data_present {
                    (EffectiveLayoutAuthority::Legacy, Some(error))
                } else {
                    return Err(error.into());
                }
            }
        };
        Ok(Self {
            authority,
            roots: candidates,
            root_error,
            home,
        })
    }

    pub fn authority(&self) -> EffectiveLayoutAuthority {
        self.authority
    }

    pub fn root_error(&self) -> Option<&RootValidationError> {
        self.root_error.as_ref()
    }

    pub fn storage_roots(&self) -> &StorageRoots {
        &self.roots
    }

    /// Build root for private state, relocated only after authority commits.
    pub fn state_base(&self) -> PathBuf {
        if self.authority == EffectiveLayoutAuthority::Committed {
            self.roots.state.join("build")
        } else {
            self.legacy_ccync_home().join("build")
        }
    }

    pub fn portable_root(&self) -> PathBuf {
        self.roots.portable.clone()
    }

    pub fn machine_config_root(&self) -> PathBuf {
        self.roots.config.clone()
    }

    pub fn machine_state_root(&self) -> PathBuf {
        self.roots.state.clone()
    }

    pub fn machine_cache_root(&self) -> PathBuf {
        self.roots.cache.clone()
    }

    pub fn plugins_path(&self) -> PathBuf {
        self.object_path("plugins.json", self.roots.config.join("plugins.json"))
    }

    pub fn personal_catalog_path(&self) -> PathBuf {
        self.plugins_path()
    }

    pub fn config_path(&self) -> PathBuf {
        self.object_path("config.json", self.roots.config.join("config.json"))
    }

    pub fn machine_config_path(&self) -> PathBuf {
        self.config_path()
    }

    pub fn agents_md_path(&self) -> PathBuf {
        self.object_path("AGENTS.md", self.roots.config.join("AGENTS.md"))
    }

    pub fn mcp_path(&self) -> PathBuf {
        self.object_path("mcp.json", self.roots.config.join("mcp.json"))
    }

    pub fn mcp_input_path(&self) -> PathBuf {
        personal_mcp_input_path(&self.home)
    }

    pub fn mcp_input_candidates(&self) -> (PathBuf, PathBuf) {
        (personal_mcp_input_path(&self.home), self.mcp_path())
    }

    pub fn tools_path(&self) -> PathBuf {
        self.object_path("tools.json", self.roots.config.join("tools.json"))
    }

    pub fn sources_export_path(&self) -> Option<PathBuf> {
        (self.authority == EffectiveLayoutAuthority::Committed)
            .then(|| self.roots.portable.join("sources.json"))
    }

    pub fn lock_export_path(&self) -> Option<PathBuf> {
        (self.authority == EffectiveLayoutAuthority::Committed)
            .then(|| self.roots.portable.join("lock.json"))
    }

    pub fn build_lock_path(&self) -> PathBuf {
        self.state_base().join("lock.json")
    }

    pub fn tool_state_path(&self) -> PathBuf {
        self.state_base().join("tools").join("state.json")
    }

    pub fn tool_artifacts_root(&self) -> PathBuf {
        self.state_base().join("tools").join("artifacts")
    }

    pub fn tool_operation_journal_path(&self) -> PathBuf {
        self.state_base().join("tools").join("operation.txn.json")
    }

    pub fn cache_path(&self) -> PathBuf {
        if self.authority == EffectiveLayoutAuthority::Committed {
            self.roots.state.join("sources")
        } else {
            self.legacy_ccync_home().join("cache")
        }
    }

    pub fn source_cache_path(&self) -> PathBuf {
        self.cache_path()
    }

    pub fn dist_path(&self) -> PathBuf {
        self.object_path("dist", self.roots.state.join("dist"))
    }

    pub fn backups_path(&self) -> PathBuf {
        self.object_path("backups", self.roots.state.join("backups"))
    }

    pub fn generated_mcp_path(&self) -> PathBuf {
        self.state_base().join("mcp").join("managed.json")
    }

    pub fn routes_path(&self) -> PathBuf {
        self.state_base().join("routes.json")
    }

    pub fn state_lock_path(&self) -> PathBuf {
        self.roots.state.join("lifecycle.lock")
    }

    pub fn legacy_lock_path(&self) -> PathBuf {
        self.legacy_ccync_home()
            .join("build")
            .join("lifecycle.lock")
    }

    pub fn layout_path(&self) -> PathBuf {
        self.roots.state.join("layout.json")
    }

    pub fn migration_journal_path(&self) -> PathBuf {
        self.roots.state.join("migration.json")
    }

    pub fn migration_sentinel_path(&self) -> PathBuf {
        self.legacy_ccync_home().join("migration.json")
    }

    fn legacy_ccync_home(&self) -> PathBuf {
        self.home.join(".ccync")
    }

    fn object_path(&self, relative: &str, committed: PathBuf) -> PathBuf {
        if self.authority == EffectiveLayoutAuthority::Committed {
            committed
        } else {
            self.legacy_ccync_home().join(relative)
        }
    }
}

fn current_root_environment() -> (RootPlatform, HashMap<String, PathBuf>) {
    let env = [
        "USERPROFILE",
        "HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ]
    .into_iter()
    .filter_map(|key| std::env::var_os(key).map(|value| (key.to_string(), PathBuf::from(value))))
    .collect();
    (
        if cfg!(windows) {
            RootPlatform::Windows
        } else {
            RootPlatform::Unix
        },
        env,
    )
}

fn layout_root_identities(roots: &StorageRoots) -> Vec<(String, PathBuf)> {
    vec![
        ("portable".into(), roots.portable.clone()),
        ("config".into(), roots.config.clone()),
        ("state".into(), roots.state.clone()),
        ("cache".into(), roots.cache.clone()),
    ]
}

fn read_layout_commit(path: &Path) -> Result<Option<StorageLayoutCommit>, ()> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

fn path_exists(path: &Path) -> Result<bool, LayoutLoadError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(LayoutLoadError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn legacy_data_present(home: &Path, roots: &StorageRoots) -> Result<bool, LayoutLoadError> {
    let legacy_home = home.join(".ccync");
    for (root, ignored) in [
        (
            &legacy_home,
            Some(vec![
                PathBuf::from("build/lifecycle.lock"),
                PathBuf::from("migration.json"),
            ]),
        ),
        (&roots.portable, None),
        (&roots.config, None),
        (&roots.state, Some(vec![PathBuf::from("lifecycle.lock")])),
    ] {
        if tree_has_data(root, ignored.as_deref().unwrap_or_default())? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn tree_has_data(root: &Path, ignored: &[PathBuf]) -> Result<bool, LayoutLoadError> {
    fn visit(root: &Path, dir: &Path, ignored: &[PathBuf]) -> Result<bool, LayoutLoadError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => {
                return Err(LayoutLoadError::Io {
                    path: dir.to_path_buf(),
                    source,
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|source| LayoutLoadError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("entry is under root");
            if relative == Path::new("install-receipts")
                || ignored.iter().any(|item| relative == item)
            {
                continue;
            }
            let kind = entry.file_type().map_err(|source| LayoutLoadError::Io {
                path: path.clone(),
                source,
            })?;
            if kind.is_dir() {
                if visit(root, &path, ignored)? {
                    return Ok(true);
                }
            } else {
                return Ok(true);
            }
        }
        Ok(false)
    }
    if !path_exists(root)? {
        return Ok(false);
    }
    visit(root, root, ignored)
}

impl StorageRoots {
    /// Resolve defaults and reject roots that physically overlap, without writing.
    pub fn resolve() -> Result<Self, RootValidationError> {
        let values = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(|v| (key.to_string(), PathBuf::from(v))))
        .collect();
        Self::resolve_for(
            if cfg!(windows) {
                RootPlatform::Windows
            } else {
                RootPlatform::Unix
            },
            &values,
        )
    }

    pub(crate) fn resolve_for(
        platform: RootPlatform,
        env: &HashMap<String, PathBuf>,
    ) -> Result<Self, RootValidationError> {
        let roots = Self::candidates_for(platform, env)?;
        roots.validate(platform)?;
        Ok(roots)
    }

    fn candidates_for(
        platform: RootPlatform,
        env: &HashMap<String, PathBuf>,
    ) -> Result<Self, RootValidationError> {
        let home_key = if platform == RootPlatform::Windows {
            "USERPROFILE"
        } else {
            "HOME"
        };
        let home = env
            .get(home_key)
            .cloned()
            .ok_or(RootValidationError::MissingRoot)?;
        let (config_base, state_base, cache_base) = match platform {
            RootPlatform::Windows => (
                env.get("APPDATA")
                    .cloned()
                    .unwrap_or_else(|| home.join("AppData").join("Roaming")),
                env.get("LOCALAPPDATA")
                    .cloned()
                    .unwrap_or_else(|| home.join("AppData").join("Local")),
                env.get("LOCALAPPDATA")
                    .cloned()
                    .unwrap_or_else(|| home.join("AppData").join("Local")),
            ),
            RootPlatform::Unix => (
                env.get("XDG_CONFIG_HOME")
                    .filter(|p| !p.as_os_str().is_empty())
                    .cloned()
                    .unwrap_or_else(|| home.join(".config")),
                env.get("XDG_STATE_HOME")
                    .filter(|p| !p.as_os_str().is_empty())
                    .cloned()
                    .unwrap_or_else(|| home.join(".local").join("state")),
                env.get("XDG_CACHE_HOME")
                    .filter(|p| !p.as_os_str().is_empty())
                    .cloned()
                    .unwrap_or_else(|| home.join(".cache")),
            ),
        };
        let roots = Self {
            portable: home.join(".agents").join("ccync"),
            config: config_base.join("ccync"),
            state: if platform == RootPlatform::Windows {
                state_base.join("ccync").join("state")
            } else {
                state_base.join("ccync")
            },
            cache: if platform == RootPlatform::Windows {
                cache_base.join("ccync").join("cache")
            } else {
                cache_base.join("ccync")
            },
        };
        Ok(roots)
    }

    fn validate(&self, platform: RootPlatform) -> Result<(), RootValidationError> {
        let list = [&self.portable, &self.config, &self.state, &self.cache];
        let canonical: Vec<_> = list
            .iter()
            .map(|p| canonical_root_candidate(p, platform))
            .collect::<Result<_, _>>()?;
        for i in 0..canonical.len() {
            for j in i + 1..canonical.len() {
                if roots_may_alias(&canonical[i], &canonical[j]) {
                    return Err(RootValidationError::OverlappingRoots(
                        list[i].clone(),
                        list[j].clone(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn scoped_root(&self, root: crate::provider::PathRoot) -> PathBuf {
        let home = self
            .portable
            .parent()
            .and_then(Path::parent)
            .expect("portable root derives from home");
        match root {
            crate::provider::PathRoot::Portable => self.portable.clone(),
            crate::provider::PathRoot::MachineConfig => self.config.clone(),
            crate::provider::PathRoot::MachineState => self.state.clone(),
            crate::provider::PathRoot::MachineCache => self.cache.clone(),
            crate::provider::PathRoot::UserHome => home.to_path_buf(),
            crate::provider::PathRoot::CcyncHome | crate::provider::PathRoot::LegacyCcyncHome => {
                home.join(".ccync")
            }
            crate::provider::PathRoot::ConfigDir => self
                .config
                .parent()
                .expect("config root has base")
                .to_path_buf(),
            crate::provider::PathRoot::LegacyBuild => home.join(".ccync").join("build"),
        }
    }
}

fn canonical_root_candidate(
    path: &Path,
    platform: RootPlatform,
) -> Result<PathBuf, RootValidationError> {
    let candidate = canonical_candidate_for(path, platform)?;
    let mut ancestor = path;
    while !ancestor.exists() {
        let component = ancestor
            .file_name()
            .ok_or_else(|| RootValidationError::InvalidRoot(path.to_path_buf()))?;
        if !component
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(RootValidationError::InvalidRoot(path.to_path_buf()));
        }
        ancestor = ancestor
            .parent()
            .ok_or_else(|| RootValidationError::InvalidRoot(path.to_path_buf()))?;
    }
    Ok(candidate)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootPlatform {
    Windows,
    Unix,
}

fn roots_may_alias(a: &Path, b: &Path) -> bool {
    let normalize = |path: &Path| {
        let raw = path.to_string_lossy().replace('\\', "/");
        let mut parts = Vec::new();
        for part in raw.split('/') {
            if part.is_empty() {
                continue;
            }
            let part = part.trim_end_matches(['.', ' ']);
            parts.push(part.to_lowercase());
        }
        parts.join("/")
    };
    let a = normalize(a);
    let b = normalize(b);
    a == b
        || a.strip_prefix(&b).is_some_and(|tail| tail.starts_with('/'))
        || b.strip_prefix(&a).is_some_and(|tail| tail.starts_with('/'))
}

pub(crate) fn canonical_candidate(path: &Path) -> Result<PathBuf, RootValidationError> {
    canonical_candidate_for(
        path,
        if cfg!(windows) {
            RootPlatform::Windows
        } else {
            RootPlatform::Unix
        },
    )
}

fn canonical_candidate_for(
    path: &Path,
    platform: RootPlatform,
) -> Result<PathBuf, RootValidationError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(RootValidationError::InvalidRoot(path.to_path_buf()));
    }
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(RootValidationError::AncestorIo(path.to_path_buf(), error));
            }
        }
        let name = ancestor
            .file_name()
            .ok_or_else(|| RootValidationError::InvalidRoot(path.to_path_buf()))?;
        if platform == RootPlatform::Windows {
            let component = name.to_string_lossy();
            if component.ends_with('.') || component.ends_with(' ') {
                return Err(RootValidationError::InvalidRoot(path.to_path_buf()));
            }
        }
        suffix.push(name.to_os_string());
        ancestor = ancestor
            .parent()
            .ok_or_else(|| RootValidationError::InvalidRoot(path.to_path_buf()))?;
    }
    let mut resolved = ancestor
        .canonicalize()
        .map_err(|e| RootValidationError::AncestorIo(path.to_path_buf(), e))?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err(RootValidationError::InvalidRoot(path.to_path_buf()));
        }
    }
    Ok(resolved)
}

pub(crate) fn contained_physically(
    root: &Path,
    child: &Path,
) -> Result<PathBuf, RootValidationError> {
    let physical_root = canonical_candidate(root)?;
    let physical_child = canonical_candidate(child)?;
    if !physical_child.starts_with(&physical_root) {
        return Err(RootValidationError::EscapingDestination(
            child.to_path_buf(),
        ));
    }
    Ok(physical_child)
}

// ── config paths ─────────────────────────────────────────────────────────────

/// `~/.ccync/config.json`
pub fn machine_config_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("config.json"))
}

/// `~/.ccync/AGENTS.md`
pub fn ccync_agents_md_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("AGENTS.md"))
}

/// `~/.agents/mcp.json`
///
/// Canonical user-owned MCP input file. Resolved only; callers own any read or write.
pub fn mcp_input_path() -> Option<PathBuf> {
    home_dir().map(|home| personal_mcp_input_path(&home))
}

// ── state paths ───────────────────────────────────────────────────────────────

// ── plugin paths ──────────────────────────────────────────────────────────────

/// `~/.ccync/plugins`
pub fn ccync_plugins_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("plugins"))
}

/// Canonical render root for the CCYNC-managed plugin tree (`~/.ccync/build/render`).
///
/// Shared by install/sync so they resolve the same root the projection layer uses.
pub fn canonical_plugin_root() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("render"))
}

/// `~/.ccync/build/render/providers`
///
/// Root of immutable per-provider render generations.
pub fn provider_render_root() -> Option<PathBuf> {
    canonical_plugin_root().map(|r| r.join("providers"))
}

/// `~/.ccync/build/render/providers/<source-key>/<generation>`
///
/// Both segments are validated types, so the result always stays under
/// [`provider_render_root`].
pub fn provider_generation_path(
    source: &ProviderSourceKey,
    generation: &GenerationId,
) -> Option<PathBuf> {
    provider_render_root().map(|r| r.join(source.as_str()).join(generation.as_str()))
}

/// `~/.ccync/build/projection.txn.json`
///
/// Provider lifecycle intent journal, recorded before native registration or
/// live path replacement. Separate from the MCP projection journal under
/// `build/mcp/`.
pub fn provider_lifecycle_journal_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("projection.txn.json"))
}

// ── cache paths ──────────────────────────────────────────────────────────────

/// `~/.ccync/cache`
pub fn ccync_cache_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("cache"))
}

// ── data paths ───────────────────────────────────────────────────────────────

/// `~/.ccync/data`
///
/// plugin-owned persistent data, created by the render, never pruned by ccync.
pub fn ccync_data_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("data"))
}

// ── build / output paths ──────────────────────────────────────────────────────

/// `~/.ccync/build`
///
/// Root of all derived OUTPUT artifacts. Rebuildable by `ccync sync`.
pub fn ccync_build_root() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("build"))
}

/// `~/.ccync/tools.json`
pub fn tools_json_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("tools.json"))
}

/// `~/.ccync/build/tools/state.json`
pub fn tool_state_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("tools").join("state.json"))
}

/// `~/.ccync/build/tools/artifacts`
pub fn tool_artifacts_root() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("tools").join("artifacts"))
}

/// `~/.ccync/build/tools/operation.txn.json`
pub fn tool_operation_journal_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("tools").join("operation.txn.json"))
}

/// `~/.ccync/build/lifecycle.lock`
pub fn lifecycle_lock_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("lifecycle.lock"))
}

/// `~/.ccync/migration.json`, retained as the explicit legacy migration sentinel.
pub fn legacy_migration_sentinel_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("migration.json"))
}

/// `~/.ccync/install-receipts/ccync.json`
pub fn install_receipt_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("install-receipts").join("ccync.json"))
}

/// `~/.ccync/build/active/<provider>`
///
/// Per-provider projected output root under the build tree.
pub fn ccync_active_provider_path(provider: &str) -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("active").join(provider))
}

// ── personal paths ───────────────────────────────────────────────────────────

/// `~/.ccync/plugins.json`
///
/// Personal plugin catalog (catalog-v1 entry schema). Absent when no personal
/// plugins have been added; resolver treats absence as an empty additive set.
pub fn personal_catalog_path() -> Option<PathBuf> {
    ccync_home().map(|h| h.join("plugins.json"))
}

/// `~/.ccync/build/lock.json`
///
/// The canonical catalog lockfile written by the resolve step and consumed
/// read-only by the install/projection layers.
pub fn plugins_lock_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("lock.json"))
}

// ── agent state paths ─────────────────────────────────────────────────────────

/// `~/.claude/plugins/installed_plugins.json`
///
/// Claude installed plugin state (v2 schema: `{plugins:{"name@mkt":[...]}}`).
pub fn claude_installed_plugins_path() -> Option<PathBuf> {
    home_dir().map(|h| {
        h.join(".claude")
            .join("plugins")
            .join("installed_plugins.json")
    })
}

/// `~/.claude/plugins/known_marketplaces.json`
///
/// Claude known marketplaces list.
pub fn claude_known_marketplaces_path() -> Option<PathBuf> {
    home_dir().map(|h| {
        h.join(".claude")
            .join("plugins")
            .join("known_marketplaces.json")
    })
}

/// `~/.claude.json`
///
/// Claude top-level config (contains `mcpServers` entries).
pub fn claude_config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude.json"))
}

/// `~/.claude/.mcp.json`
///
/// Claude's secondary MCP definition file. `claude_config_path()`
/// (`~/.claude.json`) is read first; this file fills in only names missing
/// from that store.
pub fn claude_mcp_json_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude").join(".mcp.json"))
}

/// `~/.codex/config.toml`
///
/// Codex config file; CCYNC reads the MCP section only.
pub fn codex_config_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".codex").join("config.toml"))
}

/// `~/.claude/skills`
///
/// Claude's loose-skill directory; each child dir with a `SKILL.md` is one skill.
pub fn claude_skills_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude").join("skills"))
}

/// `~/.agents/skills`
///
/// Shared skill directory read by Codex (and opencode/copilot); each child dir
/// with a `SKILL.md` is one skill.
pub fn shared_agents_skills_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".agents").join("skills"))
}

// ── user-scope instruction paths ─────────────────────────────────────────────

/// Resolve Claude's legacy user-scope instruction file for migration or cleanup.
///
/// Returns `None` when the user home directory is unavailable.
pub fn legacy_claude_user_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".claude").join("CLAUDE.md"))
}

fn codex_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".codex").join("AGENTS.md"))
}

fn copilot_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".copilot").join("AGENTS.md"))
}

fn opencode_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".config").join("opencode").join("AGENTS.md"))
}

fn antigravity_user_scope_instruction_path() -> Option<PathBuf> {
    user_home().map(|h| h.join(".gemini").join("AGENTS.md"))
}

/// Resolve the current user-scope instruction write path for a projection target.
///
/// Claude has no current write path. Other targets resolve to an `AGENTS.md`
/// path, with the three Antigravity faces sharing one file. Returns `None` when
/// the target has no current write path or the user home directory is unavailable.
pub fn user_scope_instruction_write_path(target: ProjectionTargetId) -> Option<PathBuf> {
    match target {
        ProjectionTargetId::Claude => None,
        ProjectionTargetId::Codex => codex_user_scope_instruction_path(),
        ProjectionTargetId::Copilot => copilot_user_scope_instruction_path(),
        ProjectionTargetId::Opencode => opencode_user_scope_instruction_path(),
        ProjectionTargetId::AgyCli | ProjectionTargetId::AgyIde | ProjectionTargetId::AgyGui => {
            antigravity_user_scope_instruction_path()
        }
    }
}

// ── generated paths ───────────────────────────────────────────────────────────

/// `~/.ccync/build/mcp/managed.json`
///
/// MCP managed manifest written by `ccync sync` and consumed by MCP projection.
pub fn generated_mcp_path() -> Option<PathBuf> {
    ccync_build_root().map(|b| b.join("mcp").join("managed.json"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_load_reports_legacy_home_directory_read_error() {
        let home = tempfile::tempdir().unwrap();
        let legacy_home = home.path().join(".ccync");
        std::fs::write(&legacy_home, b"data").unwrap();
        let env = [
            ("USERPROFILE", home.path().to_path_buf()),
            ("HOME", home.path().to_path_buf()),
            ("APPDATA", home.path().join("roaming")),
            ("LOCALAPPDATA", home.path().join("local")),
            ("XDG_CONFIG_HOME", home.path().join("config")),
            ("XDG_STATE_HOME", home.path().join("state")),
            ("XDG_CACHE_HOME", home.path().join("cache")),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
        let platform = if cfg!(windows) {
            RootPlatform::Windows
        } else {
            RootPlatform::Unix
        };

        assert!(matches!(
            EffectiveLayout::load_for(platform, &env),
            Err(LayoutLoadError::Io { path, .. }) if path == legacy_home
        ));
    }

    #[test]
    fn mcp_candidates_use_injected_home_and_active_old_layout_without_io() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("absent-home");
        let roots = StorageRoots {
            portable: home.join(".agents/ccync"),
            config: home.join("config/ccync"),
            state: home.join("state/ccync"),
            cache: home.join("cache/ccync"),
        };
        for authority in [
            EffectiveLayoutAuthority::Legacy,
            EffectiveLayoutAuthority::Committed,
        ] {
            let layout = EffectiveLayout {
                authority,
                roots: roots.clone(),
                root_error: None,
                home: home.clone(),
            };
            let expected_old = if authority == EffectiveLayoutAuthority::Committed {
                roots.config.join("mcp.json")
            } else {
                home.join(".ccync/mcp.json")
            };
            assert_eq!(
                layout.mcp_input_candidates(),
                (home.join(".agents/mcp.json"), expected_old.clone())
            );
            assert_eq!(layout.mcp_path(), expected_old);
            assert_eq!(layout.mcp_input_path(), home.join(".agents/mcp.json"));
            assert!(!home.exists());
        }
    }

    #[test]
    fn layout_authority_table_selects_one_read_authority_or_blocks_mutation() {
        use EffectiveLayoutAuthority::*;

        let portable = std::env::current_dir()
            .expect("current directory")
            .join("portable");
        let roots = vec![("portable".to_string(), portable)];
        let commit = StorageLayoutCommit {
            layout_version: 1,
            roots: vec![crate::provider::LayoutRootIdentity {
                name: "portable".to_string(),
                path: roots[0].1.clone(),
            }],
        };
        let mismatch = StorageLayoutCommit {
            layout_version: 1,
            roots: vec![crate::provider::LayoutRootIdentity {
                name: "portable".to_string(),
                path: roots[0].1.join("different"),
            }],
        };

        let rows = [
            (
                "fresh without marker, state, or legacy data",
                None,
                None,
                false,
                false,
                Fresh,
            ),
            (
                "legacy data without marker or state",
                None,
                None,
                false,
                true,
                Legacy,
            ),
            (
                "matching marker and commit",
                Some(&commit),
                Some(&commit),
                false,
                false,
                Committed,
            ),
            (
                "marker without state",
                Some(&commit),
                None,
                false,
                false,
                MutationBlocked,
            ),
            (
                "state without marker",
                None,
                Some(&commit),
                false,
                false,
                MutationBlocked,
            ),
            ("corrupt state", None, None, true, false, MutationBlocked),
            (
                "marker with corrupt state",
                Some(&commit),
                None,
                true,
                false,
                MutationBlocked,
            ),
            (
                "marker disagreement",
                Some(&commit),
                Some(&mismatch),
                false,
                false,
                MutationBlocked,
            ),
            (
                "committed root identity mismatch",
                Some(&commit),
                Some(&commit),
                false,
                false,
                MutationBlocked,
            ),
        ];
        for (label, marker, state, corrupt, legacy_data_present, expected) in rows {
            let current_roots = if label == "committed root identity mismatch" {
                vec![("portable".to_string(), roots[0].1.join("moved"))]
            } else {
                roots.clone()
            };
            assert_eq!(
                effective_layout_authority(
                    marker,
                    state,
                    &current_roots,
                    corrupt,
                    legacy_data_present,
                ),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn layout_authority_fresh_legacy() {
        use EffectiveLayoutAuthority::*;

        let roots = Vec::new();
        let rows = [
            ("no marker, state, or legacy data", false, Fresh),
            ("legacy data without marker or state", true, Legacy),
        ];
        for (label, legacy_data_present, expected) in rows {
            assert_eq!(
                effective_layout_authority(None, None, &roots, false, legacy_data_present),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn load_or_legacy_uses_legacy_paths_for_invalid_roots_and_matches_load_when_valid() {
        let _guard = crate::test_support::env_guard();
        struct RestoreEnv([(&'static str, Option<std::ffi::OsString>); 7]);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                for (key, value) in &self.0 {
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(key, value),
                            None => std::env::remove_var(key),
                        }
                    }
                }
            }
        }
        let _restore = RestoreEnv(
            [
                "USERPROFILE",
                "HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ]
            .map(|key| (key, std::env::var_os(key))),
        );
        let home = tempfile::tempdir().unwrap();
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        std::env::set_var(home_var, home.path());
        for key in [
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            std::env::remove_var(key);
        }

        if cfg!(windows) {
            let shared = home.path().join("shared");
            std::env::set_var("APPDATA", &shared);
            std::env::set_var("LOCALAPPDATA", &shared);
        } else {
            let shared = home.path().join("shared");
            std::env::set_var("XDG_CONFIG_HOME", &shared);
            std::env::set_var("XDG_STATE_HOME", &shared);
        }

        let legacy = EffectiveLayout::load_or_legacy().unwrap();
        assert_eq!(legacy.authority(), EffectiveLayoutAuthority::Legacy);
        assert!(legacy.root_error().is_some());
        assert_eq!(legacy.state_base(), home.path().join(".ccync/build"));
        assert_eq!(legacy.tools_path(), home.path().join(".ccync/tools.json"));
        assert_eq!(
            legacy.tool_operation_journal_path(),
            home.path().join(".ccync/build/tools/operation.txn.json")
        );
        assert!(!home.path().join(".ccync").exists());

        for key in [
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            std::env::remove_var(key);
        }
        let loaded = EffectiveLayout::load().unwrap();
        let loaded_or_legacy = EffectiveLayout::load_or_legacy().unwrap();
        assert_eq!(loaded.authority(), loaded_or_legacy.authority());
        assert_eq!(loaded.storage_roots(), loaded_or_legacy.storage_roots());
        assert_eq!(loaded.state_base(), loaded_or_legacy.state_base());
        assert_eq!(loaded.tools_path(), loaded_or_legacy.tools_path());
    }

    #[test]
    fn ccync_home_ends_with_dot_ccync() {
        if let Some(h) = ccync_home() {
            assert_eq!(h.file_name().and_then(|n| n.to_str()), Some(".ccync"));
        }
    }

    #[test]
    fn machine_config_path_ends_with_config_json() {
        if let (Some(p), Some(home)) = (machine_config_path(), ccync_home()) {
            assert_eq!(
                p.parent(),
                Some(home.as_path()),
                "config.json must be directly under ccync_home, no config/ subdir"
            );
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("config.json"));
        }
    }

    #[test]
    fn ccync_plugins_root_leaf_is_plugins() {
        if let Some(p) = ccync_plugins_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("plugins"));
        }
    }

    #[test]
    fn ccync_cache_root_leaf_is_cache() {
        if let Some(p) = ccync_cache_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("cache"));
        }
    }

    #[test]
    fn ccync_data_root_leaf_is_data() {
        if let Some(p) = ccync_data_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("data"));
        }
    }

    #[test]
    fn ccync_data_root_resolves_under_set_home_and_none_without() {
        let _guard = crate::test_support::env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);

        let fake_home = tempfile::TempDir::new().unwrap();
        std::env::set_var(var, fake_home.path());
        assert_eq!(
            ccync_data_root(),
            Some(fake_home.path().join(".ccync").join("data"))
        );

        std::env::remove_var(var);
        assert_eq!(ccync_data_root(), None);

        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn canonical_plugin_root_under_build_render() {
        if let Some(p) = canonical_plugin_root() {
            let s = p.to_string_lossy();
            assert!(s.contains("build"), "must be under build: {s}");
            assert!(
                s.ends_with("render") || s.ends_with("render\\"),
                "must end with render: {s}"
            );
        }
    }

    #[test]
    fn ccync_build_root_leaf_is_build() {
        if let Some(p) = ccync_build_root() {
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("build"));
        }
    }

    #[test]
    fn tool_and_lifecycle_paths_resolve_under_temporary_home_and_none_without_home() {
        let _guard = crate::test_support::env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = tempfile::TempDir::new().unwrap();
        std::env::set_var(var, fake_home.path());

        assert_eq!(
            tools_json_path(),
            Some(fake_home.path().join(".ccync/tools.json"))
        );
        assert_eq!(
            tool_state_path(),
            Some(fake_home.path().join(".ccync/build/tools/state.json"))
        );
        assert_eq!(
            tool_artifacts_root(),
            Some(fake_home.path().join(".ccync/build/tools/artifacts"))
        );
        assert_eq!(
            tool_operation_journal_path(),
            Some(
                fake_home
                    .path()
                    .join(".ccync/build/tools/operation.txn.json")
            )
        );
        assert_eq!(
            lifecycle_lock_path(),
            Some(fake_home.path().join(".ccync/build/lifecycle.lock"))
        );
        assert_eq!(
            install_receipt_path(),
            Some(fake_home.path().join(".ccync/install-receipts/ccync.json"))
        );

        std::env::remove_var(var);
        assert_eq!(tools_json_path(), None);
        assert_eq!(tool_state_path(), None);
        assert_eq!(tool_artifacts_root(), None);
        assert_eq!(tool_operation_journal_path(), None);
        assert_eq!(lifecycle_lock_path(), None);
        assert_eq!(install_receipt_path(), None);

        match prior {
            Some(value) => std::env::set_var(var, value),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn mcp_input_path_resolves_under_temporary_home_and_none_without_home() {
        let _guard = crate::test_support::env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = tempfile::TempDir::new().unwrap();
        std::env::set_var(var, fake_home.path());

        assert_eq!(
            mcp_input_path(),
            Some(fake_home.path().join(".agents").join("mcp.json"))
        );
        assert!(
            !fake_home.path().join(".agents").exists(),
            "resolving the path must not create directories"
        );

        std::env::remove_var(var);
        assert_eq!(mcp_input_path(), None);

        match prior {
            Some(value) => std::env::set_var(var, value),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn ccync_active_provider_path_under_build_active() {
        if let Some(p) = ccync_active_provider_path("claude") {
            let s = p.to_string_lossy();
            assert!(s.contains("build"), "must be under build: {s}");
            assert!(s.contains("active"), "must contain active: {s}");
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("claude"));
        }
    }

    #[test]
    fn claude_installed_plugins_path_under_claude_plugins() {
        if let Some(p) = claude_installed_plugins_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".claude"), "must be under .claude: {s}");
            assert!(
                s.ends_with("installed_plugins.json"),
                "must end with installed_plugins.json: {s}"
            );
        }
    }

    #[test]
    fn claude_known_marketplaces_path_under_claude_plugins() {
        if let Some(p) = claude_known_marketplaces_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".claude"), "must be under .claude: {s}");
            assert!(
                s.ends_with("known_marketplaces.json"),
                "must end with known_marketplaces.json: {s}"
            );
        }
    }

    #[test]
    fn claude_config_path_ends_with_claude_json() {
        if let Some(p) = claude_config_path() {
            let s = p.to_string_lossy();
            assert!(
                s.ends_with(".claude.json"),
                "must end with .claude.json: {s}"
            );
        }
    }

    #[test]
    fn claude_mcp_json_path_ends_with_claude_mcp_json() {
        if let Some(p) = claude_mcp_json_path() {
            let s = p.to_string_lossy();
            assert!(
                s.ends_with(".claude/.mcp.json") || s.ends_with(".claude\\.mcp.json"),
                "must end with .claude/.mcp.json: {s}"
            );
        }
    }

    #[test]
    fn codex_config_path_under_codex_dir() {
        if let Some(p) = codex_config_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".codex"), "must be under .codex: {s}");
            assert!(s.ends_with("config.toml"), "must end with config.toml: {s}");
        }
    }

    #[test]
    fn all_agent_state_paths_under_home() {
        if let Some(home) = home_dir() {
            for (label, path) in [
                ("claude_installed_plugins", claude_installed_plugins_path()),
                (
                    "claude_known_marketplaces",
                    claude_known_marketplaces_path(),
                ),
                ("claude_config", claude_config_path()),
                ("claude_mcp_json", claude_mcp_json_path()),
                ("codex_config", codex_config_path()),
            ] {
                if let Some(p) = path {
                    assert!(
                        p.starts_with(&home),
                        "{label} ({}) must be under home ({})",
                        p.display(),
                        home.display()
                    );
                }
            }
        }
    }

    #[test]
    fn generated_mcp_path_under_build_mcp() {
        if let Some(p) = generated_mcp_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("build"), "must be under build: {s}");
            assert!(s.contains("mcp"), "must contain mcp: {s}");
            assert!(
                s.ends_with("managed.json"),
                "must end with managed.json: {s}"
            );
        }
    }

    #[test]
    fn personal_catalog_path_directly_under_ccync_home() {
        if let (Some(p), Some(home)) = (personal_catalog_path(), ccync_home()) {
            assert_eq!(
                p.parent(),
                Some(home.as_path()),
                "plugins.json must be directly under ccync_home"
            );
            assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("plugins.json"));
        }
    }

    #[test]
    fn plugins_lock_path_under_build() {
        if let Some(p) = plugins_lock_path() {
            let s = p.to_string_lossy();
            assert!(s.contains("build"), "must be under build: {s}");
            assert!(s.ends_with("lock.json"), "must end with lock.json: {s}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn config_dir_prefers_appdata_env() {
        let _guard = crate::test_support::env_guard();
        let prior = std::env::var_os("APPDATA");
        std::env::set_var("APPDATA", r"C:\injected\AppData\Roaming");
        let result = config_dir();
        match prior {
            Some(v) => std::env::set_var("APPDATA", v),
            None => std::env::remove_var("APPDATA"),
        }
        assert_eq!(result, Some(PathBuf::from(r"C:\injected\AppData\Roaming")));
    }

    #[cfg(windows)]
    #[test]
    fn config_dir_falls_back_to_home_appdata_roaming_without_appdata_env() {
        let _guard = crate::test_support::env_guard();
        let prior = std::env::var_os("APPDATA");
        std::env::remove_var("APPDATA");
        let result = config_dir();
        if let Some(v) = prior {
            std::env::set_var("APPDATA", v);
        }
        if let Some(home) = home_dir() {
            assert_eq!(result, Some(home.join("AppData").join("Roaming")));
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn config_dir_prefers_xdg_config_home_env() {
        let _guard = crate::test_support::env_guard();
        let prior = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/injected/config");
        let result = config_dir();
        match prior {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        assert_eq!(result, Some(PathBuf::from("/injected/config")));
    }

    #[cfg(not(windows))]
    #[test]
    fn config_dir_falls_back_to_home_dot_config_without_xdg_env() {
        let _guard = crate::test_support::env_guard();
        let prior = std::env::var_os("XDG_CONFIG_HOME");
        std::env::remove_var("XDG_CONFIG_HOME");
        let result = config_dir();
        if let Some(v) = prior {
            std::env::set_var("XDG_CONFIG_HOME", v);
        }
        if let Some(home) = home_dir() {
            assert_eq!(result, Some(home.join(".config")));
        }
    }

    #[test]
    fn all_paths_descend_from_ccync_home() {
        if let Some(home) = ccync_home() {
            for (label, path) in [
                ("plugins_root", ccync_plugins_root()),
                ("cache_root", ccync_cache_root()),
                ("data_root", ccync_data_root()),
                ("build_root", ccync_build_root()),
                ("machine_config", machine_config_path()),
                ("generated_mcp", generated_mcp_path()),
                ("plugins_lock_path", plugins_lock_path()),
                ("canonical_plugin_root", canonical_plugin_root()),
                ("provider_render_root", provider_render_root()),
                (
                    "provider_lifecycle_journal",
                    provider_lifecycle_journal_path(),
                ),
            ] {
                if let Some(p) = path {
                    assert!(
                        p.starts_with(&home),
                        "{label} ({}) must be under ccync_home ({})",
                        p.display(),
                        home.display()
                    );
                }
            }
        }
    }
}
