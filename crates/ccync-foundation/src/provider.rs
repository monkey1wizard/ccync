//! Provider identity and native-ownership contracts.
//!
//! Pure data and validation layer shared by render, projection, lifecycle,
//! and inspection code. It describes:
//!
//! - which provider (source) and original plugin a generated component came
//!   from ([`ProviderSourceKey`], [`ComponentIdentity`]);
//! - the immutable provider generation a target loads ([`ProviderGeneration`]);
//! - per-target qualified aliases for flat adapters ([`TargetAlias`]);
//! - what was observed in a native loader ([`NativeObservation`]);
//! - pending intent recorded before a native mutation ([`PendingIntent`],
//!   persisted in [`ProviderLifecycleJournal`]) and committed native ownership
//!   stored in the lockfile namespace [`PROVIDER_LIFECYCLE_NAMESPACE`]
//!   ([`ProviderLifecycleLock`]).
//!
//! Ownership is never inferred from names or content: a record without
//! explicit attribution is [`OwnershipState::Unowned`]. No file I/O happens
//! here; callers read and write documents and pass `serde_json::Value`s or
//! strings in and out.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::pins::entry_pin;
use crate::target::{ClientFace, ProjectionTargetId};

/// Lockfile namespace that holds committed native provider ownership.
pub const PROVIDER_LIFECYCLE_NAMESPACE: &str = "_providerLifecycle";

/// Schema version written to the `_providerLifecycle` lock namespace.
pub const PROVIDER_LIFECYCLE_SCHEMA_VERSION: u32 = 1;

/// Schema version written to the provider lifecycle intent journal.
pub const PROVIDER_JOURNAL_SCHEMA_VERSION: u32 = 3;

/// Schema version for portable source intent documents.
pub const PORTABLE_SOURCE_SCHEMA_VERSION: u32 = 1;

/// Upper bound for a single path-segment identifier (source key, generation,
/// alias). Keeps generated paths well inside Windows path limits.
const MAX_SEGMENT_LEN: usize = 128;

/// Characters that are invalid in a Windows file name, in addition to path
/// separators and control characters.
const RESERVED_SEGMENT_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// Windows reserved device names; invalid as a file stem on Windows and
/// rejected everywhere so generated trees stay portable.
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

// ── errors ───────────────────────────────────────────────────────────────────

/// Error raised when a provider contract value fails validation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProviderContractError {
    /// An identifier used as a single path segment is unsafe.
    #[error("unsafe {field} {value:?}: {reason}")]
    UnsafeSegment {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    /// A required text field is empty or contains control characters.
    #[error("invalid {field} {value:?}: {reason}")]
    InvalidField {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    /// A relative path is absolute, has a drive prefix, or traverses upward.
    #[error("path {path:?} escapes its root: {reason}")]
    PathOutsideRoot { path: String, reason: &'static str },
    /// A persisted document declares a schema version this build cannot read.
    #[error("unsupported {document} schema version {found} (supported: {supported})")]
    UnsupportedSchemaVersion {
        document: &'static str,
        found: String,
        supported: u32,
    },
    /// A persisted document has the wrong shape.
    #[error("malformed {document}: {message}")]
    Malformed {
        document: &'static str,
        message: String,
    },
}

// ── segment validation ───────────────────────────────────────────────────────

/// Validate a value that becomes exactly one path segment on every platform.
fn validate_segment(field: &'static str, value: &str) -> Result<(), ProviderContractError> {
    let fail = |reason: &'static str| ProviderContractError::UnsafeSegment {
        field,
        value: value.to_string(),
        reason,
    };
    if value.is_empty() {
        return Err(fail("must not be empty"));
    }
    if value == "." || value == ".." {
        return Err(fail("must not be a relative directory reference"));
    }
    if value.len() > MAX_SEGMENT_LEN {
        return Err(fail("exceeds 128 bytes"));
    }
    if value.contains(['/', '\\']) {
        return Err(fail("must not contain path separators"));
    }
    if value.chars().any(char::is_control) {
        return Err(fail("must not contain control characters"));
    }
    if value.contains(RESERVED_SEGMENT_CHARS) {
        return Err(fail(
            "must not contain drive prefixes or reserved characters",
        ));
    }
    if value.ends_with('.') || value.ends_with(' ') || value.starts_with(' ') {
        return Err(fail(
            "must not start with a space or end with a dot or space",
        ));
    }
    if is_reserved_device_name(value) {
        return Err(fail("must not be a reserved device name"));
    }
    Ok(())
}

/// Whether a single path component names a Windows device (`NUL`,
/// `nul.txt`, `NUL .json`, ...). Windows resolves such components to the
/// device regardless of extension or trailing spaces before the dot.
fn is_reserved_device_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let stem = stem.trim_end_matches(' ');
    RESERVED_DEVICE_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// Reject leading or trailing whitespace in a text field.
fn validate_trimmed(field: &'static str, value: &str) -> Result<(), ProviderContractError> {
    validate_text(field, value)?;
    if value.trim() != value {
        return Err(ProviderContractError::InvalidField {
            field,
            value: value.to_string(),
            reason: "must not have leading or trailing whitespace",
        });
    }
    Ok(())
}

/// Validate a non-empty, control-free text field.
fn validate_text(field: &'static str, value: &str) -> Result<(), ProviderContractError> {
    let fail = |reason: &'static str| ProviderContractError::InvalidField {
        field,
        value: value.to_string(),
        reason,
    };
    if value.trim().is_empty() {
        return Err(fail("must not be empty"));
    }
    if value.chars().any(char::is_control) {
        return Err(fail("must not contain control characters"));
    }
    Ok(())
}

/// Declare a validated string newtype with serde support that rejects
/// invalid values during deserialization.
macro_rules! validated_string {
    ($(#[$meta:meta])* $name:ident, $validate:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap a value.
            pub fn new(value: impl Into<String>) -> Result<Self, ProviderContractError> {
                let value = value.into();
                ($validate)(value.as_str())?;
                Ok(Self(value))
            }

            /// The validated value.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = ProviderContractError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

validated_string!(
    /// Stable provider key derived from source identity (never display text).
    ///
    /// Used as one path segment under the provider render root and in native
    /// publisher roots, so it must be a safe single segment on every platform.
    ProviderSourceKey,
    |v| validate_segment("provider source key", v)
);

validated_string!(
    /// Stable key used to resolve a machine-local source path privately.
    SourceBindingKey,
    |v| validate_segment("source binding key", v)
);

validated_string!(
    /// Immutable generation identifier for a rendered provider package.
    GenerationId,
    |v| validate_segment("generation id", v)
);

validated_string!(
    /// Content digest of a generated provider package (for example
    /// `sha256:<hex>`).
    PackageDigest,
    |v: &str| {
        validate_text("package digest", v)?;
        if v.chars().any(char::is_whitespace) {
            return Err(ProviderContractError::InvalidField {
                field: "package digest",
                value: v.to_string(),
                reason: "must not contain whitespace",
            });
        }
        Ok(())
    }
);

validated_string!(
    /// Exact native selector for a registration (for example
    /// `demo@ccync-marketplace`) or a native path selector.
    NativeSelector,
    |v| validate_trimmed("native selector", v)
);

validated_string!(
    /// Client scope that a native ownership record or intent acts for.
    ClientScope,
    |v| validate_trimmed("client scope", v)
);

validated_string!(
    /// Label of one step within a grouped native publish.
    GroupSubstep,
    |v| validate_trimmed("group substep", v)
);

// ── scoped paths ─────────────────────────────────────────────────────────────

/// Root that a [`ScopedPath`] is resolved against.
///
/// Recording the root rather than an absolute path keeps recovery correct
/// when home or config overrides differ between runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathRoot {
    /// The user home directory.
    UserHome,
    /// The ccync home directory (`~/.ccync`).
    CcyncHome,
    /// The platform config directory.
    ConfigDir,
    /// Portable package and selection state (`~/.agents/ccync`).
    Portable,
    /// Private machine configuration root.
    MachineConfig,
    /// Durable private operational state root.
    MachineState,
    /// Disposable cache root.
    MachineCache,
    /// Stable legacy lifecycle-lock rendezvous (`~/.ccync/build`).
    LegacyBuild,
    /// Stable legacy CCYNC home retained for migration and compatibility files.
    LegacyCcyncHome,
}

impl PathRoot {
    /// Resolve the root through the shared path helpers.
    pub fn resolve(self) -> Option<PathBuf> {
        match self {
            PathRoot::UserHome => crate::paths::user_home(),
            PathRoot::CcyncHome => crate::paths::ccync_home(),
            PathRoot::ConfigDir => crate::paths::config_dir(),
            PathRoot::Portable => crate::paths::portable_root(),
            PathRoot::MachineConfig => crate::paths::machine_config_root(),
            PathRoot::MachineState => crate::paths::machine_state_root(),
            PathRoot::MachineCache => crate::paths::machine_cache_root(),
            PathRoot::LegacyBuild => crate::paths::ccync_build_root(),
            PathRoot::LegacyCcyncHome => crate::paths::ccync_home(),
        }
    }

    #[cfg(test)]
    pub(crate) fn resolve_for(
        self,
        platform: crate::paths::RootPlatform,
        env: &std::collections::HashMap<String, PathBuf>,
    ) -> Result<PathBuf, crate::paths::RootValidationError> {
        let roots = crate::paths::StorageRoots::resolve_for(platform, env)?;
        Ok(roots.scoped_root(self))
    }
}

/// Relative path that is guaranteed to stay inside the root it is joined to.
///
/// Stored with `/` separators. Rejects empty paths, absolute paths, drive
/// prefixes, `.`/`..` components, empty components, components ending in a
/// dot or space, and Windows device-name components.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelativePath(String);

impl RelativePath {
    /// Validate and normalize a relative path (`\` is accepted as separator).
    pub fn new(value: impl AsRef<str>) -> Result<Self, ProviderContractError> {
        let raw = value.as_ref();
        let fail = |reason: &'static str| ProviderContractError::PathOutsideRoot {
            path: raw.to_string(),
            reason,
        };
        if raw.is_empty() {
            return Err(fail("path must not be empty"));
        }
        if raw.chars().any(char::is_control) {
            return Err(fail("path must not contain control characters"));
        }
        let normalized = raw.replace('\\', "/");
        if normalized.starts_with('/') {
            return Err(fail("path must be relative"));
        }
        let mut parts = Vec::new();
        for part in normalized.split('/') {
            match part {
                "" => return Err(fail("path must not contain empty components")),
                "." | ".." => return Err(fail("path must not traverse directories")),
                _ if part.contains(':') => {
                    return Err(fail("path must not contain drive or stream prefixes"));
                }
                _ if part.ends_with('.') || part.ends_with(' ') => {
                    return Err(fail("path components must not end with a dot or space"));
                }
                _ if is_reserved_device_name(part) => {
                    return Err(fail("path components must not be reserved device names"));
                }
                _ => parts.push(part),
            }
        }
        Ok(Self(parts.join("/")))
    }

    /// The normalized `/`-separated value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Join onto `root`; the result always stays under `root`.
    pub fn join_to(&self, root: &Path) -> PathBuf {
        self.0
            .split('/')
            .fold(root.to_path_buf(), |path, part| path.join(part))
    }

    /// Express `candidate` relative to `root`, rejecting any path that is not
    /// lexically inside `root` or that traverses with `..`.
    pub fn from_within(root: &Path, candidate: &Path) -> Result<Self, ProviderContractError> {
        let display = candidate.to_string_lossy().into_owned();
        if candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(ProviderContractError::PathOutsideRoot {
                path: display,
                reason: "path must not traverse directories",
            });
        }
        let relative =
            candidate
                .strip_prefix(root)
                .map_err(|_| ProviderContractError::PathOutsideRoot {
                    path: display.clone(),
                    reason: "path is not under the root",
                })?;
        let parts: Vec<String> = relative
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        Self::new(parts.join("/"))
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for RelativePath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RelativePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// A live path recorded as a root plus a contained relative path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedPath {
    /// Root the path is resolved against.
    pub root: PathRoot,
    /// Contained path under the root.
    pub path: RelativePath,
}

impl ScopedPath {
    /// Resolve to an absolute path; `None` when the root is unavailable.
    pub fn resolve(&self) -> Option<PathBuf> {
        self.root.resolve().map(|root| self.path.join_to(&root))
    }

    /// Resolve after validating that the configured storage roots are safe.
    pub fn resolve_validated(&self) -> Result<Option<PathBuf>, crate::paths::RootValidationError> {
        let roots = crate::paths::StorageRoots::resolve()?;
        let root = roots.scoped_root(self.root);
        let child = self.path.join_to(&root);
        let physical_child = crate::paths::contained_physically(&root, &child)?;
        debug_assert!(physical_child.is_absolute());
        Ok(Some(physical_child))
    }
}

// ── provider and component identity ──────────────────────────────────────────

/// Logical component type exported by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentKind {
    Skill,
    Command,
    Agent,
    Hook,
    Resource,
}

/// Original provenance of one generated component.
///
/// `plugin_name` and `component_id` are the upstream names; they are never
/// replaced by the ccync management name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentIdentity {
    /// Provider source key.
    pub source: ProviderSourceKey,
    /// Original plugin name declared by the provider.
    pub plugin_name: String,
    /// Upstream revision (pin) the component was rendered from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_revision: Option<String>,
    /// Logical component type.
    pub kind: ComponentKind,
    /// Upstream logical component id.
    pub component_id: String,
    /// Origin evidence, when the source location is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ComponentOrigin>,
    /// Target-specific native route, when one has been selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<ComponentRoute>,
}

/// Stable evidence for the physical or canonical origin of a component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "ComponentOriginRecord")]
pub struct ComponentOrigin {
    /// Origin evidence format. Only version 1 is currently understood.
    pub structure_version: u32,
    /// Canonical package or repository reference, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_reference: Option<String>,
    /// Absolute physical source path, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<PathBuf>,
    /// Source revision independent of display name and component identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Content digest independent of display name and component identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<PackageDigest>,
    /// Whether the upstream component is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComponentOriginRecord {
    structure_version: u32,
    #[serde(default)]
    canonical_reference: Option<String>,
    #[serde(default)]
    resolved_path: Option<PathBuf>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    digest: Option<PackageDigest>,
    #[serde(default)]
    enabled: Option<bool>,
}

impl TryFrom<ComponentOriginRecord> for ComponentOrigin {
    type Error = ProviderContractError;
    fn try_from(record: ComponentOriginRecord) -> Result<Self, Self::Error> {
        if record.structure_version != 1 {
            return Err(ProviderContractError::UnsupportedSchemaVersion {
                document: "component origin",
                found: record.structure_version.to_string(),
                supported: 1,
            });
        }
        if let Some(reference) = &record.canonical_reference {
            validate_text("canonical origin reference", reference)?;
        }
        if let Some(path) = &record.resolved_path {
            let text = path.to_string_lossy();
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            {
                return Err(ProviderContractError::PathOutsideRoot {
                    path: text.into_owned(),
                    reason: "origin path must be absolute and normalized",
                });
            }
        }
        if let Some(revision) = &record.revision {
            validate_text("origin revision", revision)?;
        }
        if record.canonical_reference.is_none() && record.resolved_path.is_none() {
            return Err(ProviderContractError::InvalidField {
                field: "component origin",
                value: String::new(),
                reason: "requires a canonical reference or resolved path",
            });
        }
        Ok(Self {
            structure_version: record.structure_version,
            canonical_reference: record.canonical_reference,
            resolved_path: record.resolved_path,
            revision: record.revision,
            digest: record.digest,
            enabled: record.enabled,
        })
    }
}

/// Native route selected for a component on one projection target.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentRoute {
    /// Projection target using this route.
    #[serde(with = "target_id_serde")]
    pub target: ProjectionTargetId,
    /// Native surface or route identifier.
    pub route: String,
}

/// Whether the current build can deliver a component to a client face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientFaceSupportDisposition {
    Supported,
    Unsupported,
    Blocked,
}

/// Evidence that a client loader actually loaded the published component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoaderEvidenceDisposition {
    /// No loader check was performed.
    NotRun,
    /// A loader check confirmed the component loaded.
    Loaded,
    /// A loader check confirmed the component did not load.
    Failed,
}

/// Loader evidence associated with delivery to one internal face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientFaceEvidence {
    pub face: ClientFace,
    pub support: ClientFaceSupportDisposition,
    pub loader: LoaderEvidenceDisposition,
}

impl ClientFaceEvidence {
    /// Record publication without claiming that the client loader ran.
    pub fn published(face: ClientFace, support: ClientFaceSupportDisposition) -> Self {
        Self {
            face,
            support,
            loader: LoaderEvidenceDisposition::NotRun,
        }
    }
}

impl ComponentIdentity {
    /// Build a validated identity.
    pub fn new(
        source: ProviderSourceKey,
        plugin_name: impl Into<String>,
        upstream_revision: Option<String>,
        kind: ComponentKind,
        component_id: impl Into<String>,
    ) -> Result<Self, ProviderContractError> {
        let identity = Self {
            source,
            plugin_name: plugin_name.into(),
            upstream_revision,
            kind,
            component_id: component_id.into(),
            origin: None,
            route: None,
        };
        identity.validate()?;
        Ok(identity)
    }

    /// Re-check field invariants (used after deserialization).
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        validate_text("plugin name", &self.plugin_name)?;
        validate_text("component id", &self.component_id)?;
        if let Some(revision) = &self.upstream_revision {
            validate_text("upstream revision", revision)?;
        }
        if let Some(origin) = &self.origin {
            if origin.structure_version != 1 {
                return Err(ProviderContractError::UnsupportedSchemaVersion {
                    document: "component origin",
                    found: origin.structure_version.to_string(),
                    supported: 1,
                });
            }
            if let Some(reference) = &origin.canonical_reference {
                validate_text("canonical origin reference", reference)?;
            }
            if let Some(path) = &origin.resolved_path {
                if !path.is_absolute()
                    || path
                        .components()
                        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
                {
                    return Err(ProviderContractError::PathOutsideRoot {
                        path: path.to_string_lossy().into_owned(),
                        reason: "origin path must be absolute and normalized",
                    });
                }
            }
            if origin.canonical_reference.is_none() && origin.resolved_path.is_none() {
                return Err(ProviderContractError::InvalidField {
                    field: "component origin",
                    value: String::new(),
                    reason: "requires a canonical reference or resolved path",
                });
            }
        }
        if let Some(route) = &self.route {
            validate_text("component route", &route.route)?;
        }
        Ok(())
    }

    /// Whether two records identify the same upstream component, ignoring
    /// mutable revision, digest, enabled state, and route metadata.
    pub fn same_component(&self, other: &Self) -> bool {
        self.source == other.source
            && self.plugin_name == other.plugin_name
            && self.kind == other.kind
            && self.component_id == other.component_id
            && self
                .origin
                .as_ref()
                .map(|origin| (&origin.canonical_reference, &origin.resolved_path))
                == other
                    .origin
                    .as_ref()
                    .map(|origin| (&origin.canonical_reference, &origin.resolved_path))
    }
}

/// Read the upstream revision for a provider from its lockfile entry, using
/// the shared pin precedence.
pub fn upstream_revision_from_lock_entry(entry: &Value) -> Option<String> {
    entry_pin(entry)
}

/// Qualified per-target alias for a component exposed through a flat adapter.
///
/// Deserialization runs the same validation as [`TargetAlias::new`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "TargetAliasRecord")]
pub struct TargetAlias {
    /// Projection target the alias is published to.
    #[serde(with = "target_id_serde")]
    pub target: ProjectionTargetId,
    /// Component the alias points at (keeps the original logical id).
    pub identity: ComponentIdentity,
    /// Published invocation name on that target.
    pub alias: String,
}

/// Unvalidated wire form of [`TargetAlias`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetAliasRecord {
    #[serde(with = "target_id_serde")]
    target: ProjectionTargetId,
    identity: ComponentIdentity,
    alias: String,
}

impl TryFrom<TargetAliasRecord> for TargetAlias {
    type Error = ProviderContractError;

    fn try_from(record: TargetAliasRecord) -> Result<Self, Self::Error> {
        Self::new(record.target, record.identity, record.alias)
    }
}

impl TargetAlias {
    /// Build a validated alias. The alias must be a safe single path segment
    /// because flat adapters publish it as a directory or file stem.
    pub fn new(
        target: ProjectionTargetId,
        identity: ComponentIdentity,
        alias: impl Into<String>,
    ) -> Result<Self, ProviderContractError> {
        let alias = alias.into();
        validate_segment("target alias", &alias)?;
        identity.validate()?;
        Ok(Self {
            target,
            identity,
            alias,
        })
    }
}

/// One immutable rendered provider package.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderGeneration {
    /// Provider source key.
    pub source: ProviderSourceKey,
    /// Generation identifier (directory name under the provider root).
    pub generation: GenerationId,
    /// Digest of the generated package content.
    pub package_digest: PackageDigest,
}

/// Portable, credential-free provider source intent.
///
/// This allowlist deliberately omits origins, native selectors, local paths,
/// installation identity, and ownership evidence. Local sources carry a
/// binding key which is resolved only by private machine configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableProviderSource {
    pub source: ProviderSourceKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_key: Option<SourceBindingKey>,
    #[serde(default)]
    pub selection: Vec<PortableComponentSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<PackageDigest>,
}

impl PortableProviderSource {
    /// Allow only credential-free HTTPS references with a restricted host,
    /// optional numeric port, and path. Local sources use `bindingKey`.
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if let Some(revision) = &self.revision {
            validate_trimmed("source revision", revision)?;
        }
        if let Some(reference) = &self.reference {
            let malformed = || ProviderContractError::Malformed {
                document: "portable source",
                message: "source reference must be an allowlisted HTTPS URL".to_owned(),
            };
            let (scheme, rest) = reference.split_once("://").ok_or_else(malformed)?;
            if !scheme.eq_ignore_ascii_case("https") {
                return Err(malformed());
            }
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let (host, port) = authority
                .split_once(':')
                .map_or((authority, None), |(host, port)| (host, Some(port)));
            let allowed_host_char =
                |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-');
            let allowed_path_char =
                |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '~' | '/');
            if host.is_empty()
                || !host.chars().all(allowed_host_char)
                || port.is_some_and(|port| {
                    port.is_empty() || !port.chars().all(|ch| ch.is_ascii_digit())
                })
                || !path.chars().all(allowed_path_char)
                || path.split('/').any(|segment| matches!(segment, "." | ".."))
            {
                return Err(ProviderContractError::Malformed {
                    document: "portable source",
                    message: "source reference must be an allowlisted HTTPS URL".to_owned(),
                });
            }
        }
        if self.reference.is_some() == self.binding_key.is_some() {
            return Err(ProviderContractError::Malformed {
                document: "portable source",
                message: "exactly one of reference or bindingKey is required".to_owned(),
            });
        }
        for selection in &self.selection {
            validate_trimmed("selection plugin name", &selection.plugin_name)?;
            validate_trimmed("selection component id", &selection.component_id)?;
        }
        Ok(())
    }
}

/// Portable component selection using the stable fields from
/// [`ComponentIdentity`] while excluding origin and native route details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableComponentSelection {
    pub plugin_name: String,
    pub kind: ComponentKind,
    pub component_id: String,
}

/// Versioned allowlist document for portable provider intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableSourcesDocument {
    pub schema_version: u32,
    pub sources: Vec<PortableProviderSource>,
}

impl PortableSourcesDocument {
    pub fn new(sources: Vec<PortableProviderSource>) -> Result<Self, ProviderContractError> {
        let document = Self {
            schema_version: PORTABLE_SOURCE_SCHEMA_VERSION,
            sources,
        };
        document.validate()?;
        Ok(document)
    }

    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.schema_version != PORTABLE_SOURCE_SCHEMA_VERSION {
            return Err(ProviderContractError::UnsupportedSchemaVersion {
                document: "portable sources",
                found: self.schema_version.to_string(),
                supported: PORTABLE_SOURCE_SCHEMA_VERSION,
            });
        }
        let mut keys = BTreeSet::new();
        for source in &self.sources {
            source.validate()?;
            if !keys.insert(&source.source) {
                return Err(ProviderContractError::Malformed {
                    document: "portable sources",
                    message: format!("duplicate source key {}", source.source),
                });
            }
        }
        Ok(())
    }

    /// Parse strictly. Unknown fields and unsupported versions are errors.
    pub fn parse(input: &str) -> Result<Self, ProviderContractError> {
        let document: Self =
            serde_json::from_str(input).map_err(|error| ProviderContractError::Malformed {
                document: "portable sources",
                message: error.to_string(),
            })?;
        document.validate()?;
        Ok(document)
    }
}

impl<'de> Deserialize<'de> for PortableSourcesDocument {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Record {
            schema_version: u32,
            sources: Vec<PortableProviderSource>,
        }
        let record = Record::deserialize(deserializer)?;
        let document = Self {
            schema_version: record.schema_version,
            sources: record.sources,
        };
        document.validate().map_err(serde::de::Error::custom)?;
        Ok(document)
    }
}

impl ProviderGeneration {
    /// Absolute generation directory, see
    /// [`crate::paths::provider_generation_path`].
    pub fn path(&self) -> Option<PathBuf> {
        crate::paths::provider_generation_path(&self.source, &self.generation)
    }
}

// ── native observation and ownership ─────────────────────────────────────────

/// What was observed for one provider on one native target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeObservation {
    /// The native mechanism could not be queried (missing CLI or support).
    Unavailable,
    /// No registration or path occupies the selector.
    Absent,
    /// A registration or path exists that ccync does not own.
    External,
    /// A registration exists but is disabled.
    Disabled,
    /// A registration exists but cannot load.
    Broken,
    /// The native state could not be read.
    Unreadable,
    /// The owned registration differs from the committed digest.
    Drifted,
    /// The owned registration matches an older committed generation.
    OwnedOld,
    /// The owned registration matches the desired generation.
    Desired,
}

impl NativeObservation {
    /// Whether this state blocks provider changes on the target.
    ///
    /// Only an absent selector or a verified owned registration (old or
    /// desired generation) allows ccync to proceed.
    pub fn blocks_provider_change(self) -> bool {
        !matches!(
            self,
            NativeObservation::Absent | NativeObservation::OwnedOld | NativeObservation::Desired
        )
    }
}

/// Ownership class of a native registration or projected artifact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnershipState {
    /// No ownership evidence; the default for legacy records.
    #[default]
    Unowned,
    /// Recorded intent exists but ownership is not yet committed.
    Pending,
    /// Ownership committed after verification.
    Committed,
    /// Legacy proof retained for drift detection, never for write or prune.
    Historical,
}

/// Attribution attached to an artifact record.
///
/// Every field defaults so legacy records without attribution deserialize;
/// such records are unowned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactAttribution {
    /// Declared ownership state.
    #[serde(default)]
    pub ownership: OwnershipState,
    /// Layout schema that authorized this ownership, when current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_version: Option<u32>,
    /// Root identity selected by the committed layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_identity: Option<String>,
    /// Provenance of the artifact, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ComponentIdentity>,
}

impl ArtifactAttribution {
    /// Ownership that callers may act on. A declared state without recorded
    /// provenance is treated as unowned.
    pub fn effective_ownership(&self) -> OwnershipState {
        match self.identity.as_ref() {
            Some(identity)
                if identity.origin.is_some()
                    && identity.validate().is_ok()
                    && self.ownership == OwnershipState::Committed
                    && self.layout_version == Some(1)
                    && self.root_identity.as_ref().is_some_and(|root| {
                        !root.trim().is_empty() && Path::new(root).is_absolute()
                    }) =>
            {
                self.ownership
            }
            None => OwnershipState::Unowned,
            Some(_) => OwnershipState::Unowned,
        }
    }

    /// Read attribution from an arbitrary record. Records without
    /// attribution, or with malformed attribution, yield the unowned default.
    pub fn from_record(record: &Value) -> Self {
        serde_json::from_value::<Self>(record.clone())
            .ok()
            .filter(|attribution| {
                attribution
                    .identity
                    .as_ref()
                    .is_none_or(|identity| identity.validate().is_ok())
            })
            .unwrap_or_default()
    }
}

/// Identity of one root in a committed storage layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutRootIdentity {
    /// Stable root role, such as `portable` or `machine-state`.
    pub name: String,
    /// Absolute root path captured by the commit.
    pub path: PathBuf,
}

/// A committed storage layout. Its presence is the authority commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageLayoutCommit {
    /// Layout format version.
    pub layout_version: u32,
    /// Root identities selected by this commit.
    pub roots: Vec<LayoutRootIdentity>,
}

impl StorageLayoutCommit {
    /// Validate versioned root identities against the current resolved roots.
    pub fn matches_roots(&self, roots: &[(String, PathBuf)]) -> bool {
        self.layout_version == 1
            && self.roots.len() == roots.len()
            && self.roots.iter().all(|root| {
                !root.name.is_empty()
                    && root.path.is_absolute()
                    && !root.path.as_os_str().is_empty()
            })
            && self
                .roots
                .iter()
                .map(|root| &root.name)
                .collect::<BTreeSet<_>>()
                .len()
                == self.roots.len()
            && self.roots.iter().all(|committed| {
                roots
                    .iter()
                    .any(|(name, path)| committed.name == *name && committed.path == *path)
            })
    }
}

/// Ownership fingerprint recorded before a native mutation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipFingerprint {
    /// Exact native selector.
    pub selector: NativeSelector,
    /// Package digest expected at that selector.
    pub package_digest: PackageDigest,
}

/// Reference to a saved preimage of a live path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreimageRef {
    /// Live path whose prior content was saved.
    pub path: ScopedPath,
    /// Saved copy, relative to the ccync build root.
    pub saved: RelativePath,
}

/// Phase of a recorded lifecycle intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntentPhase {
    /// Replacements staged; nothing live changed yet.
    Staged,
    /// Native install or live path replacement in progress.
    Publishing,
    /// Re-reading native state and validating capabilities.
    Verifying,
    /// Ownership committed; pruning proven-owned superseded paths.
    Pruning,
    /// Waiting on the final loader reload check.
    Reloading,
}

/// Enablement state that an intent asks the native loader to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnablementIntent {
    /// The registration must load and run.
    Enabled,
    /// The registration stays installed but must not load.
    Disabled,
}

/// Intent recorded before native registration or live path replacement.
///
/// Holds only what recovery needs: fingerprints, scoped paths, preimage
/// references, generations, and phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingIntent {
    /// Target the intent applies to.
    #[serde(with = "target_id_serde")]
    pub target: ProjectionTargetId,
    /// Ownership fingerprints for every selector the intent touches.
    pub ownership_fingerprints: Vec<OwnershipFingerprint>,
    /// Live paths the intent may create or replace.
    pub scoped_paths: Vec<ScopedPath>,
    /// Saved preimages for replaced live paths.
    #[serde(default)]
    pub preimage_refs: Vec<PreimageRef>,
    /// Generation being replaced, when one was committed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_generation: Option<ProviderGeneration>,
    /// Generation being published.
    pub desired_generation: ProviderGeneration,
    /// Current phase.
    pub phase: IntentPhase,
    /// Client scope the intent acts for. Absent on legacy intents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_scope: Option<ClientScope>,
    /// Digest of the provider source the intent publishes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<PackageDigest>,
    /// Digest of the rendered view the intent publishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_digest: Option<PackageDigest>,
    /// Digest of the live content observed before the mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_preimage: Option<PackageDigest>,
    /// Enablement state the intent asks the native loader to reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enablement_intent: Option<EnablementIntent>,
    /// Grouped-publish step the intent is currently in, when grouped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_substep: Option<GroupSubstep>,
}

/// Shape of a standalone destination before publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum StandalonePreimage {
    /// No destination existed.
    Absent,
    /// Destination was a link with its raw target preserved.
    Link { raw_target: PathBuf },
    /// Destination was a directory tree with its content fingerprint.
    Tree { fingerprint: PackageDigest },
}

/// Recovery operation required to restore a standalone link preimage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StandaloneLinkKind {
    Junction,
    DirectorySymlink,
    FileSymlink,
}

/// Typed publication transaction for a standalone plugin tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StandalonePublicationIntent {
    /// Stable identifier for this publication attempt.
    pub operation_id: String,
    /// Proof identifying the actual source and its content.
    pub source: String,
    pub source_fingerprint: PackageDigest,
    /// Readers or targets covered by this operation.
    pub targets: Vec<String>,
    /// Destination controlled by the standalone publisher.
    pub destination: ScopedPath,
    /// Destination shape and content before publication.
    pub preimage: StandalonePreimage,
    /// Kind of link used by the publication, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_kind: Option<StandaloneLinkKind>,
    /// Saved copy and fingerprint of a replaced destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preimage_ref: Option<PreimageRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preimage_fingerprint: Option<PackageDigest>,
    /// Staged new tree and backup paths.
    pub stage_path: RelativePath,
    pub backup_path: RelativePath,
    /// Fingerprint expected after switching the destination.
    pub expected_tree_fingerprint: PackageDigest,
    /// Registry state before and after publication, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_before: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_after: Option<Value>,
    /// Current standalone publication phase.
    pub phase: IntentPhase,
}

impl StandalonePublicationIntent {
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        validate_trimmed("standalone operation id", &self.operation_id)?;
        validate_trimmed("standalone source", &self.source)?;
        if self.targets.is_empty() {
            return Err(ProviderContractError::InvalidField {
                field: "standalone targets",
                value: String::new(),
                reason: "must not be empty",
            });
        }
        for target in &self.targets {
            validate_trimmed("standalone target", target)?;
        }
        if let Some(preimage) = &self.preimage_ref {
            if preimage.path != self.destination {
                return Err(ProviderContractError::Malformed {
                    document: "standalone publication intent",
                    message: "preimage path must match destination".into(),
                });
            }
        }
        match &self.preimage {
            StandalonePreimage::Link { raw_target }
                if raw_target.as_os_str().is_empty() || self.link_kind.is_none() =>
            {
                return Err(ProviderContractError::Malformed {
                    document: "standalone publication intent",
                    message: "link preimage requires a nonempty raw target and recovery kind"
                        .into(),
                });
            }
            StandalonePreimage::Link { .. } => {}
            _ if self.link_kind.is_some() => {
                return Err(ProviderContractError::Malformed {
                    document: "standalone publication intent",
                    message: "non-link preimage cannot have a link recovery kind".into(),
                });
            }
            StandalonePreimage::Absent
                if self.preimage_ref.is_some() || self.preimage_fingerprint.is_some() =>
            {
                return Err(ProviderContractError::Malformed {
                    document: "standalone publication intent",
                    message: "absent preimage cannot have saved content proof".into(),
                });
            }
            StandalonePreimage::Tree { fingerprint }
                if self.preimage_fingerprint.as_ref() != Some(fingerprint) =>
            {
                return Err(ProviderContractError::Malformed {
                    document: "standalone publication intent",
                    message: "tree preimage fingerprint must match its content proof".into(),
                });
            }
            _ => {}
        }
        Ok(())
    }
}

/// Proven reader outcome that authorizes a standalone path to become absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StandaloneWithdrawalReaderIntent {
    /// The reader deliberately no longer loads this source, including removal.
    Disabled,
    /// The reader has a separately verified replacement delivery.
    ReplacementVerified,
}

/// Current intent and verification evidence for one known reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StandaloneWithdrawalReaderProof {
    pub reader: String,
    pub intent: StandaloneWithdrawalReaderIntent,
    pub evidence: String,
}

/// A committed standalone path is moved out of discovery without a new tree.
/// Source evidence comes from its committed row, even after source removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StandaloneWithdrawalIntent {
    pub operation_id: String,
    pub source_id: String,
    pub source_revision: String,
    pub source_fingerprint: PackageDigest,
    /// Complete known reader set, reverified by the caller before recovery.
    pub readers: Vec<StandaloneWithdrawalReaderProof>,
    pub destination: ScopedPath,
    /// Withdrawal requires an existing, exactly proven preimage.
    pub preimage: StandalonePreimage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_kind: Option<StandaloneLinkKind>,
    pub preimage_ref: PreimageRef,
    pub preimage_fingerprint: PackageDigest,
    pub backup_path: RelativePath,
    /// Exact projection row envelopes before and after ownership removal.
    pub registry_before: Value,
    pub registry_after: Value,
    pub phase: IntentPhase,
}

impl StandaloneWithdrawalIntent {
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        let invalid = |message: &str| ProviderContractError::Malformed {
            document: "standalone withdrawal intent",
            message: message.into(),
        };
        validate_segment("standalone withdrawal operation id", &self.operation_id)?;
        validate_trimmed("standalone withdrawal source", &self.source_id)?;
        validate_trimmed("standalone withdrawal revision", &self.source_revision)?;
        if self.readers.is_empty() {
            return Err(invalid(
                "withdrawal requires a nonempty complete reader set",
            ));
        }
        let mut readers = BTreeSet::new();
        for proof in &self.readers {
            validate_trimmed("standalone withdrawal reader", &proof.reader)?;
            validate_trimmed("standalone withdrawal reader evidence", &proof.evidence)?;
            if !readers.insert(proof.reader.as_str()) {
                return Err(invalid("withdrawal readers must be unique"));
            }
        }
        if self.preimage_ref.path != self.destination || self.preimage_ref.saved != self.backup_path
        {
            return Err(invalid(
                "saved preimage must match destination and private backup",
            ));
        }
        match &self.preimage {
            StandalonePreimage::Absent => {
                return Err(invalid("withdrawal requires a non-Absent preimage"));
            }
            StandalonePreimage::Link { raw_target } => {
                if raw_target.as_os_str().is_empty() || self.link_kind.is_none() {
                    return Err(invalid(
                        "link preimage requires a nonempty raw target and recovery kind",
                    ));
                }
            }
            StandalonePreimage::Tree { fingerprint } => {
                if self.link_kind.is_some() || fingerprint != &self.preimage_fingerprint {
                    return Err(invalid(
                        "tree preimage requires matching content proof and no link kind",
                    ));
                }
            }
        }
        let before = self
            .registry_before
            .as_object()
            .ok_or_else(|| invalid("registry before must be an ownership row object"))?;
        let categories = before
            .get("categories")
            .and_then(Value::as_array)
            .filter(|categories| !categories.is_empty())
            .ok_or_else(|| invalid("registry before requires ownership categories"))?;
        let mut category_names = BTreeSet::new();
        for category in categories {
            let category = category
                .as_str()
                .ok_or_else(|| invalid("ownership category must be a string"))?;
            validate_trimmed("standalone withdrawal ownership category", category)?;
            if !category_names.insert(category) {
                return Err(invalid("ownership categories must be unique"));
            }
        }
        if before.get("source").and_then(Value::as_str) != Some(self.source_id.as_str()) {
            return Err(invalid(
                "registry before source does not match withdrawal source",
            ));
        }
        let committed = before
            .get("standalone")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("registry before requires committed standalone proof"))?;
        for (field, expected) in [
            ("sourceId", self.source_id.as_str()),
            ("sourceRevision", self.source_revision.as_str()),
            ("sourceFingerprint", self.source_fingerprint.as_str()),
            ("treeFingerprint", self.preimage_fingerprint.as_str()),
        ] {
            if committed.get(field).and_then(Value::as_str) != Some(expected) {
                return Err(invalid(
                    "committed standalone source, revision or fingerprint differs",
                ));
            }
        }
        let path = committed
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("committed standalone path is required"))?;
        validate_text("committed standalone path", path)?;
        let targets = committed
            .get("targets")
            .and_then(Value::as_array)
            .filter(|targets| !targets.is_empty())
            .ok_or_else(|| invalid("committed standalone targets are required"))?;
        let mut committed_targets = BTreeSet::new();
        for target in targets {
            let target = target
                .as_str()
                .ok_or_else(|| invalid("committed target must be a string"))?;
            validate_trimmed("committed standalone target", target)?;
            if !committed_targets.insert(target) || !readers.contains(target) {
                return Err(invalid("readers must cover every unique committed target"));
            }
        }
        let after = self
            .registry_after
            .as_object()
            .ok_or_else(|| invalid("registry after must be an ownership row object"))?;
        if !after
            .get("categories")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
            || after.get("source") != Some(&Value::Null)
            || after.get("standalone") != Some(&Value::Null)
        {
            return Err(invalid("registry after must prove ownership absence"));
        }
        Ok(())
    }
}

/// Verified capability inventory of an installed provider package.
pub type CapabilityInventory = BTreeSet<ComponentKind>;

/// Committed native ownership for one provider on one target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommittedOwnership {
    /// Target the registration lives on.
    #[serde(with = "target_id_serde")]
    pub target: ProjectionTargetId,
    /// Exact native selector.
    pub selector: NativeSelector,
    /// Original plugin name published under that selector.
    pub plugin_name: String,
    /// Upstream revision the generation was rendered from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_revision: Option<String>,
    /// Installed generation (source key and generated digest).
    pub generation: ProviderGeneration,
    /// Capabilities verified after installation.
    #[serde(default)]
    pub capabilities: CapabilityInventory,
    /// Client scope the ownership was committed for. Part of the ownership
    /// proof together with `source_digest` and `view_digest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_scope: Option<ClientScope>,
    /// Digest of the provider source the ownership was committed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<PackageDigest>,
    /// Digest of the rendered view the ownership was committed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_digest: Option<PackageDigest>,
    /// Digest of the live content observed before the commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_preimage: Option<PackageDigest>,
    /// Enablement state the committed registration was verified to reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enablement_intent: Option<EnablementIntent>,
    /// Grouped-publish step the commit belongs to, when grouped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_substep: Option<GroupSubstep>,
}

impl CommittedOwnership {
    /// Re-check text invariants (used after deserialization). The upstream
    /// revision follows the same rule as [`ComponentIdentity::validate`].
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        validate_text("plugin name", &self.plugin_name)?;
        if let Some(revision) = &self.upstream_revision {
            validate_text("upstream revision", revision)?;
        }
        Ok(())
    }

    /// Whether the record carries the full ownership proof: client scope,
    /// source digest, and view digest. A legacy row without them grants no
    /// ownership.
    fn has_proof(&self) -> bool {
        self.client_scope.is_some() && self.source_digest.is_some() && self.view_digest.is_some()
    }

    /// Whether an observed registration is exactly this committed record.
    ///
    /// Requires the same target, selector, source, upstream revision, and
    /// package digest, and that every required capability was verified.
    pub fn proves(
        &self,
        target: ProjectionTargetId,
        selector: &NativeSelector,
        generation: &ProviderGeneration,
        upstream_revision: Option<&str>,
        required: &CapabilityInventory,
    ) -> bool {
        self.has_proof()
            && self.target == target
            && &self.selector == selector
            && self.generation.source == generation.source
            && self.generation.package_digest == generation.package_digest
            && self.upstream_revision.as_deref() == upstream_revision
            && required.is_subset(&self.capabilities)
    }
}

// ── versioned documents ──────────────────────────────────────────────────────

/// Read and check the `schemaVersion` field of a versioned document.
fn check_schema_version(
    document: &'static str,
    value: &Value,
    supported: u32,
) -> Result<(), ProviderContractError> {
    let object = value
        .as_object()
        .ok_or_else(|| ProviderContractError::Malformed {
            document,
            message: "expected a JSON object".to_string(),
        })?;
    match object.get("schemaVersion") {
        Some(Value::Number(n)) if n.as_u64() == Some(u64::from(supported)) => Ok(()),
        Some(other) => Err(ProviderContractError::UnsupportedSchemaVersion {
            document,
            found: other.to_string(),
            supported,
        }),
        None => Err(ProviderContractError::UnsupportedSchemaVersion {
            document,
            found: "missing".to_string(),
            supported,
        }),
    }
}

fn malformed(document: &'static str, err: serde_json::Error) -> ProviderContractError {
    ProviderContractError::Malformed {
        document,
        message: err.to_string(),
    }
}

/// Content of the `_providerLifecycle` lockfile namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderLifecycleLock {
    /// Always [`PROVIDER_LIFECYCLE_SCHEMA_VERSION`] when written.
    pub schema_version: u32,
    /// Committed native ownership records.
    #[serde(default)]
    pub committed: Vec<CommittedOwnership>,
}

impl Default for ProviderLifecycleLock {
    fn default() -> Self {
        Self {
            schema_version: PROVIDER_LIFECYCLE_SCHEMA_VERSION,
            committed: Vec::new(),
        }
    }
}

impl ProviderLifecycleLock {
    const DOCUMENT: &'static str = "provider lifecycle lock namespace";

    /// Read the namespace from a whole lockfile value. A lockfile without
    /// the namespace yields an empty section (nothing is owned).
    pub fn from_lock(lock: &Value) -> Result<Self, ProviderContractError> {
        let object = lock
            .as_object()
            .ok_or_else(|| ProviderContractError::Malformed {
                document: Self::DOCUMENT,
                message: "lockfile must be a JSON object".to_string(),
            })?;
        match object.get(PROVIDER_LIFECYCLE_NAMESPACE) {
            None => Ok(Self::default()),
            Some(section) => {
                check_schema_version(Self::DOCUMENT, section, PROVIDER_LIFECYCLE_SCHEMA_VERSION)?;
                let parsed: Self = serde_json::from_value(section.clone())
                    .map_err(|e| malformed(Self::DOCUMENT, e))?;
                for record in &parsed.committed {
                    record.validate()?;
                }
                Ok(parsed)
            }
        }
    }

    /// Write the namespace into a whole lockfile value, leaving every other
    /// key untouched.
    pub fn write_to_lock(&self, lock: &mut Value) -> Result<(), ProviderContractError> {
        let object: &mut Map<String, Value> =
            lock.as_object_mut()
                .ok_or_else(|| ProviderContractError::Malformed {
                    document: Self::DOCUMENT,
                    message: "lockfile must be a JSON object".to_string(),
                })?;
        let mut section = self.clone();
        section.schema_version = PROVIDER_LIFECYCLE_SCHEMA_VERSION;
        let value = serde_json::to_value(&section).map_err(|e| malformed(Self::DOCUMENT, e))?;
        object.insert(PROVIDER_LIFECYCLE_NAMESPACE.to_string(), value);
        Ok(())
    }

    /// Committed record for an exact target and selector.
    pub fn find(
        &self,
        target: ProjectionTargetId,
        selector: &NativeSelector,
    ) -> Option<&CommittedOwnership> {
        self.committed
            .iter()
            .find(|record| record.target == target && &record.selector == selector)
    }
}

/// Provider lifecycle intent journal (`~/.ccync/build/projection.txn.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderLifecycleJournal {
    /// Always [`PROVIDER_JOURNAL_SCHEMA_VERSION`] when written.
    pub schema_version: u32,
    /// Pending intents, in recording order.
    #[serde(default)]
    pub intents: Vec<PendingIntent>,
    /// Pending standalone tree publications. Absent in v1 journals.
    #[serde(default)]
    pub standalone_intents: Vec<StandalonePublicationIntent>,
    /// Pending standalone withdrawals. Absent in v1 and v2 journals.
    #[serde(default)]
    pub standalone_withdrawal_intents: Vec<StandaloneWithdrawalIntent>,
    /// Target list consented to for the direct projection cutover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "target_ids_serde")]
    pub cutover_targets: Option<Vec<ProjectionTargetId>>,
    /// Combined entries and link to retire after replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combined_entries_and_link_to_retire: Option<Vec<ScopedPath>>,
    /// Redundant managed copies to retire after replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundant_managed_copy_paths: Option<Vec<ScopedPath>>,
}

impl Default for ProviderLifecycleJournal {
    fn default() -> Self {
        Self {
            schema_version: PROVIDER_JOURNAL_SCHEMA_VERSION,
            intents: Vec::new(),
            standalone_intents: Vec::new(),
            standalone_withdrawal_intents: Vec::new(),
            cutover_targets: None,
            combined_entries_and_link_to_retire: None,
            redundant_managed_copy_paths: None,
        }
    }
}

impl ProviderLifecycleJournal {
    const DOCUMENT: &'static str = "provider lifecycle journal";

    /// Parse a journal, rejecting unsupported schema versions before
    /// interpreting any intent.
    pub fn from_json_str(text: &str) -> Result<Self, ProviderContractError> {
        let value: Value = serde_json::from_str(text).map_err(|e| malformed(Self::DOCUMENT, e))?;
        let version = value
            .get("schemaVersion")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !matches!(version, 1 | 2) {
            check_schema_version(Self::DOCUMENT, &value, PROVIDER_JOURNAL_SCHEMA_VERSION)?;
        }
        let mut journal: Self =
            serde_json::from_value(value).map_err(|e| malformed(Self::DOCUMENT, e))?;
        if version == 1 && !journal.standalone_intents.is_empty() {
            return Err(ProviderContractError::UnsupportedSchemaVersion {
                document: Self::DOCUMENT,
                found: "1 with standalone publication intents".to_string(),
                supported: PROVIDER_JOURNAL_SCHEMA_VERSION,
            });
        }
        if version < 3 && !journal.standalone_withdrawal_intents.is_empty() {
            return Err(ProviderContractError::UnsupportedSchemaVersion {
                document: Self::DOCUMENT,
                found: format!("{version} with standalone withdrawal intents"),
                supported: PROVIDER_JOURNAL_SCHEMA_VERSION,
            });
        }
        journal.validate_standalone_operations()?;
        journal.schema_version = PROVIDER_JOURNAL_SCHEMA_VERSION;
        Ok(journal)
    }

    /// Serialize the journal with the current schema version.
    pub fn to_json_string(&self) -> Result<String, ProviderContractError> {
        let mut journal = self.clone();
        journal.schema_version = PROVIDER_JOURNAL_SCHEMA_VERSION;
        journal.validate_standalone_operations()?;
        serde_json::to_string_pretty(&journal).map_err(|e| malformed(Self::DOCUMENT, e))
    }

    fn validate_standalone_operations(&self) -> Result<(), ProviderContractError> {
        for intent in &self.standalone_intents {
            intent.validate()?;
        }
        for intent in &self.standalone_withdrawal_intents {
            intent.validate()?;
        }
        Ok(())
    }
}

// ── serde helpers ────────────────────────────────────────────────────────────

/// Serialize [`ProjectionTargetId`] through its canonical string form.
mod target_id_serde {
    use super::ProjectionTargetId;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        target: &ProjectionTargetId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(target.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ProjectionTargetId, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

mod target_ids_serde {
    use super::ProjectionTargetId;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(
        value: &Option<Vec<ProjectionTargetId>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .as_ref()
            .map(|targets| {
                targets
                    .iter()
                    .map(|target| target.as_str())
                    .collect::<Vec<_>>()
            })
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<ProjectionTargetId>>, D::Error> {
        Option::<Vec<String>>::deserialize(deserializer)?
            .map(|targets| {
                targets
                    .into_iter()
                    .map(|target| target.parse().map_err(serde::de::Error::custom))
                    .collect()
            })
            .transpose()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_faces_publication_keeps_loader_evidence_not_run() {
        let evidence = ClientFaceEvidence::published(
            ClientFace::CopilotCli,
            ClientFaceSupportDisposition::Supported,
        );
        assert_eq!(evidence.loader, LoaderEvidenceDisposition::NotRun);
        let encoded = serde_json::to_value(evidence).expect("serialize face evidence");
        assert_eq!(encoded["support"], json!("supported"));
        assert_eq!(encoded["loader"], json!("not-run"));
        println!(
            "E1 client_faces observation: publication=recorded loader_evidence=not-run support=supported"
        );
    }

    #[test]
    fn standalone_publication_journal_roundtrips_v1_and_writes_v3() {
        let legacy = ProviderLifecycleJournal::from_json_str(r#"{"schemaVersion":1,"intents":[]}"#)
            .expect("v1 journal");
        assert!(legacy.standalone_intents.is_empty());
        let legacy_with_empty_collection = ProviderLifecycleJournal::from_json_str(
            r#"{"schemaVersion":1,"intents":[],"standaloneIntents":[]}"#,
        )
        .expect("v1 journal with empty standalone collection");
        assert!(legacy_with_empty_collection.standalone_intents.is_empty());
        let encoded = legacy.to_json_string().expect("write v3");
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["schemaVersion"], json!(3));
        assert!(matches!(
            check_schema_version("old reader", &value, 1),
            Err(ProviderContractError::UnsupportedSchemaVersion { .. })
        ));
        let intent = StandalonePublicationIntent {
            operation_id: "op-1".into(),
            source: "plugin:actual-source".into(),
            source_fingerprint: PackageDigest::new("sha256:source").unwrap(),
            targets: vec!["claude".into()],
            destination: ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(".claude/skills/example").unwrap(),
            },
            preimage: StandalonePreimage::Link {
                raw_target: PathBuf::from("../source/example"),
            },
            link_kind: Some(StandaloneLinkKind::DirectorySymlink),
            preimage_ref: None,
            preimage_fingerprint: None,
            stage_path: RelativePath::new("stage/op-1").unwrap(),
            backup_path: RelativePath::new("backup/op-1").unwrap(),
            expected_tree_fingerprint: PackageDigest::new("sha256:new-tree").unwrap(),
            registry_before: Some(json!({"key":"before"})),
            registry_after: Some(json!({"key":"after"})),
            phase: IntentPhase::Staged,
        };
        intent.validate().unwrap();
        let mut journal = ProviderLifecycleJournal::default();
        journal.standalone_intents.push(intent);
        journal.cutover_targets = Some(vec![ProjectionTargetId::Claude]);
        let mut mislabeled_v1: Value =
            serde_json::from_str(&journal.to_json_string().unwrap()).unwrap();
        mislabeled_v1["schemaVersion"] = json!(1);
        assert!(matches!(
            ProviderLifecycleJournal::from_json_str(&mislabeled_v1.to_string()),
            Err(ProviderContractError::UnsupportedSchemaVersion { .. })
        ));
        let text = journal.to_json_string().unwrap();
        assert_eq!(
            ProviderLifecycleJournal::from_json_str(&text).unwrap(),
            journal
        );
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["cutoverTargets"], json!(["claude"]));
    }

    #[test]
    fn standalone_publication_rejects_invalid_paths_and_preimage_proof() {
        assert!(RelativePath::new("../../escape").is_err());
        let mut intent = StandalonePublicationIntent {
            operation_id: "op-1".into(),
            source: "actual-source".into(),
            source_fingerprint: PackageDigest::new("sha256:source").unwrap(),
            targets: vec!["claude".into()],
            destination: ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(".claude/skills/example").unwrap(),
            },
            preimage: StandalonePreimage::Absent,
            link_kind: None,
            preimage_ref: None,
            preimage_fingerprint: None,
            stage_path: RelativePath::new("stage/op-1").unwrap(),
            backup_path: RelativePath::new("backup/op-1").unwrap(),
            expected_tree_fingerprint: PackageDigest::new("sha256:new-tree").unwrap(),
            registry_before: None,
            registry_after: None,
            phase: IntentPhase::Staged,
        };
        intent.preimage_ref = Some(PreimageRef {
            path: ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new("elsewhere").unwrap(),
            },
            saved: RelativePath::new("preimages/op-1").unwrap(),
        });
        assert!(intent.validate().is_err());
        intent.preimage_ref = None;
        intent.targets.clear();
        assert!(intent.validate().is_err());
        intent.targets.push("claude".into());
        intent.preimage = StandalonePreimage::Tree {
            fingerprint: PackageDigest::new("sha256:tree").unwrap(),
        };
        assert!(intent.validate().is_err());
        intent.preimage_fingerprint = Some(PackageDigest::new("sha256:tree").unwrap());
        intent.link_kind = Some(StandaloneLinkKind::Junction);
        assert!(intent.validate().is_err());
        intent.link_kind = None;
        intent.preimage = StandalonePreimage::Link {
            raw_target: PathBuf::new(),
        };
        assert!(intent.validate().is_err());
        intent.preimage = StandalonePreimage::Link {
            raw_target: PathBuf::from("../source/example"),
        };
        intent.preimage_fingerprint = None;
        assert!(intent.validate().is_err());
        let mut journal = ProviderLifecycleJournal::default();
        journal.standalone_intents.push(intent);
        assert!(journal.to_json_string().is_err());
    }
    fn standalone_withdrawal_fixture() -> StandaloneWithdrawalIntent {
        let destination = ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".agents/skills/example").unwrap(),
        };
        let backup_path = RelativePath::new("standalone-withdrawals/withdraw-1/backup").unwrap();
        let source_fingerprint =
            PackageDigest::new("standalone-v1:fnv1a64:0123456789abcdef").unwrap();
        let preimage_fingerprint =
            PackageDigest::new("standalone-v1:fnv1a64:fedcba9876543210").unwrap();
        StandaloneWithdrawalIntent {
            operation_id: "withdraw-1".into(),
            source_id: "actual-provider".into(),
            source_revision: "commit:abc123".into(),
            source_fingerprint: source_fingerprint.clone(),
            readers: vec![
                StandaloneWithdrawalReaderProof {
                    reader: "claude".into(),
                    intent: StandaloneWithdrawalReaderIntent::Disabled,
                    evidence: "fixture: current source removal intent".into(),
                },
                StandaloneWithdrawalReaderProof {
                    reader: "codex".into(),
                    intent: StandaloneWithdrawalReaderIntent::ReplacementVerified,
                    evidence: "fixture: verified replacement generation".into(),
                },
            ],
            preimage: StandalonePreimage::Tree {
                fingerprint: preimage_fingerprint.clone(),
            },
            link_kind: None,
            preimage_ref: PreimageRef {
                path: destination.clone(),
                saved: backup_path.clone(),
            },
            destination,
            preimage_fingerprint: preimage_fingerprint.clone(),
            backup_path,
            registry_before: json!({
                "categories": ["directSkillProjectionPaths"],
                "source": "actual-provider",
                "standalone": {
                    "path": "/fixture/home/.agents/skills/example",
                    "sourceId": "actual-provider",
                    "sourceRevision": "commit:abc123",
                    "sourceFingerprint": source_fingerprint,
                    "treeFingerprint": preimage_fingerprint,
                    "targets": ["claude", "codex"]
                }
            }),
            registry_after: json!({"categories": [], "source": null, "standalone": null}),
            phase: IntentPhase::Staged,
        }
    }

    fn standalone_publication_fixture() -> StandalonePublicationIntent {
        StandalonePublicationIntent {
            operation_id: "publish-1".into(),
            source: "fixture: distinct publication source".into(),
            source_fingerprint: PackageDigest::new("sha256:publication-source").unwrap(),
            targets: vec!["copilot".into()],
            destination: ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(".copilot/skills/other").unwrap(),
            },
            preimage: StandalonePreimage::Absent,
            link_kind: None,
            preimage_ref: None,
            preimage_fingerprint: None,
            stage_path: RelativePath::new("standalone-publications/publish-1/stage").unwrap(),
            backup_path: RelativePath::new("standalone-publications/publish-1/backup").unwrap(),
            expected_tree_fingerprint: PackageDigest::new("sha256:published-tree").unwrap(),
            registry_before: None,
            registry_after: None,
            phase: IntentPhase::Publishing,
        }
    }

    #[test]
    fn standalone_withdrawal_v3_roundtrip_preserves_native_and_publication_data() {
        let mut journal = ProviderLifecycleJournal::default();
        journal.intents.push(PendingIntent {
            target: ProjectionTargetId::Codex,
            ownership_fingerprints: vec![],
            scoped_paths: vec![],
            preimage_refs: vec![],
            previous_generation: Some(generation("sha256:old-native")),
            desired_generation: generation("sha256:new-native"),
            phase: IntentPhase::Publishing,
            client_scope: Some(ClientScope::new("fixture-client").unwrap()),
            source_digest: Some(PackageDigest::new("sha256:native-source").unwrap()),
            view_digest: None,
            observed_preimage: None,
            enablement_intent: Some(EnablementIntent::Disabled),
            group_substep: None,
        });
        journal
            .standalone_intents
            .push(standalone_publication_fixture());
        journal
            .standalone_withdrawal_intents
            .push(standalone_withdrawal_fixture());
        journal.cutover_targets = Some(vec![ProjectionTargetId::Claude]);
        journal.combined_entries_and_link_to_retire = Some(vec![ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".claude/plugins/combined").unwrap(),
        }]);
        let encoded = journal.to_json_string().unwrap();
        assert_eq!(
            ProviderLifecycleJournal::from_json_str(&encoded).unwrap(),
            journal
        );
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["schemaVersion"], json!(3));
        let withdrawal = &value["standaloneWithdrawalIntents"][0];
        assert_eq!(withdrawal["readers"][0]["intent"], json!("disabled"));
        assert_eq!(
            withdrawal["readers"][1]["intent"],
            json!("replacement-verified")
        );
        for absent in [
            "sourcePath",
            "stagePath",
            "expectedTreeFingerprint",
            "selector",
        ] {
            assert!(withdrawal.get(absent).is_none(), "{absent}");
        }
    }

    #[test]
    fn standalone_withdrawal_reads_v1_v2_without_file_migration() {
        let temp = tempfile::TempDir::new().unwrap();
        for version in [1, 2] {
            let mut value = json!({"schemaVersion": version, "intents": []});
            if version == 2 {
                value["standaloneIntents"] = json!([standalone_publication_fixture()]);
            }
            let text = serde_json::to_string_pretty(&value).unwrap();
            let path = temp.path().join(format!("journal-v{version}.json"));
            std::fs::write(&path, &text).unwrap();
            let decoded =
                ProviderLifecycleJournal::from_json_str(&std::fs::read_to_string(&path).unwrap())
                    .unwrap();
            assert!(decoded.standalone_withdrawal_intents.is_empty());
            assert_eq!(decoded.standalone_intents.len(), usize::from(version == 2));
            let updated: Value = serde_json::from_str(&decoded.to_json_string().unwrap()).unwrap();
            assert_eq!(updated["schemaVersion"], json!(3));
            assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
        }
    }

    #[test]
    fn standalone_withdrawal_rejects_low_version_new_collections_and_invalid_publication() {
        let mut journal = ProviderLifecycleJournal::default();
        journal
            .standalone_withdrawal_intents
            .push(standalone_withdrawal_fixture());
        for version in [1, 2] {
            let mut value: Value =
                serde_json::from_str(&journal.to_json_string().unwrap()).unwrap();
            value["schemaVersion"] = json!(version);
            assert!(matches!(
                ProviderLifecycleJournal::from_json_str(&value.to_string()),
                Err(ProviderContractError::UnsupportedSchemaVersion { .. })
            ));
        }
        journal.standalone_withdrawal_intents.clear();
        journal
            .standalone_intents
            .push(standalone_publication_fixture());
        let mut value: Value = serde_json::from_str(&journal.to_json_string().unwrap()).unwrap();
        value["schemaVersion"] = json!(1);
        assert!(matches!(
            ProviderLifecycleJournal::from_json_str(&value.to_string()),
            Err(ProviderContractError::UnsupportedSchemaVersion { .. })
        ));
        value["schemaVersion"] = json!(2);
        value["standaloneIntents"][0]["targets"] = json!([]);
        assert!(ProviderLifecycleJournal::from_json_str(&value.to_string()).is_err());
    }

    #[test]
    fn standalone_withdrawal_requires_unique_complete_readers_and_evidence() {
        let valid = standalone_withdrawal_fixture();
        let mut empty_name = valid.readers.clone();
        empty_name[0].reader.clear();
        let mut empty_evidence = valid.readers.clone();
        empty_evidence[0].evidence.clear();
        let mut missing_reader = valid.readers.clone();
        missing_reader.pop();
        let duplicate = vec![
            valid.readers[0].clone(),
            valid.readers[0].clone(),
            valid.readers[1].clone(),
        ];
        for readers in [
            vec![],
            empty_name,
            empty_evidence,
            missing_reader,
            duplicate,
        ] {
            let mut invalid = valid.clone();
            invalid.readers = readers;
            assert!(invalid.validate().is_err());
            let mut journal = ProviderLifecycleJournal::default();
            journal.standalone_withdrawal_intents.push(invalid);
            assert!(journal.to_json_string().is_err());
        }
        let mut with_new_reader = valid;
        with_new_reader
            .readers
            .push(StandaloneWithdrawalReaderProof {
                reader: "copilot".into(),
                intent: StandaloneWithdrawalReaderIntent::Disabled,
                evidence: "fixture: additional current reader deliberately disabled".into(),
            });
        with_new_reader.validate().unwrap();
    }

    #[test]
    fn standalone_withdrawal_rejects_invalid_paths_preimages_and_backup_bindings() {
        let valid = standalone_withdrawal_fixture();
        let mut absent = valid.clone();
        absent.preimage = StandalonePreimage::Absent;
        let mut mismatched_tree = valid.clone();
        mismatched_tree.preimage = StandalonePreimage::Tree {
            fingerprint: PackageDigest::new("sha256:foreign").unwrap(),
        };
        let mut wrong_destination = valid.clone();
        wrong_destination.preimage_ref.path.path =
            RelativePath::new(".agents/skills/foreign").unwrap();
        let mut wrong_backup = valid.clone();
        wrong_backup.preimage_ref.saved = RelativePath::new("foreign/backup").unwrap();
        let mut tree_link_kind = valid.clone();
        tree_link_kind.link_kind = Some(StandaloneLinkKind::Junction);
        let mut empty_link = valid.clone();
        empty_link.preimage = StandalonePreimage::Link {
            raw_target: PathBuf::new(),
        };
        empty_link.link_kind = Some(StandaloneLinkKind::Junction);
        let mut missing_link_kind = valid.clone();
        missing_link_kind.preimage = StandalonePreimage::Link {
            raw_target: PathBuf::from("../original"),
        };
        let mut unsafe_operation = valid;
        unsafe_operation.operation_id = "../escape".into();
        for invalid in [
            absent,
            mismatched_tree,
            wrong_destination,
            wrong_backup,
            tree_link_kind,
            empty_link,
            missing_link_kind,
            unsafe_operation,
        ] {
            assert!(invalid.validate().is_err());
        }
        let mut encoded = serde_json::to_value(standalone_withdrawal_fixture()).unwrap();
        for path in ["/destination/path", "/preimageRef/saved", "/backupPath"] {
            let original = encoded.pointer(path).unwrap().clone();
            *encoded.pointer_mut(path).unwrap() = json!("../../escape");
            assert!(serde_json::from_value::<StandaloneWithdrawalIntent>(encoded.clone()).is_err());
            *encoded.pointer_mut(path).unwrap() = original;
        }
    }

    #[test]
    fn standalone_withdrawal_rejects_registry_identity_and_absence_drift() {
        let original = serde_json::to_value(standalone_withdrawal_fixture()).unwrap();
        for (path, replacement) in [
            ("/sourceId", json!("foreign-source")),
            ("/sourceRevision", json!("foreign-revision")),
            ("/registryBefore", Value::Null),
            ("/registryBefore/source", json!("foreign-source")),
            ("/registryBefore/categories", json!([])),
            (
                "/registryBefore/categories",
                json!(["directSkillProjectionPaths", "directSkillProjectionPaths"]),
            ),
            (
                "/registryBefore/standalone/sourceFingerprint",
                json!("sha256:foreign"),
            ),
            (
                "/registryBefore/standalone/treeFingerprint",
                json!("sha256:foreign"),
            ),
            (
                "/registryBefore/standalone/targets",
                json!(["unknown-reader"]),
            ),
            ("/registryAfter", Value::Null),
            (
                "/registryAfter/categories",
                json!(["directSkillProjectionPaths"]),
            ),
            ("/registryAfter/source", json!("actual-provider")),
            ("/registryAfter/standalone", json!({"retained": true})),
        ] {
            let mut value = original.clone();
            *value.pointer_mut(path).unwrap() = replacement;
            let intent: StandaloneWithdrawalIntent = serde_json::from_value(value).unwrap();
            assert!(intent.validate().is_err(), "{path}");
        }
        for missing in ["categories", "source", "standalone"] {
            let mut intent = standalone_withdrawal_fixture();
            intent
                .registry_after
                .as_object_mut()
                .unwrap()
                .remove(missing);
            assert!(intent.validate().is_err(), "{missing}");
        }
    }

    #[test]
    fn standalone_withdrawal_rejects_fake_publication_fields_and_unknown_reader_intent() {
        for field in [
            "sourcePath",
            "stagePath",
            "expectedTreeFingerprint",
            "selector",
        ] {
            let mut value = serde_json::to_value(standalone_withdrawal_fixture()).unwrap();
            value[field] = json!("fabricated");
            assert!(
                serde_json::from_value::<StandaloneWithdrawalIntent>(value).is_err(),
                "{field}"
            );
        }
        let mut value = serde_json::to_value(standalone_withdrawal_fixture()).unwrap();
        value["readers"][0]["intent"] = json!("unknown");
        assert!(serde_json::from_value::<StandaloneWithdrawalIntent>(value).is_err());
    }

    #[test]
    fn standalone_withdrawal_frozen_old_version_gates_reject_empty_and_pending_v3() {
        // Frozen pre-deserialization rejection paths from provider.rs at
        // b9fe6ee8^ (v1) and e77e756b (v2). These are decoder gates, not old CLIs.
        fn old_v1_gate(text: &str) -> Result<(), ProviderContractError> {
            let value: Value = serde_json::from_str(text)
                .map_err(|e| malformed("provider lifecycle journal", e))?;
            check_schema_version("provider lifecycle journal", &value, 1)
        }
        fn old_v2_gate(text: &str) -> Result<(), ProviderContractError> {
            let value: Value = serde_json::from_str(text)
                .map_err(|e| malformed("provider lifecycle journal", e))?;
            if value
                .get("schemaVersion")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                == 1
            {
                return Ok(());
            }
            check_schema_version("provider lifecycle journal", &value, 2)
        }
        let temp = tempfile::TempDir::new().unwrap();
        for pending in [false, true] {
            let mut journal = ProviderLifecycleJournal::default();
            if pending {
                journal
                    .standalone_withdrawal_intents
                    .push(standalone_withdrawal_fixture());
            }
            let text = journal.to_json_string().unwrap();
            let path = temp.path().join(format!("v3-pending-{pending}.json"));
            std::fs::write(&path, &text).unwrap();
            let input = std::fs::read_to_string(&path).unwrap();
            for (version, result) in [(1, old_v1_gate(&input)), (2, old_v2_gate(&input))] {
                assert!(
                    matches!(result, Err(ProviderContractError::UnsupportedSchemaVersion { supported, .. }) if supported == version)
                );
                assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes());
            }
        }
    }

    use serde_json::json;

    fn key(value: &str) -> ProviderSourceKey {
        ProviderSourceKey::new(value).expect("valid key")
    }

    fn generation(digest: &str) -> ProviderGeneration {
        ProviderGeneration {
            source: key("demo"),
            generation: GenerationId::new("g-0001").expect("valid generation"),
            package_digest: PackageDigest::new(digest).expect("valid digest"),
        }
    }

    fn committed(digest: &str) -> CommittedOwnership {
        CommittedOwnership {
            target: ProjectionTargetId::Codex,
            selector: NativeSelector::new("demo@ccync-marketplace").expect("valid selector"),
            plugin_name: "demo".to_string(),
            upstream_revision: Some("abc123".to_string()),
            generation: generation(digest),
            capabilities: [ComponentKind::Skill, ComponentKind::Hook]
                .into_iter()
                .collect(),
            client_scope: Some(ClientScope::new("demo-client").expect("valid scope")),
            source_digest: Some(PackageDigest::new(digest).expect("valid digest")),
            view_digest: Some(PackageDigest::new("sha256:view").expect("valid digest")),
            observed_preimage: None,
            enablement_intent: Some(EnablementIntent::Enabled),
            group_substep: None,
        }
    }

    #[test]
    fn delivery_identity_distinguishes_same_name_origins_and_ignores_revision() {
        let make = |reference: &str, revision: &str| {
            let mut identity =
                ComponentIdentity::new(key("same"), "display", None, ComponentKind::Skill, "skill")
                    .unwrap();
            identity.origin = Some(ComponentOrigin {
                structure_version: 1,
                canonical_reference: Some(reference.to_string()),
                resolved_path: None,
                revision: Some(revision.to_string()),
                digest: None,
                enabled: Some(true),
            });
            identity
        };
        let one = make("repo:one", "rev-a");
        let revision = make("repo:one", "rev-b");
        let other = make("repo:two", "rev-a");
        assert!(one.same_component(&revision));
        assert!(!one.same_component(&other));
        let roundtrip: ComponentIdentity =
            serde_json::from_value(serde_json::to_value(&one).unwrap()).unwrap();
        assert_eq!(roundtrip, one);
    }

    #[test]
    fn delivery_identity_rejects_unsafe_and_unknown_origin_records() {
        for path in ["relative/path", "/safe/../escape"] {
            let value = json!({"source":"src", "pluginName":"p", "kind":"skill", "componentId":"x", "origin":{"structureVersion":1,"resolvedPath":path}});
            assert!(serde_json::from_value::<ComponentIdentity>(value).is_err());
        }
        let unknown = json!({"source":"src", "pluginName":"p", "kind":"skill", "componentId":"x", "origin":{"structureVersion":2,"canonicalReference":"repo:x"}});
        assert!(serde_json::from_value::<ComponentIdentity>(unknown).is_err());
    }

    #[test]
    fn delivery_identity_legacy_records_are_unmanaged() {
        let identity =
            ComponentIdentity::new(key("src"), "p", None, ComponentKind::Skill, "x").unwrap();
        let attribution = ArtifactAttribution {
            ownership: OwnershipState::Committed,
            layout_version: None,
            root_identity: None,
            identity: Some(identity),
        };
        assert_eq!(attribution.effective_ownership(), OwnershipState::Unowned);
    }

    #[test]
    fn provider_source_key_accepts_safe_values() {
        for value in ["demo", "monkey1wizard-demo", "demo_core.v2", "a"] {
            assert!(ProviderSourceKey::new(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn provider_source_key_rejects_unsafe_values() {
        for value in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "C:",
            "c:demo",
            "x<y",
            "x|y",
            "x?",
            "x*",
            "trail.",
            "trail ",
            " lead",
            "CON",
            "nul",
            "com1.json",
            "LPT9",
            "tab\tkey",
        ] {
            assert!(ProviderSourceKey::new(value).is_err(), "{value:?}");
        }
        assert!(ProviderSourceKey::new("x".repeat(MAX_SEGMENT_LEN + 1)).is_err());
    }

    #[test]
    fn provider_source_key_rejects_unsafe_values_on_deserialize() {
        let parsed: Result<ProviderSourceKey, _> = serde_json::from_value(json!("../escape"));
        assert!(parsed.is_err());
        let parsed: Result<GenerationId, _> = serde_json::from_value(json!(".."));
        assert!(parsed.is_err());
    }

    #[test]
    fn provider_relative_path_rejects_traversal_and_absolute() {
        for value in [
            "",
            "../x",
            "a/../b",
            "a/./b",
            "/abs",
            "\\abs",
            "C:/x",
            "C:x",
            "a//b",
            "a/",
            "file.txt:stream",
        ] {
            assert!(RelativePath::new(value).is_err(), "{value:?}");
        }
        let ok = RelativePath::new("skills\\demo\\SKILL.md").expect("valid");
        assert_eq!(ok.as_str(), "skills/demo/SKILL.md");
    }

    #[test]
    fn provider_relative_path_from_within_rejects_out_of_root() {
        let root = Path::new("/home/u/.ccync/build");
        let inside = root.join("render").join("providers");
        assert_eq!(
            RelativePath::from_within(root, &inside)
                .expect("inside")
                .as_str(),
            "render/providers"
        );
        assert!(RelativePath::from_within(root, Path::new("/home/u/.claude")).is_err());
        assert!(RelativePath::from_within(root, &root.join("..").join("escape")).is_err());
        assert!(RelativePath::from_within(root, root).is_err());
    }

    #[test]
    fn provider_relative_path_join_stays_under_root() {
        let root = PathBuf::from("root");
        let joined = RelativePath::new("a/b").expect("valid").join_to(&root);
        assert!(joined.starts_with(&root));
        assert_eq!(joined, root.join("a").join("b"));
    }

    #[test]
    fn provider_lock_without_namespace_is_empty_and_preserves_other_keys() {
        let original = json!({
            "_personalPlugins": [{"id": "x", "pin": "abc"}],
            "_mcpServers": {"memory": {"command": "npx"}},
            "resolvedPlugins": {"demo": {"sha": "def"}}
        });
        let section = ProviderLifecycleLock::from_lock(&original).expect("old lock parses");
        assert!(section.committed.is_empty());

        let mut rewritten = original.clone();
        section.write_to_lock(&mut rewritten).expect("write");
        let object = rewritten.as_object_mut().expect("object");
        assert!(object.contains_key(PROVIDER_LIFECYCLE_NAMESPACE));
        object.remove(PROVIDER_LIFECYCLE_NAMESPACE);
        assert_eq!(rewritten, original);

        let text = serde_json::to_string(&original).expect("serialize");
        let reparsed: Value = serde_json::from_str(&text).expect("reparse");
        assert_eq!(reparsed, original);
    }

    #[test]
    fn provider_lock_with_namespace_roundtrips() {
        let mut lock = json!({"_personalPlugins": []});
        let section = ProviderLifecycleLock {
            schema_version: PROVIDER_LIFECYCLE_SCHEMA_VERSION,
            committed: vec![committed("sha256:aa")],
        };
        section.write_to_lock(&mut lock).expect("write");
        let text = serde_json::to_string_pretty(&lock).expect("serialize");
        let reparsed: Value = serde_json::from_str(&text).expect("reparse");
        let back = ProviderLifecycleLock::from_lock(&reparsed).expect("read");
        assert_eq!(back, section);
        assert_eq!(reparsed["_personalPlugins"], json!([]));
        assert_eq!(
            reparsed[PROVIDER_LIFECYCLE_NAMESPACE]["committed"][0]["target"],
            json!("codex")
        );
        assert!(back
            .find(
                ProjectionTargetId::Codex,
                &NativeSelector::new("demo@ccync-marketplace").expect("selector")
            )
            .is_some());
        assert!(back
            .find(
                ProjectionTargetId::Claude,
                &NativeSelector::new("demo@ccync-marketplace").expect("selector")
            )
            .is_none());
    }

    #[test]
    fn provider_lock_rejects_unsupported_schema_version() {
        for version in [json!(2), json!(0), json!("1"), Value::Null] {
            let lock = json!({
                PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": version, "committed": []}
            });
            assert!(matches!(
                ProviderLifecycleLock::from_lock(&lock),
                Err(ProviderContractError::UnsupportedSchemaVersion { .. })
            ));
        }
        let missing = json!({PROVIDER_LIFECYCLE_NAMESPACE: {"committed": []}});
        assert!(matches!(
            ProviderLifecycleLock::from_lock(&missing),
            Err(ProviderContractError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn provider_lock_rejects_unsafe_source_key_in_record() {
        let mut record = serde_json::to_value(committed("sha256:aa")).expect("value");
        record["generation"]["source"] = json!("../outside");
        let lock = json!({
            PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": 1, "committed": [record]}
        });
        assert!(matches!(
            ProviderLifecycleLock::from_lock(&lock),
            Err(ProviderContractError::Malformed { .. })
        ));
    }

    #[test]
    fn provider_journal_roundtrips_and_rejects_unknown_version() {
        let intent = PendingIntent {
            target: ProjectionTargetId::Claude,
            ownership_fingerprints: vec![OwnershipFingerprint {
                selector: NativeSelector::new("demo@ccync").expect("selector"),
                package_digest: PackageDigest::new("sha256:bb").expect("digest"),
            }],
            scoped_paths: vec![ScopedPath {
                root: PathRoot::UserHome,
                path: RelativePath::new(".claude/skills/ccync").expect("path"),
            }],
            preimage_refs: vec![PreimageRef {
                path: ScopedPath {
                    root: PathRoot::UserHome,
                    path: RelativePath::new(".claude/skills/ccync").expect("path"),
                },
                saved: RelativePath::new("preimages/0001").expect("path"),
            }],
            previous_generation: Some(generation("sha256:aa")),
            desired_generation: generation("sha256:bb"),
            phase: IntentPhase::Publishing,
            client_scope: None,
            source_digest: None,
            view_digest: None,
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        };
        let journal = ProviderLifecycleJournal {
            schema_version: PROVIDER_JOURNAL_SCHEMA_VERSION,
            intents: vec![intent],
            standalone_intents: Vec::new(),
            standalone_withdrawal_intents: Vec::new(),
            cutover_targets: None,
            combined_entries_and_link_to_retire: None,
            redundant_managed_copy_paths: None,
        };
        let text = journal.to_json_string().expect("serialize");
        assert_eq!(
            ProviderLifecycleJournal::from_json_str(&text).expect("parse"),
            journal
        );

        let mut cutover = journal.clone();
        cutover.cutover_targets = Some(vec![ProjectionTargetId::Claude, ProjectionTargetId::Codex]);
        cutover.combined_entries_and_link_to_retire = Some(vec![ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".claude/skills/ccync").unwrap(),
        }]);
        cutover.redundant_managed_copy_paths = Some(vec![ScopedPath {
            root: PathRoot::UserHome,
            path: RelativePath::new(".agents/skills/ccync").unwrap(),
        }]);
        let cutover_text = cutover.to_json_string().expect("serialize cutover");
        assert_eq!(
            ProviderLifecycleJournal::from_json_str(&cutover_text).unwrap(),
            cutover
        );
        assert_eq!(
            serde_json::from_str::<Value>(&cutover_text).unwrap()["schemaVersion"],
            json!(PROVIDER_JOURNAL_SCHEMA_VERSION)
        );
        let legacy = r#"{"schemaVersion":1,"intents":[]}"#;
        let legacy = ProviderLifecycleJournal::from_json_str(legacy).unwrap();
        assert!(legacy.cutover_targets.is_none());
        assert_eq!(
            ProviderLifecycleJournal::from_json_str(&legacy.to_json_string().unwrap()).unwrap(),
            legacy
        );

        let future = r#"{"schemaVersion": 99, "intents": []}"#;
        assert!(matches!(
            ProviderLifecycleJournal::from_json_str(future),
            Err(ProviderContractError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn provider_journal_rejects_traversing_scoped_path() {
        let text = json!({
            "schemaVersion": 1,
            "intents": [{
                "target": "codex",
                "ownershipFingerprints": [],
                "scopedPaths": [{"root": "user-home", "path": "../../etc"}],
                "desiredGeneration": {
                    "source": "demo", "generation": "g1", "packageDigest": "sha256:aa"
                },
                "phase": "staged"
            }]
        })
        .to_string();
        assert!(matches!(
            ProviderLifecycleJournal::from_json_str(&text),
            Err(ProviderContractError::Malformed { .. })
        ));
    }

    #[test]
    fn provider_legacy_records_stay_unowned() {
        let legacy = json!({"name": "demo", "path": "skills/demo", "sha": "abc"});
        let attribution = ArtifactAttribution::from_record(&legacy);
        assert_eq!(attribution.effective_ownership(), OwnershipState::Unowned);
        assert!(attribution.identity.is_none());

        // A claimed state without provenance is not ownership.
        let claimed = json!({"ownership": "committed"});
        assert_eq!(
            ArtifactAttribution::from_record(&claimed).effective_ownership(),
            OwnershipState::Unowned
        );

        // Malformed attribution falls back to unowned instead of failing open.
        let malformed = json!({
            "ownership": "committed",
            "identity": {"source": "..", "pluginName": "demo", "kind": "skill", "componentId": "x"}
        });
        assert_eq!(
            ArtifactAttribution::from_record(&malformed).effective_ownership(),
            OwnershipState::Unowned
        );

        let root_identity = std::env::temp_dir().join("portable");
        let attributed = json!({
            "ownership": "committed",
            "layoutVersion": 1,
            "rootIdentity": root_identity.to_string_lossy(),
            "identity": {"source": "demo", "pluginName": "demo", "kind": "skill", "componentId": "pipeline", "origin": {"structureVersion": 1, "canonicalReference": "repo:demo"}}
        });
        assert_eq!(
            ArtifactAttribution::from_record(&attributed).effective_ownership(),
            OwnershipState::Committed
        );
    }

    #[test]
    fn layout_authority_historical_proof_never_authorizes_mutation() {
        let record = json!({
            "ownership": "historical",
            "identity": {"source":"demo", "pluginName":"demo", "kind":"skill", "componentId":"pipeline", "origin":{"structureVersion":1,"canonicalReference":"repo:demo"}}
        });
        let attribution = ArtifactAttribution::from_record(&record);
        assert_eq!(attribution.ownership, OwnershipState::Historical);
        assert_eq!(attribution.effective_ownership(), OwnershipState::Unowned);

        let mut promoted = record;
        promoted["layoutVersion"] = json!(1);
        promoted["rootIdentity"] = json!(std::env::temp_dir().join("portable").to_string_lossy());
        let attribution = ArtifactAttribution::from_record(&promoted);
        assert_eq!(attribution.effective_ownership(), OwnershipState::Unowned);
    }

    #[test]
    fn provider_committed_ownership_requires_exact_match() {
        let record = committed("sha256:aa");
        let selector = NativeSelector::new("demo@ccync-marketplace").expect("selector");
        let required: CapabilityInventory = [ComponentKind::Skill].into_iter().collect();
        assert!(record.proves(
            ProjectionTargetId::Codex,
            &selector,
            &generation("sha256:aa"),
            Some("abc123"),
            &required
        ));
        assert!(!record.proves(
            ProjectionTargetId::Codex,
            &selector,
            &generation("sha256:bb"),
            Some("abc123"),
            &required
        ));
        assert!(!record.proves(
            ProjectionTargetId::Codex,
            &selector,
            &generation("sha256:aa"),
            Some("other"),
            &required
        ));
        assert!(!record.proves(
            ProjectionTargetId::Codex,
            &NativeSelector::new("demo@demo").expect("selector"),
            &generation("sha256:aa"),
            Some("abc123"),
            &required
        ));
        let needs_agent: CapabilityInventory = [ComponentKind::Agent].into_iter().collect();
        assert!(!record.proves(
            ProjectionTargetId::Codex,
            &selector,
            &generation("sha256:aa"),
            Some("abc123"),
            &needs_agent
        ));
    }

    #[test]
    fn provider_native_observation_blocking_states() {
        use NativeObservation::*;
        for state in [Unavailable, External, Disabled, Broken, Unreadable, Drifted] {
            assert!(state.blocks_provider_change(), "{state:?}");
        }
        for state in [Absent, OwnedOld, Desired] {
            assert!(!state.blocks_provider_change(), "{state:?}");
        }
        assert_eq!(
            serde_json::to_value(OwnedOld).expect("value"),
            json!("owned-old")
        );
        assert_eq!(OwnershipState::default(), OwnershipState::Unowned);
    }

    #[test]
    fn provider_component_identity_validates_and_reuses_pin_precedence() {
        let entry = json!({"pinnedSha": "def456", "sha": "legacy"});
        let revision = upstream_revision_from_lock_entry(&entry);
        assert_eq!(revision.as_deref(), Some("def456"));
        let identity = ComponentIdentity::new(
            key("demo"),
            "demo",
            revision,
            ComponentKind::Command,
            "pipeline",
        )
        .expect("valid");
        let alias = TargetAlias::new(
            ProjectionTargetId::Opencode,
            identity.clone(),
            "demo-pipeline",
        )
        .expect("alias");
        let value = serde_json::to_value(&alias).expect("value");
        assert_eq!(value["target"], json!("opencode"));
        assert_eq!(value["identity"]["componentId"], json!("pipeline"));
        let back: TargetAlias = serde_json::from_value(value).expect("roundtrip");
        assert_eq!(back, alias);

        assert!(ComponentIdentity::new(key("demo"), "", None, ComponentKind::Skill, "x").is_err());
        assert!(
            TargetAlias::new(ProjectionTargetId::Codex, identity.clone(), "demo/pipeline").is_err()
        );
        assert!(serde_json::from_value::<TargetAlias>(json!({
            "target": "nowhere",
            "identity": serde_json::to_value(&identity).expect("value"),
            "alias": "x"
        }))
        .is_err());
    }

    #[test]
    fn provider_generation_path_resolves_under_render_providers() {
        let _guard = crate::test_support::env_guard();
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let prior = std::env::var_os(var);
        let fake_home = tempfile::TempDir::new().expect("tempdir");
        std::env::set_var(var, fake_home.path());

        let resolved = generation("sha256:aa").path();
        let journal = crate::paths::provider_lifecycle_journal_path();
        let scoped = ScopedPath {
            root: PathRoot::CcyncHome,
            path: RelativePath::new("build/render").expect("path"),
        }
        .resolve();

        match prior {
            Some(value) => std::env::set_var(var, value),
            None => std::env::remove_var(var),
        }

        let ccync = fake_home.path().join(".ccync");
        assert_eq!(
            resolved,
            Some(
                ccync
                    .join("build")
                    .join("render")
                    .join("providers")
                    .join("demo")
                    .join("g-0001")
            )
        );
        assert_eq!(
            journal,
            Some(ccync.join("build").join("projection.txn.json"))
        );
        assert_eq!(scoped, Some(ccync.join("build").join("render")));
    }

    // ── independent adversarial coverage ────────────────────────────────────

    fn schema_edge_values() -> Vec<Value> {
        vec![
            json!(0),
            json!(-1),
            json!(1.0),
            json!(1.5),
            json!(2),
            json!(4_294_967_297_u64),
            json!(true),
            json!("1"),
            json!({}),
            json!([1]),
            Value::Null,
        ]
    }

    #[test]
    fn provider_lock_schema_version_edge_values_fail_closed() {
        for version in schema_edge_values() {
            let lock = json!({
                PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": version.clone(), "committed": []}
            });
            assert!(
                matches!(
                    ProviderLifecycleLock::from_lock(&lock),
                    Err(ProviderContractError::UnsupportedSchemaVersion { .. })
                ),
                "lock schemaVersion {version}"
            );
        }
    }

    #[test]
    fn provider_journal_schema_version_edge_values_fail_closed() {
        for version in schema_edge_values() {
            if version == json!(2) || version == json!(PROVIDER_JOURNAL_SCHEMA_VERSION) {
                continue;
            }
            let text = json!({"schemaVersion": version.clone(), "intents": []}).to_string();
            assert!(
                matches!(
                    ProviderLifecycleJournal::from_json_str(&text),
                    Err(ProviderContractError::UnsupportedSchemaVersion { .. })
                ),
                "journal schemaVersion {version}"
            );
        }
        let missing = r#"{"intents": []}"#;
        assert!(matches!(
            ProviderLifecycleJournal::from_json_str(missing),
            Err(ProviderContractError::UnsupportedSchemaVersion { .. })
        ));
        for bad in ["", "not json", "[]", "null", "\"text\""] {
            assert!(
                ProviderLifecycleJournal::from_json_str(bad).is_err(),
                "journal text {bad:?}"
            );
        }
    }

    #[test]
    fn provider_lock_non_object_shapes_fail_closed() {
        for namespace in [Value::Null, json!([]), json!("x"), json!(1)] {
            let lock = json!({ PROVIDER_LIFECYCLE_NAMESPACE: namespace.clone() });
            assert!(
                ProviderLifecycleLock::from_lock(&lock).is_err(),
                "namespace {namespace}"
            );
        }
        for root in [Value::Null, json!([]), json!("lock")] {
            assert!(ProviderLifecycleLock::from_lock(&root).is_err(), "{root}");
            let mut target = root.clone();
            assert!(ProviderLifecycleLock::default()
                .write_to_lock(&mut target)
                .is_err());
            assert_eq!(target, root, "failed write must not mutate {root}");
        }
        let committed_not_list = json!({
            PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": 1, "committed": {"a": 1}}
        });
        assert!(matches!(
            ProviderLifecycleLock::from_lock(&committed_not_list),
            Err(ProviderContractError::Malformed { .. })
        ));
    }

    #[test]
    fn provider_lock_rejects_malformed_committed_records() {
        let base = serde_json::to_value(committed("sha256:aa")).expect("value");
        let mutations: Vec<(&str, Value)> = vec![
            ("pluginName", json!("")),
            ("pluginName", json!("  ")),
            ("pluginName", json!("ga\nl")),
            ("target", json!("nowhere")),
            ("selector", json!("")),
            ("selector", json!("demo\u{0}@x")),
            ("capabilities", json!(["teleport"])),
        ];
        for (field, bad) in mutations {
            let mut record = base.clone();
            record[field] = bad.clone();
            let lock = json!({
                PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": 1, "committed": [record]}
            });
            assert!(
                ProviderLifecycleLock::from_lock(&lock).is_err(),
                "{field} = {bad}"
            );
        }
        for (field, bad) in [
            ("source", json!("CON")),
            ("source", json!("a/b")),
            ("generation", json!("..")),
            ("generation", json!("C:")),
            ("packageDigest", json!("sha256: aa")),
            ("packageDigest", json!("")),
        ] {
            let mut record = base.clone();
            record["generation"][field] = bad.clone();
            let lock = json!({
                PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": 1, "committed": [record]}
            });
            assert!(
                ProviderLifecycleLock::from_lock(&lock).is_err(),
                "generation.{field} = {bad}"
            );
        }
    }

    #[test]
    fn provider_lock_write_replaces_only_namespace_and_pins_schema() {
        let original = json!({
            "_personalPlugins": [{"id": "x", "pin": "abc", "nested": {"deep": [1, 2, {"k": null}]}}],
            "_mcpServers": {"memory": {"command": "npx", "args": ["-y", "pkg"]}},
            "unknownFutureKey": {"anything": true},
            PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": 1, "committed": []}
        });
        let section = ProviderLifecycleLock {
            schema_version: 42,
            committed: vec![committed("sha256:cc")],
        };
        let mut rewritten = original.clone();
        section.write_to_lock(&mut rewritten).expect("write");
        assert_eq!(
            rewritten[PROVIDER_LIFECYCLE_NAMESPACE]["schemaVersion"],
            json!(PROVIDER_LIFECYCLE_SCHEMA_VERSION)
        );
        let back = ProviderLifecycleLock::from_lock(&rewritten).expect("read back");
        assert_eq!(back.committed, section.committed);

        let mut without = rewritten.clone();
        let mut expected = original.clone();
        without
            .as_object_mut()
            .expect("object")
            .remove(PROVIDER_LIFECYCLE_NAMESPACE);
        expected
            .as_object_mut()
            .expect("object")
            .remove(PROVIDER_LIFECYCLE_NAMESPACE);
        assert_eq!(without, expected);

        let journal = ProviderLifecycleJournal {
            schema_version: 9,
            intents: Vec::new(),
            standalone_intents: Vec::new(),
            standalone_withdrawal_intents: Vec::new(),
            cutover_targets: None,
            combined_entries_and_link_to_retire: None,
            redundant_managed_copy_paths: None,
        };
        let text = journal.to_json_string().expect("serialize");
        assert_eq!(
            ProviderLifecycleJournal::from_json_str(&text)
                .expect("parse")
                .schema_version,
            PROVIDER_JOURNAL_SCHEMA_VERSION
        );
    }

    #[test]
    fn provider_attribution_never_infers_ownership_from_malformed_records() {
        let valid_identity = json!({
            "source": "demo", "pluginName": "demo", "kind": "skill", "componentId": "pipeline",
            "origin": {"structureVersion": 1, "canonicalReference": "repo:demo"}
        });
        let unowned_cases = vec![
            json!("committed"),
            json!(["committed"]),
            Value::Null,
            json!({}),
            json!({"ownership": "bogus", "identity": valid_identity.clone()}),
            json!({"ownership": "COMMITTED", "identity": valid_identity.clone()}),
            json!({"ownership": "committed", "identity": null}),
            json!({"ownership": "committed", "identity": "demo"}),
            json!({"identity": valid_identity.clone()}),
            json!({"ownership": "committed", "identity": {
                "source": "demo", "pluginName": "", "kind": "skill", "componentId": "x"
            }}),
            json!({"ownership": "committed", "identity": {
                "source": "demo", "pluginName": "demo", "kind": "skill", "componentId": "a\u{7}b"
            }}),
            json!({"ownership": "committed", "identity": {
                "source": "demo", "pluginName": "demo", "kind": "skill", "componentId": "x",
                "upstreamRevision": ""
            }}),
            json!({"ownership": "committed", "identity": {
                "source": "demo", "pluginName": "demo", "componentId": "x"
            }}),
            json!({"ownership": "committed", "identity": {
                "source": "demo", "pluginName": "demo", "kind": "widget", "componentId": "x"
            }}),
            json!({"ownership": "committed", "identity": {
                "source": "a\\b", "pluginName": "demo", "kind": "skill", "componentId": "x"
            }}),
            json!({"ownership": "committed", "identity": {
                "source": "NUL", "pluginName": "demo", "kind": "skill", "componentId": "x"
            }}),
        ];
        for record in unowned_cases {
            let attribution = ArtifactAttribution::from_record(&record);
            assert_eq!(
                attribution.effective_ownership(),
                OwnershipState::Unowned,
                "{record}"
            );
        }

        let pending = json!({"ownership": "pending", "identity": valid_identity.clone()});
        // Pending intent is never actionable ownership.
        assert_eq!(
            ArtifactAttribution::from_record(&pending).effective_ownership(),
            OwnershipState::Unowned
        );

        // A hand-built attribution with no identity never acts as owned.
        let claimed = ArtifactAttribution {
            ownership: OwnershipState::Committed,
            layout_version: None,
            root_identity: None,
            identity: None,
        };
        assert_eq!(claimed.effective_ownership(), OwnershipState::Unowned);

        // Unowned default omits identity on serialization and reparses unowned.
        let text = serde_json::to_value(ArtifactAttribution::default()).expect("value");
        assert_eq!(text, json!({"ownership": "unowned"}));
    }

    #[test]
    fn provider_relative_path_rejects_windows_escape_forms() {
        for value in [
            "\\\\?\\C:\\x",
            "\\\\.\\NUL",
            "\\\\server\\share\\x",
            "//server/share",
            "\\??\\C:\\x",
            "C:foo",
            "c:\\x",
            "a\\..\\b",
            "a/b\\..\\..\\c",
            "..\\x",
            "./a",
            "a/.",
            "a\\\\b",
            "a\u{0}b",
            "a\nb",
            "skills/x:$DATA",
        ] {
            assert!(RelativePath::new(value).is_err(), "{value:?}");
        }
        let mixed = RelativePath::new("skills\\demo/SKILL.md").expect("mixed separators");
        assert_eq!(mixed.as_str(), "skills/demo/SKILL.md");
        let reparsed: RelativePath =
            serde_json::from_value(serde_json::to_value(&mixed).expect("value")).expect("parse");
        assert_eq!(reparsed, mixed);
        assert!(serde_json::from_value::<RelativePath>(json!("..\\..\\x")).is_err());
    }

    #[test]
    fn provider_relative_path_rejects_device_name_components() {
        // On Windows the final component `NUL` (also `NUL.` and `NUL `)
        // resolves to the `\\.\NUL` device rather than a file under the root,
        // so a scoped path ending in one escapes its root.
        for value in [
            "NUL",
            "skills/NUL",
            "skills/nul",
            "skills/NUL.",
            "skills/NUL ",
        ] {
            assert!(RelativePath::new(value).is_err(), "{value:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn provider_relative_path_from_within_windows_prefix_mismatch_fails_closed() {
        let root = Path::new(r"C:\Users\u\.ccync\build");
        assert_eq!(
            RelativePath::from_within(root, &root.join("render"))
                .expect("inside")
                .as_str(),
            "render"
        );
        for candidate in [
            r"\\?\C:\Users\u\.ccync\build\render",
            r"D:\Users\u\.ccync\build\render",
            r"C:Users\u\.ccync\build\render",
            r"\\server\share\Users\u\.ccync\build\render",
            r"C:\Users\u\.ccync\build\..\escape",
            r"C:\Users\u\.ccync\buildx\render",
        ] {
            assert!(
                RelativePath::from_within(root, Path::new(candidate)).is_err(),
                "{candidate}"
            );
        }
    }

    #[test]
    fn provider_relative_path_from_within_rejects_sibling_prefix() {
        let root = Path::new("/home/u/.ccync/build");
        assert!(RelativePath::from_within(root, Path::new("/home/u/.ccync/buildx/a")).is_err());
        assert!(RelativePath::from_within(root, Path::new("relative/a")).is_err());
    }

    #[test]
    fn provider_segment_rejects_additional_windows_forms() {
        for value in [
            "a:b",
            "x\"y",
            "x>y",
            "Con.txt.bak",
            "nul.",
            "Aux",
            "prn.json",
            "\\\\?\\x",
            "x\u{7f}",
        ] {
            assert!(ProviderSourceKey::new(value).is_err(), "{value:?}");
            assert!(GenerationId::new(value).is_err(), "{value:?}");
        }
        assert!(ProviderSourceKey::new("x".repeat(MAX_SEGMENT_LEN)).is_ok());
        let identity = ComponentIdentity::new(key("demo"), "demo", None, ComponentKind::Skill, "x")
            .expect("identity");
        for alias in ["..", "NUL", "a:b", "trail.", ""] {
            assert!(
                TargetAlias::new(ProjectionTargetId::Codex, identity.clone(), alias).is_err(),
                "{alias:?}"
            );
        }
    }

    #[test]
    fn provider_component_identity_rejects_invalid_text_on_deserialize_path() {
        assert!(ComponentIdentity::new(
            key("demo"),
            "demo",
            Some(" ".to_string()),
            ComponentKind::Skill,
            "x"
        )
        .is_err());
        assert!(
            ComponentIdentity::new(key("demo"), "demo", None, ComponentKind::Skill, "a\rb").is_err()
        );
        let alias_json = json!({
            "target": "codex",
            "identity": {"source": "demo", "pluginName": "", "kind": "skill", "componentId": "x"},
            "alias": "ok"
        });
        // Deserialization alone does not run identity validation; callers must
        // revalidate, which must reject the empty plugin name.
        if let Ok(alias) = serde_json::from_value::<TargetAlias>(alias_json) {
            assert!(alias.identity.validate().is_err());
        }
    }

    #[test]
    fn provider_target_alias_rejects_unsafe_alias_on_deserialize() {
        // A flat adapter publishes the alias as a directory or file stem, so a
        // persisted alias must be revalidated when read back.
        let identity = json!({
            "source": "demo", "pluginName": "demo", "kind": "skill", "componentId": "x"
        });
        for alias in ["../escape", "a/b", "a\\b", "..", "", "NUL", "C:"] {
            let value = json!({"target": "codex", "identity": identity.clone(), "alias": alias});
            assert!(
                serde_json::from_value::<TargetAlias>(value).is_err(),
                "alias {alias:?}"
            );
        }
    }

    #[test]
    fn provider_committed_ownership_does_not_match_on_name_or_missing_revision() {
        let record = committed("sha256:aa");
        let selector = NativeSelector::new("demo@ccync-marketplace").expect("selector");
        let empty = CapabilityInventory::new();
        // Same name and digest but different source is not proof.
        let mut other_source = generation("sha256:aa");
        other_source.source = key("demo-fork");
        assert!(!record.proves(
            ProjectionTargetId::Codex,
            &selector,
            &other_source,
            Some("abc123"),
            &empty
        ));
        // Missing upstream revision does not match a recorded one.
        assert!(!record.proves(
            ProjectionTargetId::Codex,
            &selector,
            &generation("sha256:aa"),
            None,
            &empty
        ));
        // Different target with otherwise identical evidence is not proof.
        assert!(!record.proves(
            ProjectionTargetId::Claude,
            &selector,
            &generation("sha256:aa"),
            Some("abc123"),
            &empty
        ));
        // Selector equality is exact, not case-insensitive.
        assert!(!record.proves(
            ProjectionTargetId::Codex,
            &NativeSelector::new("DEMO@ccync-marketplace").expect("selector"),
            &generation("sha256:aa"),
            Some("abc123"),
            &empty
        ));
        // Empty lock proves nothing.
        assert!(ProviderLifecycleLock::default()
            .find(ProjectionTargetId::Codex, &selector)
            .is_none());
    }

    #[test]
    fn provider_journal_rejects_unsafe_nested_values() {
        let base = json!({
            "target": "codex",
            "ownershipFingerprints": [{"selector": "demo@x", "packageDigest": "sha256:aa"}],
            "scopedPaths": [{"root": "user-home", "path": ".codex/plugins/demo"}],
            "preimageRefs": [{
                "path": {"root": "ccync-home", "path": "build/x"},
                "saved": "preimages/1"
            }],
            "desiredGeneration": {"source": "demo", "generation": "g1", "packageDigest": "sha256:aa"},
            "phase": "staged"
        });
        let ok = json!({"schemaVersion": 1, "intents": [base.clone()]}).to_string();
        assert!(ProviderLifecycleJournal::from_json_str(&ok).is_ok());

        let mutate = |pointer: &str, bad: Value| {
            let mut intent = base.clone();
            *intent.pointer_mut(pointer).expect("pointer") = bad;
            json!({"schemaVersion": 1, "intents": [intent]}).to_string()
        };
        for (pointer, bad) in [
            ("/scopedPaths/0/path", json!("C:\\Windows")),
            ("/scopedPaths/0/path", json!("\\\\?\\C:\\x")),
            ("/scopedPaths/0/root", json!("system-root")),
            ("/preimageRefs/0/saved", json!("../../outside")),
            ("/preimageRefs/0/path/path", json!("/etc/passwd")),
            ("/desiredGeneration/source", json!("..")),
            ("/desiredGeneration/generation", json!("a/b")),
            ("/ownershipFingerprints/0/selector", json!("")),
            ("/ownershipFingerprints/0/packageDigest", json!("a b")),
            ("/phase", json!("committed")),
            ("/target", json!("nowhere")),
        ] {
            assert!(
                ProviderLifecycleJournal::from_json_str(&mutate(pointer, bad.clone())).is_err(),
                "{pointer} = {bad}"
            );
        }
    }

    #[test]
    fn provider_relative_path_rejects_trailing_dot_or_space_components() {
        for value in [
            "skills./x",
            "skills /x",
            "a/b.",
            "a/b ",
            "nul.txt",
            "a/Com1.json",
        ] {
            assert!(RelativePath::new(value).is_err(), "{value:?}");
        }
        for value in ["a/.hidden", "a/b.c", "a/null", "a/console"] {
            assert!(RelativePath::new(value).is_ok(), "{value:?}");
        }
    }

    #[test]
    fn provider_committed_record_rejects_invalid_upstream_revision() {
        for bad in [json!(""), json!("  "), json!("abc\ndef")] {
            let mut record = serde_json::to_value(committed("sha256:aa")).expect("value");
            record["upstreamRevision"] = bad.clone();
            let lock = json!({
                PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": 1, "committed": [record]}
            });
            assert!(
                ProviderLifecycleLock::from_lock(&lock).is_err(),
                "upstreamRevision {bad}"
            );
        }
        assert!(committed("sha256:aa").validate().is_ok());
    }

    #[test]
    fn provider_native_selector_rejects_surrounding_whitespace() {
        for value in [" demo@ccync", "demo@ccync ", "\tgal", ""] {
            assert!(NativeSelector::new(value).is_err(), "{value:?}");
        }
        assert!(serde_json::from_value::<NativeSelector>(json!("demo@ccync ")).is_err());
        assert!(NativeSelector::new("demo@ccync-marketplace").is_ok());
    }

    #[test]
    fn p7_t01_legacy_documents_deserialize_with_defaults() {
        let journal = r#"{"schemaVersion":1,"intents":[{"target":"codex","ownershipFingerprints":[],"scopedPaths":[],"desiredGeneration":{"source":"demo","generation":"g1","packageDigest":"sha256:aa"},"phase":"staged"}]}"#;
        let parsed = ProviderLifecycleJournal::from_json_str(journal).expect("legacy journal");
        let intent = &parsed.intents[0];
        assert!(intent.client_scope.is_none());
        assert!(intent.source_digest.is_none());
        assert!(intent.view_digest.is_none());
        assert!(intent.observed_preimage.is_none());
        assert!(intent.enablement_intent.is_none());
        assert!(intent.group_substep.is_none());

        let lock = json!({
            PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": 1, "committed": [{
                "target": "codex",
                "selector": "demo@ccync-marketplace",
                "pluginName": "demo",
                "upstreamRevision": "abc123",
                "generation": {"source": "demo", "generation": "g-0001", "packageDigest": "sha256:aa"},
                "capabilities": ["skill"]
            }]}
        });
        let section = ProviderLifecycleLock::from_lock(&lock).expect("legacy lock row");
        let row = &section.committed[0];
        assert!(row.client_scope.is_none());
        assert!(row.source_digest.is_none());
        assert!(row.view_digest.is_none());
        assert!(row.enablement_intent.is_none());
        assert!(row.group_substep.is_none());

        // A legacy row missing proof never grants ownership.
        let selector = NativeSelector::new("demo@ccync-marketplace").expect("selector");
        let empty = CapabilityInventory::new();
        assert!(!row.proves(
            ProjectionTargetId::Codex,
            &selector,
            &generation("sha256:aa"),
            Some("abc123"),
            &empty
        ));
    }

    #[test]
    fn p7_t01_new_fields_round_trip_and_keep_siblings() {
        let digest = |value: &str| PackageDigest::new(value).expect("digest");
        let intent = PendingIntent {
            target: ProjectionTargetId::Codex,
            ownership_fingerprints: Vec::new(),
            scoped_paths: Vec::new(),
            preimage_refs: Vec::new(),
            previous_generation: None,
            desired_generation: generation("sha256:bb"),
            phase: IntentPhase::Staged,
            client_scope: Some(ClientScope::new("codex-cli").expect("scope")),
            source_digest: Some(digest("sha256:src")),
            view_digest: Some(digest("sha256:view")),
            observed_preimage: Some(digest("sha256:pre")),
            enablement_intent: Some(EnablementIntent::Disabled),
            group_substep: Some(GroupSubstep::new("publish-1").expect("substep")),
        };
        let journal = ProviderLifecycleJournal {
            schema_version: PROVIDER_JOURNAL_SCHEMA_VERSION,
            intents: vec![intent],
            standalone_intents: Vec::new(),
            standalone_withdrawal_intents: Vec::new(),
            cutover_targets: None,
            combined_entries_and_link_to_retire: None,
            redundant_managed_copy_paths: None,
        };
        let text = journal.to_json_string().expect("serialize");
        assert_eq!(
            ProviderLifecycleJournal::from_json_str(&text).expect("parse"),
            journal
        );

        let mut lock = json!({"_personalPlugins": [{"id": "x"}]});
        let mut row = committed("sha256:aa");
        row.observed_preimage = Some(digest("sha256:pre"));
        row.group_substep = Some(GroupSubstep::new("publish-1").expect("substep"));
        let section = ProviderLifecycleLock {
            schema_version: PROVIDER_LIFECYCLE_SCHEMA_VERSION,
            committed: vec![row.clone()],
        };
        section.write_to_lock(&mut lock).expect("write");
        assert_eq!(lock["_personalPlugins"], json!([{"id": "x"}]));
        let back = ProviderLifecycleLock::from_lock(&lock).expect("read");
        assert_eq!(back.committed, vec![row.clone()]);

        let selector = NativeSelector::new("demo@ccync-marketplace").expect("selector");
        let empty = CapabilityInventory::new();
        assert!(row.proves(
            ProjectionTargetId::Codex,
            &selector,
            &generation("sha256:aa"),
            Some("abc123"),
            &empty
        ));
    }

    #[test]
    fn p7_t01_rejects_unknown_schema_and_malformed_proof() {
        let base = |field: &str, bad: Value| {
            let mut row = serde_json::to_value(committed("sha256:aa")).expect("value");
            row[field] = bad;
            json!({PROVIDER_LIFECYCLE_NAMESPACE: {"schemaVersion": 1, "committed": [row]}})
        };
        for (field, bad) in [
            ("sourceDigest", json!("sha256: aa")),
            ("sourceDigest", json!("")),
            ("viewDigest", json!("sha256:a\tb")),
            ("clientScope", json!("  ")),
            ("clientScope", json!("demo\u{0}client")),
            ("groupSubstep", json!(" publish-1")),
            ("enablementIntent", json!("maybe")),
        ] {
            assert!(
                ProviderLifecycleLock::from_lock(&base(field, bad.clone())).is_err(),
                "{field} = {bad}"
            );
        }

        let journal_with = |field: &str, bad: Value| {
            let mut intent = json!({
                "target": "codex",
                "ownershipFingerprints": [],
                "scopedPaths": [],
                "desiredGeneration": {"source": "demo", "generation": "g1", "packageDigest": "sha256:aa"},
                "phase": "staged"
            });
            intent[field] = bad;
            json!({"schemaVersion": 1, "intents": [intent]}).to_string()
        };
        assert!(ProviderLifecycleJournal::from_json_str(&journal_with(
            "observedPreimage",
            json!("sha256: aa")
        ))
        .is_err());
        assert!(
            ProviderLifecycleJournal::from_json_str(&journal_with("clientScope", json!("")))
                .is_err()
        );

        let future = json!({"schemaVersion": PROVIDER_JOURNAL_SCHEMA_VERSION + 1, "intents": []})
            .to_string();
        assert!(matches!(
            ProviderLifecycleJournal::from_json_str(&future),
            Err(ProviderContractError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn portable_schema_roundtrips_allowlisted_identity_without_private_fields() {
        let document = PortableSourcesDocument::new(vec![PortableProviderSource {
            source: key("catalog"),
            reference: Some("https://example.test/team/catalog".into()),
            revision: Some("rev-42".into()),
            binding_key: None,
            selection: vec![PortableComponentSelection {
                plugin_name: "sample".into(),
                kind: ComponentKind::Skill,
                component_id: "review".into(),
            }],
            digest: Some(PackageDigest::new("sha256:aabb").unwrap()),
        }])
        .unwrap();
        let encoded = serde_json::to_value(&document).unwrap();
        assert_eq!(
            PortableSourcesDocument::parse(&encoded.to_string()).unwrap(),
            document
        );
        assert!(encoded.get("origin").is_none());
        assert!(encoded.get("nativeSelector").is_none());
        assert!(encoded.get("installationId").is_none());
        assert!(encoded.to_string().find("credentials").is_none());
        println!(
            "portable_schema evidence: fields=schemaVersion,sources,source,reference,revision,selection,digest private_fields=absent"
        );
    }

    #[test]
    fn portable_schema_rejects_credentials_unknown_fields_and_versions() {
        for input in [
            r#"{"schemaVersion":1,"sources":[{"source":"catalog","reference":"https://user:pass@example.test/repo"}]}"#,
            r#"{"schemaVersion":1,"sources":[{"source":"catalog","reference":"https://example.test/repo?token=secret"}]}"#,
            r#"{"schemaVersion":1,"sources":[],"private":{"secret":"x"}}"#,
            r#"{"schemaVersion":2,"sources":[]}"#,
        ] {
            let original = input.to_owned();
            assert!(
                PortableSourcesDocument::parse(&original).is_err(),
                "{input}"
            );
            assert_eq!(original, input);
        }
    }

    #[test]
    fn portable_schema_rejects_non_allowlisted_references_without_mutating_input() {
        for reference in [
            "https://example.test/repo/token%3Dsecret",
            "https://example.test/repo?signature=secret",
            "https://example.test/repo#fragment",
            r"C:\work\catalog",
            "/home/user/catalog",
            "catalog",
            "file:///home/user/catalog",
            "http://example.test/catalog",
            "https://example.test/a/../catalog",
        ] {
            let input = format!(
                r#"{{"schemaVersion":1,"sources":[{{"source":"catalog","reference":"{reference}"}}]}}"#
            );
            let original = input.clone();
            assert!(
                matches!(
                    PortableSourcesDocument::parse(&input),
                    Err(ProviderContractError::Malformed { .. })
                ),
                "{reference:?}"
            );
            assert_eq!(input, original, "{reference:?}");
        }
    }

    #[test]
    fn portable_schema_accepts_case_insensitive_https_scheme_and_numeric_port() {
        let input = r#"{"schemaVersion":1,"sources":[{"source":"catalog","reference":"HTTPS://example.test:443/team/catalog"}]}"#;
        assert!(PortableSourcesDocument::parse(input).is_ok());
    }

    #[test]
    fn portable_schema_local_source_uses_binding_key() {
        let input = r#"{"schemaVersion":1,"sources":[{"source":"local","bindingKey":"worktree"}]}"#;
        let parsed = PortableSourcesDocument::parse(input).unwrap();
        let encoded = serde_json::to_value(parsed).unwrap();
        assert_eq!(encoded["sources"][0]["bindingKey"], "worktree");
        assert!(encoded["sources"][0].get("path").is_none());
    }
}
