//! Self-update support: provenance detection, release fetch/verify, binary swap.
//!
//! Layers: provenance enum + pure `detect_provenance`; `fetch_verified_release`
//! (network seam); `apply_staged` (swap seam); `run_update` orchestrator.

use std::path::Path;

/// How the running `ccync` binary was installed.
///
/// Governs whether `ccync update` may self-replace or must redirect to
/// the native package manager.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum InstallProvenance {
    /// Installed via the curl/irm script to `~/.local/bin/ccync` (POSIX) or
    /// `%USERPROFILE%\.local\bin\ccync.exe` (Windows). Self-replace is allowed.
    SelfManaged,
    /// Installed via `cargo install`. Direct self-replace would race the registry;
    /// redirect to `cargo install --git … --force`.
    Cargo,
    /// Installed via Homebrew. Redirect to `brew upgrade ccync`.
    Homebrew,
    /// Installed via winget. Redirect to `winget upgrade Monkey1Wizard.ccync`.
    Winget,
    /// Path did not match any known installer. Fail-closed: refuse self-replace.
    Unknown,
}

/// Detect the install provenance of the running binary from its path.
///
/// Pure: no filesystem I/O, no environment lookups. The caller is responsible
/// for passing `std::env::current_exe()` (canonicalized or not — both work).
///
/// Detection order (most-specific first):
/// 1. `.cargo/bin`           → Cargo
/// 2. `Cellar`, `homebrew`, `linuxbrew` → Homebrew
/// 3. `Microsoft/WinGet`     → Winget
/// 4. `.local/bin`           → SelfManaged (curl/irm install target)
/// 5. anything else          → Unknown (fail-closed)
pub fn detect_provenance(exe: &Path) -> InstallProvenance {
    let raw = exe.to_string_lossy();
    // Normalize backslashes so a single set of patterns covers both platforms.
    let p = raw.replace('\\', "/");

    if p.contains("/.cargo/bin/") {
        InstallProvenance::Cargo
    } else if p.contains("/Cellar/") || p.contains("/homebrew/") || p.contains("/linuxbrew/") {
        InstallProvenance::Homebrew
    } else if p.contains("/Microsoft/WinGet/") {
        InstallProvenance::Winget
    } else if p.contains("/.local/bin/") {
        InstallProvenance::SelfManaged
    } else {
        InstallProvenance::Unknown
    }
}

// ── release fetch + verify ────────────────────────────────────────────────────

/// Error surface for the self-update flow.
#[derive(Debug)]
pub enum UpdateError {
    /// A network/fetch seam call failed.
    Fetch(String),
    /// `checksums.txt` had no entry for the target artifact.
    ChecksumMissing(String),
    /// The downloaded artifact's SHA-256 did not match the published checksum.
    ChecksumMismatch { expected: String, actual: String },
    /// Extracting the binary from the downloaded archive failed.
    Extract(String),
    /// Swapping the staged binary onto the live target failed.
    Swap(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::Fetch(m) => write!(f, "fetch failed: {m}"),
            UpdateError::ChecksumMissing(a) => write!(f, "no checksum entry for artifact: {a}"),
            UpdateError::ChecksumMismatch { expected, actual } => {
                write!(f, "checksum mismatch: expected {expected}, got {actual}")
            }
            UpdateError::Extract(m) => write!(f, "extract failed: {m}"),
            UpdateError::Swap(m) => write!(f, "swap failed: {m}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// A downloaded + checksum-verified release artifact held in memory, ready to
/// stage to disk (staging/unpack is the swap layer's job).
#[derive(Debug, Clone)]
pub struct StagedBinary {
    /// Verified artifact bytes (the archive exactly as published).
    pub bytes: Vec<u8>,
    /// The release version these bytes correspond to (no leading `v`).
    pub version: String,
}

/// Compare two `x.y.z` version strings; true when `latest` is strictly newer
/// than `current`. A leading `v` is ignored; non-numeric or missing components
/// compare as 0, so malformed input never panics.
pub fn is_newer_version(current: &str, latest: &str) -> bool {
    fn parts(v: &str) -> (u64, u64, u64) {
        let v = v.trim();
        let v = v.strip_prefix('v').unwrap_or(v);
        let mut it = v.split('.').map(|c| c.parse::<u64>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    }
    parts(latest) > parts(current)
}

/// Build the published artifact filename for a release tag + platform. Windows
/// ships a `.zip`; every other OS ships a `.tar.gz`. Mirrors the naming used by
/// `packaging/install.sh` / `install.ps1`.
pub fn artifact_name(tag: &str, os: &str, arch: &str) -> String {
    if os == "windows" {
        format!("ccync-{tag}-windows-{arch}.zip")
    } else {
        format!("ccync-{tag}-{os}-{arch}.tar.gz")
    }
}

/// Look up the SHA-256 hex for `artifact` in a `checksums.txt` body. Lines are
/// `<sha256-hex>  <filename>` (the `sha256sum` format; one or more spaces, an
/// optional `*` binary-mode marker, an optional path prefix).
fn checksum_for(checksums_txt: &str, artifact: &str) -> Option<String> {
    for line in checksums_txt.lines() {
        let mut it = line.split_whitespace();
        let sha = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        if name == artifact || name.ends_with(&format!("/{artifact}")) {
            return Some(sha.to_ascii_lowercase());
        }
    }
    None
}

/// Compute the SHA-256 of `bytes` and compare to `expected_hex` (case-insensitive).
pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> Result<(), UpdateError> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let expected = expected_hex.trim().to_ascii_lowercase();
    if actual == expected {
        Ok(())
    } else {
        Err(UpdateError::ChecksumMismatch { expected, actual })
    }
}

/// Fetch and verify the latest release artifact. Returns a [`StagedBinary`] when
/// `latest_ver` is strictly newer than `current_ver`, or `Ok(None)` when already
/// up to date. Network is injected via seams: `fetch_checksums` returns the
/// `checksums.txt` body and `fetch_artifact` returns the artifact bytes. The
/// artifact is downloaded only after a newer version is confirmed, and is
/// rejected unless its SHA-256 matches the published checksum (verify-before-stage).
pub fn fetch_verified_release(
    current_ver: &str,
    latest_ver: &str,
    artifact: &str,
    fetch_checksums: impl Fn() -> Result<String, String>,
    fetch_artifact: impl Fn() -> Result<Vec<u8>, String>,
) -> Result<Option<StagedBinary>, UpdateError> {
    if !is_newer_version(current_ver, latest_ver) {
        return Ok(None);
    }
    let checksums = fetch_checksums().map_err(UpdateError::Fetch)?;
    let expected = checksum_for(&checksums, artifact)
        .ok_or_else(|| UpdateError::ChecksumMissing(artifact.to_string()))?;
    let bytes = fetch_artifact().map_err(UpdateError::Fetch)?;
    verify_sha256(&bytes, &expected)?;
    let version = latest_ver.trim();
    let version = version.strip_prefix('v').unwrap_or(version).to_string();
    Ok(Some(StagedBinary { bytes, version }))
}

// ── extract + atomic swap ─────────────────────────────────────────────────────

use std::path::PathBuf;

/// Outcome of an [`apply_staged`] swap.
#[derive(Debug, PartialEq, Eq)]
pub struct SwapOutcome {
    /// Where the previous binary was moved aside (`<target>.old`), if one existed.
    pub backup: Option<PathBuf>,
    /// Whether the backup was removed. On Windows the still-mapped old image may
    /// resist deletion while a process holds it; `false` means it was left for a
    /// later best-effort cleanup, not a failure.
    pub backup_removed: bool,
}

/// Append `.old` to a path (next to the original), preserving any extension.
fn backup_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(".old");
    PathBuf::from(s)
}

/// Atomically replace `target` with the already-staged binary at `staged`.
///
/// Strategy (works on POSIX and Windows alike): a running executable can be
/// *renamed/moved* on Windows even though it cannot be overwritten or deleted,
/// so the swap is done in-process with no separate helper:
/// 1. move the current `target` aside to `<target>.old` (if it exists),
/// 2. move `staged` into place as `target`,
/// 3. best-effort remove the `.old` backup.
///
/// If step 2 fails the backup is restored so the live binary stays usable.
/// A `.old` that cannot yet be deleted (Windows, still-mapped) is left behind
/// and reported via `backup_removed: false` for later cleanup.
pub fn apply_staged(staged: &Path, target: &Path) -> Result<SwapOutcome, UpdateError> {
    use std::fs;

    let backup = backup_path(target);
    let had_target = target.exists();

    if had_target {
        fs::rename(target, &backup)
            .map_err(|e| UpdateError::Swap(format!("move current aside: {e}")))?;
    }

    if let Err(e) = fs::rename(staged, target) {
        // Roll back so the live binary is never left missing.
        if had_target {
            let _ = fs::rename(&backup, target);
        }
        return Err(UpdateError::Swap(format!("move staged into place: {e}")));
    }

    let backup_removed = if had_target {
        fs::remove_file(&backup).is_ok()
    } else {
        false
    };

    Ok(SwapOutcome {
        backup: had_target.then_some(backup),
        backup_removed,
    })
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| UpdateError::Extract(format!("get metadata: {e}")))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .map_err(|e| UpdateError::Extract(format!("set executable permissions: {e}")))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}

/// Extract the `ccync` (or `ccync.exe`) binary from a published archive into
/// `dest_dir`, returning the staged binary path. Supports gzip-tar (`is_zip =
/// false`) and zip (`is_zip = true`). The first entry whose file name is
/// `ccync`/`ccync.exe` is written out.
pub fn extract_binary(bytes: &[u8], is_zip: bool, dest_dir: &Path) -> Result<PathBuf, UpdateError> {
    use std::fs;
    use std::io::Read;

    fn is_ccync(name: &str) -> bool {
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        base == "ccync" || base == "ccync.exe"
    }

    fs::create_dir_all(dest_dir)
        .map_err(|e| UpdateError::Extract(format!("create staging dir: {e}")))?;

    if is_zip {
        let reader = std::io::Cursor::new(bytes);
        let mut zip = zip::ZipArchive::new(reader)
            .map_err(|e| UpdateError::Extract(format!("open zip: {e}")))?;
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| UpdateError::Extract(format!("read zip entry: {e}")))?;
            if entry.is_file() && is_ccync(entry.name()) {
                let out = dest_dir.join("ccync.exe");
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| UpdateError::Extract(format!("read zip body: {e}")))?;
                fs::write(&out, &buf)
                    .map_err(|e| UpdateError::Extract(format!("write staged: {e}")))?;
                set_executable(&out)?;
                return Ok(out);
            }
        }
    } else {
        let gz = flate2::read::GzDecoder::new(bytes);
        let mut tar = tar::Archive::new(gz);
        let entries = tar
            .entries()
            .map_err(|e| UpdateError::Extract(format!("read tar: {e}")))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| UpdateError::Extract(format!("tar entry: {e}")))?;
            let name = entry
                .path()
                .map_err(|e| UpdateError::Extract(format!("tar path: {e}")))?
                .to_string_lossy()
                .to_string();
            if is_ccync(&name) {
                let out = dest_dir.join("ccync");
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| UpdateError::Extract(format!("read tar body: {e}")))?;
                fs::write(&out, &buf)
                    .map_err(|e| UpdateError::Extract(format!("write staged: {e}")))?;
                set_executable(&out)?;
                return Ok(out);
            }
        }
    }

    Err(UpdateError::Extract(
        "no ccync binary entry in archive".to_string(),
    ))
}

// ── update orchestrator ───────────────────────────────────────────────────────

/// Inputs for [`run_update`]: the running binary's identity + flags.
#[derive(Debug, Clone)]
pub struct UpdateOpts {
    /// Compile-time version of the running binary (`env!("CARGO_PKG_VERSION")`).
    pub current_version: String,
    /// Path to the running binary (`std::env::current_exe()`).
    pub exe_path: PathBuf,
    /// Target OS slug used in artifact names: windows, linux, darwin.
    pub os: String,
    /// Target arch slug used in artifact names: x64, arm64.
    pub arch: String,
    /// `--check`: report only, never download or write.
    pub check_only: bool,
}

/// Result of an [`run_update`] run. Every outcome is explicit (no silent no-op).
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateReport {
    /// Provenance is package-manager-managed (or unknown → fail-closed): no
    /// self-replace performed. `hint` is an actionable next command.
    Refused {
        provenance: InstallProvenance,
        hint: String,
    },
    /// `--check` only, with package-manager-managed (or unknown) provenance:
    /// a read-only status query, so this is a *successful* answer ("this
    /// install is managed elsewhere"), not a failure — unlike bare `update`'s
    /// [`UpdateReport::Refused`], which is an action that did not happen.
    CheckRefused {
        provenance: InstallProvenance,
        current_version: String,
        hint: String,
    },
    /// Already on the latest version.
    UpToDate { version: String },
    /// `--check` only: a newer version is available but nothing was changed.
    UpdateAvailable { current: String, latest: String },
    /// The binary was replaced with `version`. `backup_removed` is false when the
    /// `.old` backup was left for later cleanup (Windows still-mapped image).
    Updated {
        version: String,
        backup_removed: bool,
    },
    /// The update attempt failed; the live binary is unchanged.
    Failed { stage: String, error: String },
}

/// Native redirect command for a package-manager-managed install.
fn redirect_hint(provenance: InstallProvenance) -> String {
    match provenance {
        InstallProvenance::Cargo => {
            "installed via cargo — run: cargo install --git https://github.com/monkey1wizard/ccync --force".to_string()
        }
        InstallProvenance::Homebrew => "installed via Homebrew — run: brew upgrade ccync".to_string(),
        InstallProvenance::Winget => {
            "installed via winget — run: winget upgrade Monkey1Wizard.ccync".to_string()
        }
        InstallProvenance::Unknown => {
            "unrecognized install location — update manually by re-running the install script".to_string()
        }
        InstallProvenance::SelfManaged => String::new(),
    }
}

/// Orchestrate `ccync update`: provenance gate → version check → (download +
/// verify + extract + swap). Network and the actual fetch are injected so the
/// gate and version logic are unit-testable without touching the network.
///
/// - Package-manager / unknown provenance → [`UpdateReport::Refused`] (fail-closed).
/// - Up to date → [`UpdateReport::UpToDate`].
/// - `check_only` with a newer release → [`UpdateReport::UpdateAvailable`] (no IO
///   beyond the version lookup).
/// - Otherwise download + verify + extract + swap → [`UpdateReport::Updated`].
pub fn run_update(
    opts: &UpdateOpts,
    latest_version: impl Fn() -> Result<String, String>,
    fetch_checksums: impl Fn() -> Result<String, String>,
    fetch_artifact: impl Fn() -> Result<Vec<u8>, String>,
) -> UpdateReport {
    let provenance = detect_provenance(&opts.exe_path);
    if provenance != InstallProvenance::SelfManaged {
        if opts.check_only {
            return UpdateReport::CheckRefused {
                provenance,
                current_version: opts.current_version.clone(),
                hint: redirect_hint(provenance),
            };
        }
        return UpdateReport::Refused {
            provenance,
            hint: redirect_hint(provenance),
        };
    }

    let latest = match latest_version() {
        Ok(v) => v,
        Err(e) => {
            return UpdateReport::Failed {
                stage: "latest-version".to_string(),
                error: e,
            }
        }
    };

    if !is_newer_version(&opts.current_version, &latest) {
        return UpdateReport::UpToDate {
            version: opts.current_version.clone(),
        };
    }

    if opts.check_only {
        return UpdateReport::UpdateAvailable {
            current: opts.current_version.clone(),
            latest,
        };
    }

    let artifact = artifact_name(&latest, &opts.os, &opts.arch);
    let staged = match fetch_verified_release(
        &opts.current_version,
        &latest,
        &artifact,
        fetch_checksums,
        fetch_artifact,
    ) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return UpdateReport::UpToDate {
                version: opts.current_version.clone(),
            }
        }
        Err(e) => {
            return UpdateReport::Failed {
                stage: "fetch".to_string(),
                error: e.to_string(),
            }
        }
    };

    // Extract into a staging subdir of the target's directory: same volume (so
    // the final rename is atomic) but a distinct path from the live binary (so
    // extraction never collides with `target`).
    let dir = opts.exe_path.parent().unwrap_or_else(|| Path::new("."));
    let stage_dir = dir.join(".ccync-update-staging");
    let is_zip = opts.os == "windows";
    let staged_path = match extract_binary(&staged.bytes, is_zip, &stage_dir) {
        Ok(p) => p,
        Err(e) => {
            return UpdateReport::Failed {
                stage: "extract".to_string(),
                error: e.to_string(),
            }
        }
    };

    match apply_staged(&staged_path, &opts.exe_path) {
        Ok(outcome) => UpdateReport::Updated {
            version: staged.version,
            backup_removed: outcome.backup_removed,
        },
        Err(e) => UpdateReport::Failed {
            stage: "swap".to_string(),
            error: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn posix_local_bin_is_self_managed() {
        assert_eq!(
            detect_provenance(&p("/home/alice/.local/bin/ccync")),
            InstallProvenance::SelfManaged
        );
    }

    #[test]
    fn windows_local_bin_is_self_managed() {
        assert_eq!(
            detect_provenance(&p(r"C:\Users\alice\.local\bin\ccync.exe")),
            InstallProvenance::SelfManaged
        );
    }

    #[test]
    fn cargo_bin_is_cargo() {
        assert_eq!(
            detect_provenance(&p("/home/alice/.cargo/bin/ccync")),
            InstallProvenance::Cargo
        );
        assert_eq!(
            detect_provenance(&p(r"C:\Users\alice\.cargo\bin\ccync.exe")),
            InstallProvenance::Cargo
        );
    }

    #[test]
    fn homebrew_cellar_is_homebrew() {
        assert_eq!(
            detect_provenance(&p("/usr/local/Cellar/ccync/1.2.3/bin/ccync")),
            InstallProvenance::Homebrew
        );
        assert_eq!(
            detect_provenance(&p("/opt/homebrew/Cellar/ccync/1.2.3/bin/ccync")),
            InstallProvenance::Homebrew
        );
    }

    #[test]
    fn linuxbrew_is_homebrew() {
        assert_eq!(
            detect_provenance(&p("/home/linuxbrew/.linuxbrew/bin/ccync")),
            InstallProvenance::Homebrew
        );
    }

    #[test]
    fn winget_path_is_winget() {
        assert_eq!(
            detect_provenance(&p(
                r"C:\Users\alice\AppData\Local\Microsoft\WinGet\Packages\ccync\ccync.exe"
            )),
            InstallProvenance::Winget
        );
    }

    #[test]
    fn unknown_path_is_unknown() {
        assert_eq!(
            detect_provenance(&p("/usr/local/bin/ccync")),
            InstallProvenance::Unknown
        );
        assert_eq!(
            detect_provenance(&p(r"C:\Program Files\ccync\ccync.exe")),
            InstallProvenance::Unknown
        );
    }

    #[test]
    fn cargo_takes_priority_over_local_bin() {
        // A path that contains both markers: cargo wins (most-specific first).
        assert_eq!(
            detect_provenance(&p("/home/alice/.cargo/bin/.local/bin/ccync")),
            InstallProvenance::Cargo
        );
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;

    /// SHA-256 of the bytes `b"hello"` (lowercase hex).
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn is_newer_version_basic() {
        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.0", "0.2.0"));
        assert!(is_newer_version("0.1.0", "1.0.0"));
        assert!(!is_newer_version("0.1.1", "0.1.1"));
        assert!(!is_newer_version("0.2.0", "0.1.9"));
    }

    #[test]
    fn is_newer_version_ignores_v_prefix_and_malformed() {
        assert!(is_newer_version("v0.1.0", "v0.1.1"));
        assert!(is_newer_version("0.1.0", "v0.1.1"));
        // malformed components compare as 0, never panic
        assert!(!is_newer_version("x.y.z", "x.y.z"));
        assert!(is_newer_version("0.1", "0.1.1"));
    }

    #[test]
    fn artifact_name_per_platform() {
        assert_eq!(
            artifact_name("v0.2.0", "windows", "x64"),
            "ccync-v0.2.0-windows-x64.zip"
        );
        assert_eq!(
            artifact_name("v0.2.0", "linux", "x64"),
            "ccync-v0.2.0-linux-x64.tar.gz"
        );
        assert_eq!(
            artifact_name("v0.2.0", "darwin", "arm64"),
            "ccync-v0.2.0-darwin-arm64.tar.gz"
        );
    }

    #[test]
    fn verify_sha256_match_and_mismatch() {
        assert!(verify_sha256(b"hello", HELLO_SHA).is_ok());
        // case-insensitive
        assert!(verify_sha256(b"hello", &HELLO_SHA.to_ascii_uppercase()).is_ok());
        let err = verify_sha256(b"hello", &"0".repeat(64));
        assert!(matches!(err, Err(UpdateError::ChecksumMismatch { .. })));
    }

    fn checksums_body() -> String {
        format!("{HELLO_SHA}  ccync-v0.2.0-linux-x64.tar.gz\n")
    }

    #[test]
    fn fetch_same_version_returns_none_without_network() {
        use std::cell::Cell;
        let checksums_called = Cell::new(false);
        let artifact_called = Cell::new(false);
        let out = fetch_verified_release(
            "0.2.0",
            "0.2.0",
            "ccync-v0.2.0-linux-x64.tar.gz",
            || {
                checksums_called.set(true);
                Ok(checksums_body())
            },
            || {
                artifact_called.set(true);
                Ok(b"hello".to_vec())
            },
        )
        .unwrap();
        assert!(out.is_none());
        assert!(
            !checksums_called.get() && !artifact_called.get(),
            "no network when up to date"
        );
    }

    #[test]
    fn fetch_newer_verified_returns_staged() {
        let staged = fetch_verified_release(
            "0.1.0",
            "v0.2.0",
            "ccync-v0.2.0-linux-x64.tar.gz",
            || Ok(checksums_body()),
            || Ok(b"hello".to_vec()),
        )
        .unwrap()
        .expect("newer version should stage");
        assert_eq!(staged.bytes, b"hello");
        assert_eq!(staged.version, "0.2.0", "leading v stripped");
    }

    #[test]
    fn fetch_checksum_mismatch_errs_no_staged() {
        let err = fetch_verified_release(
            "0.1.0",
            "0.2.0",
            "ccync-v0.2.0-linux-x64.tar.gz",
            || Ok(checksums_body()),
            || Ok(b"tampered".to_vec()),
        );
        assert!(matches!(err, Err(UpdateError::ChecksumMismatch { .. })));
    }

    #[test]
    fn fetch_missing_checksum_entry_errs() {
        let err = fetch_verified_release(
            "0.1.0",
            "0.2.0",
            "ccync-v0.2.0-windows-x64.zip", // not in the checksums body
            || Ok(checksums_body()),
            || Ok(b"hello".to_vec()),
        );
        assert!(matches!(err, Err(UpdateError::ChecksumMissing(_))));
    }

    #[test]
    fn fetch_network_failure_propagates() {
        let err = fetch_verified_release(
            "0.1.0",
            "0.2.0",
            "ccync-v0.2.0-linux-x64.tar.gz",
            || Err("offline".to_string()),
            || Ok(b"hello".to_vec()),
        );
        assert!(matches!(err, Err(UpdateError::Fetch(_))));
    }
}

#[cfg(test)]
mod swap_tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn tmp_dir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "ccync-swap-test-{tag}-{:?}",
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn backup_path_appends_old() {
        assert_eq!(
            backup_path(Path::new("/x/ccync")),
            PathBuf::from("/x/ccync.old")
        );
        assert_eq!(
            backup_path(Path::new(r"C:\x\ccync.exe")),
            PathBuf::from(r"C:\x\ccync.exe.old")
        );
    }

    #[test]
    fn apply_staged_replaces_existing_and_backs_up() {
        let d = tmp_dir("replace");
        let target = d.join("ccync");
        let staged = d.join("ccync.new");
        fs::write(&target, b"OLD").unwrap();
        fs::write(&staged, b"NEW").unwrap();

        let out = apply_staged(&staged, &target).unwrap();
        assert_eq!(
            fs::read(&target).unwrap(),
            b"NEW",
            "target now holds new bytes"
        );
        assert!(!staged.exists(), "staged moved into place");
        assert_eq!(out.backup, Some(backup_path(&target)));
        // backup either removed or left behind, but never silently lost while present
        if !out.backup_removed {
            assert!(backup_path(&target).exists());
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn apply_staged_fresh_install_no_backup() {
        let d = tmp_dir("fresh");
        let target = d.join("ccync");
        let staged = d.join("ccync.new");
        fs::write(&staged, b"NEW").unwrap();

        let out = apply_staged(&staged, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"NEW");
        assert_eq!(out.backup, None, "no prior binary → no backup");
    }

    #[test]
    fn apply_staged_rollback_on_failure() {
        let d = tmp_dir("rollback");
        let target = d.join("ccync");
        let staged = d.join("ccync.new");
        fs::write(&target, b"OLD").unwrap();
        // Do not write/create staged so that the rename fails.

        let err = apply_staged(&staged, &target);
        assert!(err.is_err(), "should fail because staged is missing");
        assert_eq!(
            fs::read(&target).unwrap(),
            b"OLD",
            "target should be rolled back to original"
        );
        assert!(
            !backup_path(&target).exists(),
            "backup file should be restored/removed"
        );
        let _ = fs::remove_dir_all(&d);
    }

    fn make_targz(entry_name: &str, body: &[u8]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, entry_name, body).unwrap();
        let tar_bytes = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    fn make_zip(entry_name: &str, body: &[u8]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file(entry_name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(body).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn extract_targz_finds_ccync() {
        let d = tmp_dir("targz");
        let arch = make_targz("ccync-v0.2.0-linux-x64/ccync", b"BINARY");
        let out = extract_binary(&arch, false, &d).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"BINARY");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&out).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0, "should be executable");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn extract_zip_finds_ccync_exe() {
        let d = tmp_dir("zip");
        let arch = make_zip("ccync.exe", b"WINBIN");
        let out = extract_binary(&arch, true, &d).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"WINBIN");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn extract_missing_binary_errs() {
        let d = tmp_dir("missing");
        let arch = make_targz("ccync-v0.2.0/README.md", b"docs");
        let err = extract_binary(&arch, false, &d);
        assert!(matches!(err, Err(UpdateError::Extract(_))));
        let _ = fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod update_tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::io::Write;

    fn opts(exe: PathBuf, current: &str, check_only: bool) -> UpdateOpts {
        UpdateOpts {
            current_version: current.to_string(),
            exe_path: exe,
            os: "linux".to_string(),
            arch: "x64".to_string(),
            check_only,
        }
    }

    /// Seams that panic if called — used to prove the gate short-circuits.
    fn no_net() -> impl Fn() -> Result<String, String> {
        || panic!("network must not be touched")
    }
    fn no_bytes() -> impl Fn() -> Result<Vec<u8>, String> {
        || panic!("artifact must not be downloaded")
    }

    #[test]
    fn managed_provenance_refuses_with_hint() {
        for (path, prov) in [
            ("/home/u/.cargo/bin/ccync", InstallProvenance::Cargo),
            (
                "/opt/homebrew/Cellar/ccync/1/bin/ccync",
                InstallProvenance::Homebrew,
            ),
            (
                r"C:\Users\u\AppData\Local\Microsoft\WinGet\ccync.exe",
                InstallProvenance::Winget,
            ),
            ("/usr/local/bin/ccync", InstallProvenance::Unknown),
        ] {
            let report = run_update(
                &opts(PathBuf::from(path), "0.1.0", false),
                || Ok("9.9.9".to_string()),
                no_net(),
                no_bytes(),
            );
            match report {
                UpdateReport::Refused { provenance, hint } => {
                    assert_eq!(provenance, prov);
                    assert!(!hint.is_empty(), "refusal must carry an actionable hint");
                }
                other => panic!("expected Refused for {path}, got {other:?}"),
            }
        }
    }

    #[test]
    fn check_only_managed_provenance_is_check_refused_not_refused() {
        // `update --check` on a
        // package-manager-managed install is a successful status query
        // (CheckRefused), not the fail-closed Refused that bare `update`
        // returns for the same provenance.
        for (path, prov) in [
            ("/home/u/.cargo/bin/ccync", InstallProvenance::Cargo),
            (
                "/opt/homebrew/Cellar/ccync/1/bin/ccync",
                InstallProvenance::Homebrew,
            ),
        ] {
            let report = run_update(
                &opts(PathBuf::from(path), "0.1.0", true),
                no_net(),
                no_net(),
                no_bytes(),
            );
            match report {
                UpdateReport::CheckRefused {
                    provenance,
                    current_version,
                    hint,
                } => {
                    assert_eq!(provenance, prov);
                    assert_eq!(current_version, "0.1.0");
                    assert!(
                        !hint.is_empty(),
                        "check-refusal must carry an actionable hint"
                    );
                }
                other => panic!("expected CheckRefused for {path}, got {other:?}"),
            }
        }
    }

    #[test]
    fn self_managed_up_to_date() {
        let report = run_update(
            &opts(PathBuf::from("/home/u/.local/bin/ccync"), "0.2.0", false),
            || Ok("0.2.0".to_string()),
            no_net(),
            no_bytes(),
        );
        assert_eq!(
            report,
            UpdateReport::UpToDate {
                version: "0.2.0".into()
            }
        );
    }

    #[test]
    fn check_only_reports_available_without_download() {
        let net_touched = Cell::new(false);
        let report = run_update(
            &opts(PathBuf::from("/home/u/.local/bin/ccync"), "0.1.0", true),
            || Ok("0.2.0".to_string()),
            || {
                net_touched.set(true);
                Ok(String::new())
            },
            || {
                net_touched.set(true);
                Ok(Vec::new())
            },
        );
        assert_eq!(
            report,
            UpdateReport::UpdateAvailable {
                current: "0.1.0".into(),
                latest: "0.2.0".into()
            }
        );
        assert!(!net_touched.get(), "--check must not download");
    }

    #[test]
    fn latest_version_lookup_failure_is_reported() {
        let report = run_update(
            &opts(PathBuf::from("/home/u/.local/bin/ccync"), "0.1.0", false),
            || Err("offline".to_string()),
            no_net(),
            no_bytes(),
        );
        assert!(matches!(report, UpdateReport::Failed { stage, .. } if stage == "latest-version"));
    }

    fn make_targz(entry: &str, body: &[u8]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, entry, body).unwrap();
        let bytes = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&bytes).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn full_self_update_swaps_binary() {
        // A SelfManaged exe path under a temp .local/bin.
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "ccync-upd-{:?}/.local/bin",
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("ccync");
        fs::write(&exe, b"OLD-BINARY").unwrap();

        let sha = {
            use sha2::{Digest, Sha256};
            let arch = make_targz("ccync-v0.2.0-linux-x64/ccync", b"NEW-BINARY");
            (arch.clone(), {
                let d = Sha256::digest(&arch);
                d.iter().map(|b| format!("{b:02x}")).collect::<String>()
            })
        };
        let (archive, archive_sha) = sha;
        let checksums = format!("{archive_sha}  ccync-0.2.0-linux-x64.tar.gz\n");

        let report = run_update(
            &opts(exe.clone(), "0.1.0", false),
            || Ok("0.2.0".to_string()),
            move || Ok(checksums.clone()),
            move || Ok(archive.clone()),
        );

        assert!(
            matches!(report, UpdateReport::Updated { ref version, .. } if version == "0.2.0"),
            "got {report:?}"
        );
        assert_eq!(
            fs::read(&exe).unwrap(),
            b"NEW-BINARY",
            "live binary replaced"
        );
        let _ = fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }
}
