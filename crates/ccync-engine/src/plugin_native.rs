//! Identity binding, observation, and reconciliation for native plugin loaders.

use ccync_foundation::provider::{
    CapabilityInventory, CommittedOwnership, EnablementIntent, NativeObservation, NativeSelector,
    PackageDigest, PendingIntent, ProviderGeneration,
};
use ccync_foundation::target::ProjectionTargetId;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Maximum encoded size accepted for one native observation.
pub const MAX_NATIVE_OBSERVATION_BYTES: usize = 64 * 1024;

/// Wall-clock bound for one native CLI invocation. A child still running at
/// the bound is killed and reported as a failure.
pub const NATIVE_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Stderr kept from a failing invocation. Longer diagnostics are dropped.
const MAX_NATIVE_STDERR_BYTES: usize = 2048;

/// Production adapter that invokes the executable selected by the native
/// target binding. The subprocess output is bounded before it is parsed.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessNativeRunner;

impl ProcessNativeRunner {
    /// Read and judge the CLI version of `executable` through the bounded
    /// runner. The verdict is cached per executable for the process lifetime.
    fn adapter_version_verdict(&self, executable: NativeExecutable) -> AdapterVersionVerdict {
        static CACHE: OnceLock<Mutex<HashMap<NativeExecutable, AdapterVersionVerdict>>> =
            OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Ok(map) = cache.lock() {
            if let Some(verdict) = map.get(&executable) {
                return verdict.clone();
            }
        }
        let verdict = match self.run(executable, &["--version"]) {
            Ok(bytes) => judge_adapter_version(executable, &bytes),
            Err(_) => AdapterVersionVerdict::Unreadable,
        };
        if let Ok(mut map) = cache.lock() {
            map.insert(executable, verdict.clone());
        }
        verdict
    }
}

impl NativeCommandRunner for ProcessNativeRunner {
    fn mutation_verified(&self, executable: NativeExecutable) -> bool {
        self.adapter_version_verdict(executable) == AdapterVersionVerdict::Verified
    }

    fn mutation_block_reason(&self, executable: NativeExecutable) -> String {
        self.adapter_version_verdict(executable).reason()
    }

    fn run(&self, executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
        let label = format!("{} {:?}", executable.command(), args);
        let mut child = Command::new(resolve_executable(executable.command()))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{label}: could not start executable: {error}"))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let stdout_reader =
            std::thread::spawn(move || read_bounded(stdout, MAX_NATIVE_OBSERVATION_BYTES));
        let stderr_reader =
            std::thread::spawn(move || read_bounded(stderr, MAX_NATIVE_STDERR_BYTES));

        let deadline = Instant::now() + NATIVE_COMMAND_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{label}: exceeded the {NATIVE_COMMAND_TIMEOUT:?} time bound"
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{label}: could not wait for executable: {error}"));
                }
            }
        };
        let stdout = stdout_reader.join().ok().flatten().ok_or_else(|| {
            format!("{label}: stdout exceeded {MAX_NATIVE_OBSERVATION_BYTES} bytes")
        })?;
        if !status.success() {
            let stderr = stderr_reader.join().ok().flatten().unwrap_or_default();
            return Err(format!(
                "{label}: exited with {status}: {}",
                String::from_utf8_lossy(&stderr)
            ));
        }
        Ok(stdout)
    }

    fn observe(
        &self,
        executable: NativeExecutable,
        selector: &NativeSelector,
    ) -> Result<NativeRegistration, String> {
        let args: &[&str] = match executable {
            NativeExecutable::Codex => &["plugin", "list", "--json"],
            NativeExecutable::Claude => &["plugin", "list", "--json"],
        };
        let bytes = self.run(executable, args)?;
        parse_cli_registration(executable, &bytes, selector)
    }

    fn marketplace_root(
        &self,
        executable: NativeExecutable,
        marketplace: &str,
    ) -> Result<Option<String>, String> {
        let bytes = self.run(executable, &["plugin", "marketplace", "list", "--json"])?;
        parse_marketplace_root(executable, &bytes, marketplace)
    }
}

/// Read the root that `marketplace` resolves to in a native marketplace listing.
///
/// Codex reports the root as `path` or `root`. Claude Code reports `path` for a
/// directory source and `installLocation` for any source. A non-directory source
/// still yields its root, so the caller reports a conflict instead of adding over it.
fn parse_marketplace_root(
    executable: NativeExecutable,
    bytes: &[u8],
    marketplace: &str,
) -> Result<Option<String>, String> {
    let name = executable.command();
    if bytes.len() > MAX_NATIVE_OBSERVATION_BYTES {
        return Err(format!("{name} marketplace list exceeded output bound"));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| format!("{name} marketplace list returned malformed JSON"))?;
    let entries = value
        .as_array()
        .or_else(|| value.get("marketplaces").and_then(Value::as_array))
        .ok_or_else(|| format!("{name} marketplace list has an unsupported shape"))?;
    let root_keys: &[&str] = match executable {
        NativeExecutable::Codex => &["path", "root"],
        NativeExecutable::Claude => &["path", "installLocation"],
    };
    Ok(entries
        .iter()
        .find(|item| item.get("name").and_then(Value::as_str) == Some(marketplace))
        .and_then(|item| {
            root_keys
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_str))
        })
        .map(str::to_owned))
}

/// Find a file on `PATH` that `Command` can start. An npm install on Windows
/// puts an extensionless shell script beside `codex.cmd`, and only the `.cmd`
/// shim starts, so the search tries each runnable suffix in turn. A name that
/// is not found is returned unchanged, so the spawn reports it as missing.
fn resolve_executable(name: &str) -> PathBuf {
    let suffixes: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd"]
    } else {
        &[""]
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .flat_map(|dir| {
            suffixes
                .iter()
                .map(move |suffix| dir.join(format!("{name}{suffix}")))
        })
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Read up to `limit` bytes of a child's stream. The rest is drained so that a
/// chatty child never blocks on a full pipe. Returns `None` past the limit.
fn read_bounded(mut reader: impl Read, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let read = (&mut reader).take(limit as u64 + 1).read_to_end(&mut bytes);
    let drained = std::io::copy(&mut reader, &mut std::io::sink());
    (read.is_ok() && drained.is_ok() && bytes.len() <= limit).then_some(bytes)
}

/// The selector that a native listing entry names, under whichever key it uses.
fn entry_selector(item: &Value) -> Option<&str> {
    item.get("pluginId")
        .or_else(|| item.get("id"))
        .or_else(|| item.get("selector"))
        .or_else(|| item.get("name"))
        .and_then(Value::as_str)
}

fn parse_cli_registration(
    executable: NativeExecutable,
    bytes: &[u8],
    selector: &NativeSelector,
) -> Result<NativeRegistration, String> {
    if bytes.len() > MAX_NATIVE_OBSERVATION_BYTES {
        return Err("native plugin list exceeded output bound".into());
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| "native plugin list returned malformed JSON".to_string())?;
    // Codex lists installed and available entries under separate keys. Only
    // the installed list can name a registration, so an available entry is
    // never observed as present or enabled.
    let entries = match executable {
        NativeExecutable::Codex => value.get("installed").and_then(Value::as_array),
        NativeExecutable::Claude => value
            .as_array()
            .or_else(|| value.get("plugins").and_then(Value::as_array)),
    }
    .ok_or_else(|| "native plugin list has an unsupported shape".to_string())?;
    let matched: Vec<&Value> = entries
        .iter()
        .filter(|item| entry_selector(item) == Some(selector.as_str()))
        .collect();
    // Claude Code lists a plugin once per scope. Only its user-scope entry is
    // the registration that ccync manages. The same selector under another
    // scope is foreign: it is reported as present, without a selector, so that
    // no mutation can treat it as owned.
    let user_scope = |item: &&Value| {
        executable != NativeExecutable::Claude
            || item.get("scope").and_then(Value::as_str) == Some("user")
    };
    let Some(item) = matched.iter().copied().find(user_scope) else {
        if let Some(foreign) = matched.first() {
            return Ok(NativeRegistration {
                available: true,
                present: true,
                enabled: foreign
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                broken: false,
                selector: None,
                source: None,
                upstream_revision: None,
                package_digest: None,
                capabilities: CapabilityInventory::new(),
            });
        }
        return Ok(NativeRegistration {
            available: true,
            present: false,
            enabled: true,
            broken: false,
            selector: None,
            source: None,
            upstream_revision: None,
            package_digest: None,
            capabilities: CapabilityInventory::new(),
        });
    };
    let mut normalized = item.clone();
    if let Some(object) = normalized.as_object_mut() {
        if executable == NativeExecutable::Codex
            && object.get("source").is_some_and(Value::is_object)
        {
            // Codex reports `source` as an object that names the local
            // marketplace path. It is not a ccync source key, so it is left out
            // and the caller joins it to the owned root instead.
            object.remove("source");
        }
        object
            .entry("selector")
            .or_insert_with(|| Value::String(selector.as_str().to_owned()));
        object.entry("available").or_insert(Value::Bool(true));
        object.entry("present").or_insert(Value::Bool(true));
    }
    NativeRegistration::parse_bounded(&serde_json::to_vec(&normalized).map_err(|e| e.to_string())?)
        .map_err(|_| {
            "native plugin registration is unreadable or lacks ownership evidence".to_string()
        })
}

/// Injectable boundary for native CLI operations. Implementations must return
/// the native manager's current state for `observe`; tests can provide a fake
/// without invoking either external CLI.
pub trait NativeCommandRunner {
    /// Whether the adapter for `executable` may mutate native registrations.
    /// Each adapter is verified separately by its live loader probe, and every
    /// install, update, and removal checks this before its first CLI call.
    /// The default fails closed: a runner must opt in explicitly.
    fn mutation_verified(&self, _executable: NativeExecutable) -> bool {
        false
    }
    /// Reason an adapter is not eligible to mutate, naming the observed CLI
    /// version when the runner can read one.
    fn mutation_block_reason(&self, executable: NativeExecutable) -> String {
        format!("{} has no verified loader probe", executable.command())
    }
    /// Run one bound executable with arguments and return stdout on success.
    fn run(&self, executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String>;
    /// Read the current registration and marketplace state for a selector.
    fn observe(
        &self,
        executable: NativeExecutable,
        selector: &NativeSelector,
    ) -> Result<NativeRegistration, String>;
    /// Verify that the marketplace name resolves to the exact ccync-owned
    /// root. A name-only match is insufficient ownership evidence.
    fn marketplace_root(
        &self,
        executable: NativeExecutable,
        marketplace: &str,
    ) -> Result<Option<String>, String>;
}

/// Remove a registration only when fresh path evidence proves ownership.
///
/// The native listings report no package digest, so ownership is proven by
/// hashing the installed view directory that the committed record names. The
/// view digest must match exactly. The native fields that are reported must
/// agree with the record, and a registration that is present but broken is
/// refused. A disabled owned registration is removable. The caller owns the
/// lifecycle lock and retains its records on any failure.
pub fn remove_committed_registration(
    runner: &dyn NativeCommandRunner,
    executable: NativeExecutable,
    ownership: &CommittedOwnership,
) -> Result<(), NativeReconcileError> {
    if !runner.mutation_verified(executable) {
        return Err(NativeReconcileError::OwnershipEvidenceUnverified(
            executable,
        ));
    }
    let observed = runner
        .observe(executable, &ownership.selector)
        .map_err(NativeReconcileError::Runner)?;
    if !observed.available {
        return Err(NativeReconcileError::Conflict(
            NativeObservation::Unavailable,
        ));
    }
    if !observed.present {
        return Ok(());
    }
    if ownership.client_scope.is_none() || ownership.source_digest.is_none() {
        return Err(NativeReconcileError::Conflict(NativeObservation::Drifted));
    }
    if !owned_registration_matches(&observed, ownership) {
        return Err(NativeReconcileError::Conflict(NativeObservation::Drifted));
    }
    let Some(view_digest) = ownership.view_digest.as_ref() else {
        return Err(NativeReconcileError::Conflict(NativeObservation::Drifted));
    };
    let Some(view_path) = committed_view_path(ownership) else {
        return Err(NativeReconcileError::Conflict(NativeObservation::Drifted));
    };
    match observe_view_digest(&view_path) {
        NativePathEvidence::Digest(digest) if digest == *view_digest => {}
        NativePathEvidence::Unreadable => {
            return Err(NativeReconcileError::Conflict(
                NativeObservation::Unreadable,
            ));
        }
        NativePathEvidence::Missing | NativePathEvidence::Digest(_) => {
            return Err(NativeReconcileError::Conflict(NativeObservation::Drifted));
        }
    }
    // A registration the user changed after the first observation must not be
    // removed. The proof is taken again immediately before the mutation.
    let confirmed = runner
        .observe(executable, &ownership.selector)
        .map_err(NativeReconcileError::Runner)?;
    if !confirmed.available {
        return Err(NativeReconcileError::Conflict(
            NativeObservation::Unavailable,
        ));
    }
    if !confirmed.present {
        return Ok(());
    }
    if !owned_registration_matches(&confirmed, ownership) {
        return Err(NativeReconcileError::Conflict(NativeObservation::Drifted));
    }
    let args: &[&str] = match executable {
        NativeExecutable::Codex => &["plugin", "remove", ownership.selector.as_str()],
        NativeExecutable::Claude => &[
            "plugin",
            "uninstall",
            ownership.selector.as_str(),
            "--scope",
            "user",
        ],
    };
    runner
        .run(executable, args)
        .map_err(NativeReconcileError::Runner)?;
    let after = runner
        .observe(executable, &ownership.selector)
        .map_err(NativeReconcileError::Runner)?;
    if after.available && !after.present {
        Ok(())
    } else {
        Err(NativeReconcileError::Verification(
            NativeObservation::Drifted,
        ))
    }
}

/// Whether an observed registration is the committed one: same selector, not
/// broken, and native fields that agree with the committed generation.
fn owned_registration_matches(
    observed: &NativeRegistration,
    ownership: &CommittedOwnership,
) -> bool {
    observed.selector.as_ref() == Some(&ownership.selector)
        && !observed.broken
        && observed.native_fields_agree(
            &ownership.generation,
            ownership.upstream_revision.as_deref(),
            &ownership.capabilities,
        )
}

/// Desired native registration for one first install or update.
#[derive(Debug, Clone, Copy)]
pub struct NativeInstall<'a> {
    /// Exact `PLUGIN@MARKETPLACE` selector to install.
    pub selector: &'a NativeSelector,
    /// Absolute local root of the ccync-owned marketplace.
    pub marketplace_root: &'a str,
    /// Generation the installed package must match.
    pub generation: &'a ProviderGeneration,
    /// Upstream revision the registration must report, when the CLI exposes it.
    pub upstream_revision: Option<&'a str>,
    /// Capabilities the loader must report.
    pub capabilities: &'a CapabilityInventory,
}

/// Install `install.selector` from the owned local marketplace.
///
/// A present registration is never replaced here. A foreign or disabled one
/// is reported as a conflict before any CLI call. The caller keeps its pending
/// intent on every error, because a failure after `plugin add` may have
/// changed native state.
pub fn install_native_registration(
    runner: &dyn NativeCommandRunner,
    executable: NativeExecutable,
    install: &NativeInstall<'_>,
) -> Result<(), NativeReconcileError> {
    if !runner.mutation_verified(executable) {
        return Err(NativeReconcileError::OwnershipEvidenceUnverified(
            executable,
        ));
    }
    let (_, marketplace) = install
        .selector
        .as_str()
        .split_once('@')
        .ok_or(NativeReconcileError::MarketplaceRootMissing)?;
    let before = runner
        .observe(executable, install.selector)
        .map_err(NativeReconcileError::Runner)?;
    if !before.available {
        return Err(NativeReconcileError::Conflict(
            NativeObservation::Unavailable,
        ));
    }
    if before.present {
        let state = if before.enabled {
            NativeObservation::External
        } else {
            NativeObservation::Disabled
        };
        return Err(NativeReconcileError::Conflict(state));
    }
    ensure_marketplace(runner, executable, marketplace, install.marketplace_root)?;
    let add: &[&str] = match executable {
        NativeExecutable::Codex => &["plugin", "add", install.selector.as_str()],
        NativeExecutable::Claude => &[
            "plugin",
            "install",
            install.selector.as_str(),
            "--scope",
            "user",
        ],
    };
    runner
        .run(executable, add)
        .map_err(NativeReconcileError::Runner)?;
    let after = runner
        .observe(executable, install.selector)
        .map_err(NativeReconcileError::Runner)?;
    let verified = after.available
        && after.present
        && after.enabled
        && !after.broken
        && after.selector.as_ref() == Some(install.selector)
        && after.native_fields_agree(
            install.generation,
            install.upstream_revision,
            install.capabilities,
        );
    if verified {
        return Ok(());
    }
    let state = if !after.available {
        NativeObservation::Unavailable
    } else if !after.present {
        NativeObservation::Absent
    } else if !after.enabled {
        NativeObservation::Disabled
    } else {
        NativeObservation::Drifted
    };
    Err(NativeReconcileError::Verification(state))
}

/// Replace a committed registration with `install`. The old registration is
/// removed only when it still matches the committed ownership exactly. If the
/// install then fails, no registration remains, and the caller's intent is
/// what allows recovery.
pub fn update_native_registration(
    runner: &dyn NativeCommandRunner,
    executable: NativeExecutable,
    committed: &CommittedOwnership,
    install: &NativeInstall<'_>,
) -> Result<(), NativeReconcileError> {
    remove_committed_registration(runner, executable, committed)?;
    install_native_registration(runner, executable, install)
}

/// Switch an owned registration on or off in the user scope.
///
/// The observed registration must still name the committed selector and agree
/// with its generation. A foreign, project-scoped, or otherwise unowned entry is
/// refused before any CLI call. A registration already in the requested state
/// is left alone, so a repeated call makes no mutation.
pub fn set_native_enablement(
    runner: &dyn NativeCommandRunner,
    executable: NativeExecutable,
    committed: &CommittedOwnership,
    enabled: bool,
) -> Result<(), NativeReconcileError> {
    if !runner.mutation_verified(executable) {
        return Err(NativeReconcileError::OwnershipEvidenceUnverified(
            executable,
        ));
    }
    if executable != NativeExecutable::Claude {
        return Err(NativeReconcileError::EnablementUnsupported(executable));
    }
    let selector = &committed.selector;
    let observed = runner
        .observe(executable, selector)
        .map_err(NativeReconcileError::Runner)?;
    if !observed.available {
        return Err(NativeReconcileError::Conflict(
            NativeObservation::Unavailable,
        ));
    }
    if !observed.present {
        return Err(NativeReconcileError::Conflict(NativeObservation::Absent));
    }
    let owned = observed.selector.as_ref() == Some(selector)
        && !observed.broken
        && observed.native_fields_agree(
            &committed.generation,
            committed.upstream_revision.as_deref(),
            &committed.capabilities,
        );
    if !owned {
        return Err(NativeReconcileError::Conflict(NativeObservation::Drifted));
    }
    if observed.enabled == enabled {
        return Ok(());
    }
    let verb = if enabled { "enable" } else { "disable" };
    runner
        .run(
            executable,
            &["plugin", verb, selector.as_str(), "--scope", "user"],
        )
        .map_err(NativeReconcileError::Runner)?;
    let after = runner
        .observe(executable, selector)
        .map_err(NativeReconcileError::Runner)?;
    if after.present && after.selector.as_ref() == Some(selector) && after.enabled == enabled {
        Ok(())
    } else {
        Err(NativeReconcileError::Verification(
            NativeObservation::Drifted,
        ))
    }
}

/// Point the owned marketplace name at `root`. An existing name that resolves
/// elsewhere is a conflict and is never rewritten. A missing name is
/// registered from `root`, then must resolve to `root`.
fn ensure_marketplace(
    runner: &dyn NativeCommandRunner,
    executable: NativeExecutable,
    marketplace: &str,
    root: &str,
) -> Result<(), NativeReconcileError> {
    match runner
        .marketplace_root(executable, marketplace)
        .map_err(NativeReconcileError::Runner)?
    {
        Some(current) if same_root(&current, root) => return Ok(()),
        Some(_) => return Err(NativeReconcileError::MarketplaceConflict),
        None => {}
    }
    let add: &[&str] = match executable {
        NativeExecutable::Codex => &["plugin", "marketplace", "add", root],
        NativeExecutable::Claude => &["plugin", "marketplace", "add", root, "--scope", "user"],
    };
    runner
        .run(executable, add)
        .map_err(NativeReconcileError::Runner)?;
    match runner
        .marketplace_root(executable, marketplace)
        .map_err(NativeReconcileError::Runner)?
    {
        Some(current) if same_root(&current, root) => Ok(()),
        _ => Err(NativeReconcileError::MarketplaceConflict),
    }
}

/// Whether two marketplace roots name the same location. Path components are
/// compared, so `C:/x` and `C:\x` match. A different root never matches.
fn same_root(current: &str, root: &str) -> bool {
    Path::new(current) == Path::new(root)
}

/// Reconciliation failed or stopped without a verified desired registration.
#[derive(Debug)]
pub enum NativeReconcileError {
    /// Injected CLI operation failed.
    Runner(String),
    /// The marketplace name is not proven to point at the owned root.
    MarketplaceConflict,
    /// Codex marketplace parameters were incomplete.
    MarketplaceRootMissing,
    /// Existing state is foreign, disabled, broken, or otherwise uncertain.
    Conflict(NativeObservation),
    /// CLI returned but reread did not verify the desired generation.
    Verification(NativeObservation),
    /// The adapter has not passed its live loader probe, so no mutation was tried.
    OwnershipEvidenceUnverified(NativeExecutable),
    /// The adapter exposes no native enablement command, so none was tried.
    EnablementUnsupported(NativeExecutable),
}

impl std::fmt::Display for NativeReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runner(error) => write!(f, "native CLI failed: {error}"),
            Self::MarketplaceConflict => f.write_str("native marketplace ownership conflict"),
            Self::MarketplaceRootMissing => f.write_str("native marketplace root is missing"),
            Self::Conflict(state) => write!(f, "native registration requires recovery: {state:?}"),
            Self::Verification(state) => write!(f, "native registration verification failed: {state:?}"),
            Self::OwnershipEvidenceUnverified(executable) => write!(
                f,
                "{} native loading not run: its CLI output is not verified to carry ccync ownership evidence",
                executable.command()
            ),
            Self::EnablementUnsupported(executable) => write!(
                f,
                "{} native enablement is not exposed by this adapter",
                executable.command()
            ),
        }
    }
}

impl std::error::Error for NativeReconcileError {}

/// Recovery decision for an interrupted native registration.
///
/// A retry must inspect the selector again before choosing a mutation. In
/// particular, an unknown or foreign registration is never treated as an
/// invitation to replay an install or remove command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeRecoveryAction {
    /// The desired registration is already present and should be committed.
    CommitObserved,
    /// The selector is absent, so a fresh registration may be attempted.
    Register,
    /// A prior owned registration remains and the desired generation can be
    /// installed as an upgrade while retaining the prior generation.
    UpgradeOwned,
    /// The observed state does not prove it is safe to mutate.
    PreservePartial,
}

/// Choose a retry action from current observation and recorded ownership.
///
/// This is deliberately a pure gate used after a durable intent has been
/// loaded. It prevents recovery from blindly repeating a partially completed
/// CLI operation.
pub fn recovery_action(
    state: NativeObservation,
    intent_generation: &ProviderGeneration,
    observed_generation: Option<&ProviderGeneration>,
) -> NativeRecoveryAction {
    match state {
        NativeObservation::Desired if observed_generation == Some(intent_generation) => {
            NativeRecoveryAction::CommitObserved
        }
        NativeObservation::Absent => NativeRecoveryAction::Register,
        NativeObservation::OwnedOld
            if observed_generation.is_some() && observed_generation != Some(intent_generation) =>
        {
            NativeRecoveryAction::UpgradeOwned
        }
        NativeObservation::Desired
        | NativeObservation::Unavailable
        | NativeObservation::External
        | NativeObservation::Disabled
        | NativeObservation::Broken
        | NativeObservation::Unreadable
        | NativeObservation::Drifted
        | NativeObservation::OwnedOld => NativeRecoveryAction::PreservePartial,
    }
}

/// Native executable family and its canonical command identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeExecutable {
    /// OpenAI Codex CLI.
    Codex,
    /// Anthropic Claude Code CLI.
    Claude,
}

/// Codex CLI `major.minor` line whose live loader probe verified native
/// package identity, enabled count 1, and disabled count 0 in an isolated
/// profile. Other lines are not eligible to mutate native registrations.
pub const CODEX_VERIFIED_VERSION_LINE: &str = "0.169";

/// Claude Code CLI `major.minor` line whose live loader probe verified native
/// package identity, enabled count 1, and disabled count 0 in an isolated
/// profile. Other lines are not eligible to mutate native registrations.
pub const CLAUDE_VERIFIED_VERSION_LINE: &str = "2.1";

/// Outcome of reading one adapter's CLI version against its verified line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterVersionVerdict {
    /// The version is on the line that the live probe verified.
    Verified,
    /// The CLI reported a version outside the verified line.
    Unverified(String),
    /// The version could not be read or did not name the adapter.
    Unreadable,
}

impl AdapterVersionVerdict {
    /// Reason text for a verdict that does not permit mutation.
    pub fn reason(&self) -> String {
        match self {
            Self::Verified => "adapter version verified".to_string(),
            Self::Unverified(observed) => format!("unverified adapter version {observed}"),
            Self::Unreadable => "unreadable adapter version".to_string(),
        }
    }
}

/// Judge the output of `<cli> --version` for `executable`. Codex prints
/// `codex-cli <version>` and Claude Code prints `<version> (Claude Code)`.
pub fn judge_adapter_version(executable: NativeExecutable, output: &[u8]) -> AdapterVersionVerdict {
    let Ok(text) = std::str::from_utf8(output) else {
        return AdapterVersionVerdict::Unreadable;
    };
    let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return AdapterVersionVerdict::Unreadable;
    };
    let token = match executable {
        NativeExecutable::Codex => line
            .strip_prefix("codex-cli ")
            .and_then(|rest| rest.split_whitespace().next()),
        NativeExecutable::Claude => line
            .contains("(Claude Code)")
            .then(|| line.split_whitespace().next())
            .flatten(),
    };
    let Some(token) = token else {
        return AdapterVersionVerdict::Unreadable;
    };
    let mut parts = token.splitn(3, '.');
    let digits = |part: Option<&str>| {
        part.filter(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_string)
    };
    let (Some(major), Some(minor), true) = (
        digits(parts.next()),
        digits(parts.next()),
        parts
            .next()
            .is_some_and(|patch| patch.starts_with(|c: char| c.is_ascii_digit())),
    ) else {
        return AdapterVersionVerdict::Unreadable;
    };
    if format!("{major}.{minor}") == executable.verified_version_line() {
        AdapterVersionVerdict::Verified
    } else {
        AdapterVersionVerdict::Unverified(token.to_string())
    }
}

impl NativeExecutable {
    /// Every native adapter, in a fixed order.
    pub const ALL: [Self; 2] = [Self::Codex, Self::Claude];

    /// The `major.minor` CLI version line that this adapter's live loader
    /// probe verified.
    pub const fn verified_version_line(self) -> &'static str {
        match self {
            Self::Codex => CODEX_VERIFIED_VERSION_LINE,
            Self::Claude => CLAUDE_VERIFIED_VERSION_LINE,
        }
    }

    /// Bind a provider projection target to its native executable.
    pub fn for_target(target: ProjectionTargetId) -> Option<Self> {
        match target {
            ProjectionTargetId::Codex => Some(Self::Codex),
            ProjectionTargetId::Claude => Some(Self::Claude),
            _ => None,
        }
    }

    /// Canonical executable name used by the adapter.
    pub const fn command(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

/// Bounded, adapter-neutral observation of one native selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRegistration {
    /// Whether the target CLI and observation mechanism are available.
    pub available: bool,
    /// Whether a registration or path occupies the selector.
    pub present: bool,
    /// Native registration enablement, independent of global feature flags.
    pub enabled: bool,
    /// Whether the native loader reports a load failure.
    pub broken: bool,
    /// Exact selector observed from the native manager.
    pub selector: Option<NativeSelector>,
    /// Source key represented by the installed registration, when exposed.
    pub source: Option<String>,
    /// Upstream revision represented by the registration, when exposed.
    pub upstream_revision: Option<String>,
    /// Installed package digest, when exposed.
    pub package_digest: Option<String>,
    /// Capabilities reported by the loader.
    pub capabilities: CapabilityInventory,
}

impl NativeRegistration {
    /// Parse a JSON observation with a strict input-size bound. Malformed or
    /// oversized observations classify as unreadable rather than absent.
    pub fn parse_bounded(input: &[u8]) -> Result<Self, NativeParseError> {
        if input.len() > MAX_NATIVE_OBSERVATION_BYTES {
            return Err(NativeParseError::TooLarge);
        }
        let value: Value =
            serde_json::from_slice(input).map_err(|_| NativeParseError::Malformed)?;
        let object = value.as_object().ok_or(NativeParseError::Malformed)?;
        let string = |key: &str| -> Result<Option<String>, NativeParseError> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value)) if value.len() <= 4096 => Ok(Some(value.clone())),
                _ => Err(NativeParseError::Malformed),
            }
        };
        let boolean = |key: &str, default: bool| -> Result<bool, NativeParseError> {
            match object.get(key) {
                None => Ok(default),
                Some(Value::Bool(value)) => Ok(*value),
                _ => Err(NativeParseError::Malformed),
            }
        };
        let selector = string("selector")?
            .map(NativeSelector::new)
            .transpose()
            .map_err(|_| NativeParseError::Malformed)?;
        let capabilities = match object.get("capabilities") {
            None => CapabilityInventory::new(),
            Some(Value::Array(values)) if values.len() <= 64 => {
                let mut result = CapabilityInventory::new();
                for value in values {
                    let name = value.as_str().ok_or(NativeParseError::Malformed)?;
                    let capability = match name {
                        "skill" => ccync_foundation::provider::ComponentKind::Skill,
                        "command" => ccync_foundation::provider::ComponentKind::Command,
                        "agent" => ccync_foundation::provider::ComponentKind::Agent,
                        "hook" => ccync_foundation::provider::ComponentKind::Hook,
                        "resource" => ccync_foundation::provider::ComponentKind::Resource,
                        _ => return Err(NativeParseError::Malformed),
                    };
                    result.insert(capability);
                }
                result
            }
            _ => return Err(NativeParseError::Malformed),
        };
        // Missing `available` and `enabled` fields fail closed. Neither an
        // absent field nor an unknown one may imply that the loader is reachable
        // or that the registration runs.
        Ok(Self {
            available: boolean("available", false)?,
            present: boolean("present", false)?,
            enabled: boolean("enabled", false)?,
            broken: boolean("broken", false)?,
            selector,
            source: string("source")?,
            upstream_revision: string("upstreamRevision")?,
            package_digest: string("packageDigest")?,
            capabilities,
        })
    }

    /// Classify this observation against committed ownership and a desired
    /// generation. A present unowned registration is always external.
    ///
    /// Ownership comes from the committed record and the fresh `path` evidence.
    /// Native-reported digests are optional cross-checks. When the CLI reports
    /// one that disagrees, the registration has drifted.
    pub fn classify(
        &self,
        target: ProjectionTargetId,
        committed: Option<&CommittedOwnership>,
        desired: &ProviderGeneration,
        desired_revision: Option<&str>,
        required: &CapabilityInventory,
        path: &NativePathEvidence,
    ) -> NativeObservation {
        if !self.available {
            return NativeObservation::Unavailable;
        }
        if !self.present {
            return NativeObservation::Absent;
        }
        if self.broken {
            return NativeObservation::Broken;
        }
        let Some(selector) = self.selector.as_ref() else {
            return NativeObservation::Unreadable;
        };
        let Some(committed) = committed else {
            return NativeObservation::External;
        };
        if committed.selector != *selector || committed.target != target {
            return NativeObservation::External;
        }
        if !self.enabled {
            return NativeObservation::Disabled;
        }
        if !self.native_fields_agree(
            &committed.generation,
            committed.upstream_revision.as_deref(),
            required,
        ) {
            return NativeObservation::Drifted;
        }
        let digest = match path {
            NativePathEvidence::Unreadable => return NativeObservation::Unreadable,
            NativePathEvidence::Missing => return NativeObservation::Drifted,
            NativePathEvidence::Digest(digest)
                if *digest != committed.generation.package_digest =>
            {
                return NativeObservation::Drifted;
            }
            NativePathEvidence::Digest(digest) => digest,
        };
        if committed.proves(target, selector, desired, desired_revision, required)
            && *digest == desired.package_digest
        {
            NativeObservation::Desired
        } else {
            NativeObservation::OwnedOld
        }
    }

    /// Classify this observation for a first install, which has no committed
    /// row. The registration is owned only when a pending intent names this
    /// exact selector and desired generation, carries the full proof fields,
    /// and fresh `path` evidence matches the desired content.
    ///
    /// A same-name registration without a matching intent is external. Any
    /// content that changes after the intent was recorded is drifted.
    pub fn classify_pending(
        &self,
        target: ProjectionTargetId,
        intent: &PendingIntent,
        desired: &ProviderGeneration,
        desired_revision: Option<&str>,
        required: &CapabilityInventory,
        path: &NativePathEvidence,
    ) -> NativeObservation {
        self.classify_pending_digest(
            target,
            intent,
            desired,
            desired_revision,
            required,
            path,
            &desired.package_digest,
        )
    }

    /// Classify a first-install observation whose content proof is the view
    /// digest the pending intent recorded. `view` must be the digest of the
    /// installed view directory, read by [`observe_view_digest`] over that
    /// directory. The same digest function produced both sides, so a match
    /// proves the bytes ccync published are the bytes the registration loads.
    pub fn classify_adopted_view(
        &self,
        target: ProjectionTargetId,
        intent: &PendingIntent,
        desired: &ProviderGeneration,
        desired_revision: Option<&str>,
        required: &CapabilityInventory,
        view: &NativePathEvidence,
    ) -> NativeObservation {
        let Some(expected) = intent.view_digest.as_ref() else {
            return NativeObservation::External;
        };
        self.classify_pending_digest(
            target,
            intent,
            desired,
            desired_revision,
            required,
            view,
            expected,
        )
    }

    /// Shared body of the pending-intent classifications. `expected` is the
    /// digest the observed content must equal.
    #[allow(clippy::too_many_arguments)]
    fn classify_pending_digest(
        &self,
        target: ProjectionTargetId,
        intent: &PendingIntent,
        desired: &ProviderGeneration,
        desired_revision: Option<&str>,
        required: &CapabilityInventory,
        path: &NativePathEvidence,
        expected: &PackageDigest,
    ) -> NativeObservation {
        if !self.available {
            return NativeObservation::Unavailable;
        }
        if !self.present {
            return NativeObservation::Absent;
        }
        if self.broken {
            return NativeObservation::Broken;
        }
        let Some(selector) = self.selector.as_ref() else {
            return NativeObservation::Unreadable;
        };
        let names_selector = intent
            .ownership_fingerprints
            .iter()
            .any(|fingerprint| fingerprint.selector == *selector);
        let full_proof = intent.client_scope.is_some()
            && intent.source_digest.is_some()
            && intent.view_digest.is_some();
        if intent.target != target
            || !names_selector
            || intent.desired_generation != *desired
            || !full_proof
        {
            return NativeObservation::External;
        }
        let expects_enabled = intent.enablement_intent != Some(EnablementIntent::Disabled);
        if self.enabled != expects_enabled {
            return if self.enabled {
                NativeObservation::Drifted
            } else {
                NativeObservation::Disabled
            };
        }
        if !self.native_fields_agree(desired, desired_revision, required) {
            return NativeObservation::Drifted;
        }
        match path {
            NativePathEvidence::Unreadable => NativeObservation::Unreadable,
            NativePathEvidence::Missing => NativeObservation::Drifted,
            NativePathEvidence::Digest(digest) if digest != expected => NativeObservation::Drifted,
            NativePathEvidence::Digest(_) => NativeObservation::Desired,
        }
    }

    /// Whether every native-reported field agrees with `generation`. A field
    /// the CLI omits is not evidence either way; the path observation decides.
    fn native_fields_agree(
        &self,
        generation: &ProviderGeneration,
        revision: Option<&str>,
        required: &CapabilityInventory,
    ) -> bool {
        self.source
            .as_deref()
            .is_none_or(|source| source == generation.source.as_str())
            && self
                .upstream_revision
                .as_deref()
                .is_none_or(|reported| Some(reported) == revision)
            && self
                .package_digest
                .as_deref()
                .is_none_or(|digest| digest == generation.package_digest.as_str())
            && required.is_subset(&self.capabilities)
    }
}

/// Parse failures intentionally do not imply absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeParseError {
    /// Input exceeded the observation byte limit.
    TooLarge,
    /// Input was not a valid observation object.
    Malformed,
}

/// Classify an observation parse failure as unreadable.
pub const fn parse_error_observation(_: NativeParseError) -> NativeObservation {
    NativeObservation::Unreadable
}

/// Upper bound on entries hashed for one copied-path observation.
pub const MAX_OBSERVED_ENTRIES: usize = 4096;

/// Upper bound on bytes hashed for one copied-path observation.
pub const MAX_OBSERVED_BYTES: u64 = 16 * 1024 * 1024;

/// Domain tag of a native view digest. It equals the tag the render layer
/// mixes into every package digest, so a view on disk hashes to the digest it
/// was published under.
const VIEW_DIGEST_DOMAIN: &[u8] = b"ccync-provider-generation\0v1\0";

/// Hex characters of the view digest that name a view directory.
const VIEW_ID_HEX_LEN: usize = 16;

/// Prefix of every native view directory name.
const VIEW_ID_PREFIX: &str = "v-";

/// Fresh content evidence for one copied generation path, read by ccync
/// itself rather than reported by the native CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePathEvidence {
    /// Nothing exists at the resolved path.
    Missing,
    /// Digest of the bytes present at the path when it was observed.
    Digest(PackageDigest),
    /// The path is a link, exceeds the bounds, or cannot be read.
    Unreadable,
}

/// Resolve a copied relative path under an approved root. Absolute paths,
/// drive prefixes, and parent traversal are rejected before any filesystem
/// access, so the result always stays inside `root`.
pub fn resolve_copied_path(root: &Path, relative: &str) -> Option<PathBuf> {
    let candidate = Path::new(relative);
    let inside = !relative.is_empty()
        && candidate
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    inside.then(|| root.join(candidate))
}

/// Observe the copied path `relative` under the approved `root` and digest the
/// bytes now on disk with the view digest encoding.
pub fn observe_copied_path(root: &Path, relative: &str) -> NativePathEvidence {
    resolve_copied_path(root, relative).map_or(NativePathEvidence::Unreadable, |path| {
        observe_view_digest(&path)
    })
}

/// Digest the view directory at `view` exactly as the render layer digests a
/// view: every regular file, sorted by relative path, length-prefixed, under
/// the view digest domain. Links, special entries, and bound overruns make the
/// observation unreadable. A missing directory is reported as missing.
pub fn observe_view_digest(view: &Path) -> NativePathEvidence {
    match fs::symlink_metadata(view) {
        Ok(meta) if crate::install::is_link_like(&meta) || !meta.is_dir() => {
            return NativePathEvidence::Unreadable;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return NativePathEvidence::Missing;
        }
        Err(_) => return NativePathEvidence::Unreadable,
    }
    let mut files = Vec::new();
    let mut budget = ObservationBudget::default();
    if collect_view_files(view, view, &mut files, &mut budget).is_err() {
        return NativePathEvidence::Unreadable;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    PackageDigest::new(format!("sha256:{}", view_digest_hex(&files)))
        .map_or(NativePathEvidence::Unreadable, NativePathEvidence::Digest)
}

/// Full SHA-256 hex over sorted `(relative path, bytes)` entries.
fn view_digest_hex(files: &[(String, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(VIEW_DIGEST_DOMAIN);
    for (rel, bytes) in files {
        hasher.update((rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

/// Installed view directory that `ownership` names:
/// `<generation>.views/<target>/v-<digest prefix>`. `None` when the record
/// lacks a view digest or the render root is unknown.
pub fn committed_view_path(ownership: &CommittedOwnership) -> Option<PathBuf> {
    let view_digest = ownership.view_digest.as_ref()?;
    let hex = view_digest.as_str().strip_prefix("sha256:")?;
    let view_id = format!("{VIEW_ID_PREFIX}{}", hex.get(..VIEW_ID_HEX_LEN)?);
    let generation_dir = ownership.generation.path()?;
    Some(
        crate::plugin_render::native_package_views_dir(&generation_dir)
            .join(ownership.target.as_str())
            .join(view_id),
    )
}

#[derive(Default)]
struct ObservationBudget {
    entries: usize,
    bytes: u64,
}

/// Collect the regular files under `dir`, keyed by their path relative to
/// `root`. Any link, special entry, non-UTF-8 name, or bound overrun fails.
fn collect_view_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, Vec<u8>)>,
    budget: &mut ObservationBudget,
) -> Result<(), ()> {
    for entry in fs::read_dir(dir).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|_| ())?;
        if crate::install::is_link_like(&meta) {
            return Err(());
        }
        budget.entries += 1;
        if budget.entries > MAX_OBSERVED_ENTRIES {
            return Err(());
        }
        if meta.is_dir() {
            collect_view_files(root, &path, out, budget)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| ())?
                .to_str()
                .ok_or(())?;
            #[cfg(windows)]
            let rel = rel.replace('\\', "/");
            #[cfg(not(windows))]
            if rel.contains('\\') {
                return Err(());
            }
            let remaining = MAX_OBSERVED_BYTES.checked_sub(budget.bytes).ok_or(())?;
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .map_err(|_| ())?
                .take(remaining + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| ())?;
            budget.bytes = budget
                .bytes
                .checked_add(u64::try_from(bytes.len()).map_err(|_| ())?)
                .ok_or(())?;
            if budget.bytes > MAX_OBSERVED_BYTES {
                return Err(());
            }
            out.push((rel.to_string(), bytes));
        } else {
            return Err(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod provider_observe_tests {
    use super::*;
    use ccync_foundation::provider::{
        ClientScope, ComponentKind, GenerationId, NativeObservation as State, PackageDigest,
        ProviderSourceKey,
    };
    use ccync_foundation::target::ProjectionTargetId;

    fn generation(digest: &str) -> ProviderGeneration {
        ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g1").unwrap(),
            package_digest: PackageDigest::new(digest).unwrap(),
        }
    }

    fn observation(digest: &str) -> NativeRegistration {
        NativeRegistration::parse_bounded(
            format!(
                r#"{{"available":true,"present":true,"enabled":true,"selector":"demo@ccync","source":"demo","upstreamRevision":"rev1","packageDigest":"{digest}","capabilities":["skill"]}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    fn committed(digest: &str) -> CommittedOwnership {
        CommittedOwnership {
            target: ProjectionTargetId::Codex,
            selector: NativeSelector::new("demo@ccync").unwrap(),
            plugin_name: "demo".to_string(),
            upstream_revision: Some("rev1".to_string()),
            generation: generation(digest),
            capabilities: [ComponentKind::Skill].into_iter().collect(),
            client_scope: Some(ClientScope::new("demo-client").unwrap()),
            source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
            view_digest: Some(PackageDigest::new("sha256:view").unwrap()),
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        }
    }

    fn path(digest: &str) -> NativePathEvidence {
        NativePathEvidence::Digest(PackageDigest::new(digest).unwrap())
    }

    #[test]
    fn provider_observe_state_matrix_separates_conflicts_and_owned_upgrade() {
        let required: CapabilityInventory = [ComponentKind::Skill].into_iter().collect();
        let desired = generation("sha256:new");
        assert_eq!(
            observation("sha256:old").classify(
                ProjectionTargetId::Codex,
                Some(&committed("sha256:old")),
                &desired,
                Some("rev1"),
                &required,
                &path("sha256:old"),
            ),
            State::OwnedOld
        );
        assert_eq!(
            observation("sha256:new").classify(
                ProjectionTargetId::Codex,
                Some(&committed("sha256:new")),
                &desired,
                Some("rev1"),
                &required,
                &path("sha256:new"),
            ),
            State::Desired
        );
        assert_eq!(
            observation("sha256:old").classify(
                ProjectionTargetId::Codex,
                None,
                &desired,
                Some("rev1"),
                &required,
                &path("sha256:old"),
            ),
            State::External
        );
        let mut disabled = observation("sha256:old");
        disabled.enabled = false;
        assert_eq!(
            disabled.classify(
                ProjectionTargetId::Codex,
                Some(&committed("sha256:old")),
                &desired,
                Some("rev1"),
                &required,
                &path("sha256:old"),
            ),
            State::Disabled
        );
        let mut broken = observation("sha256:old");
        broken.broken = true;
        assert_eq!(
            broken.classify(
                ProjectionTargetId::Codex,
                Some(&committed("sha256:old")),
                &desired,
                Some("rev1"),
                &required,
                &path("sha256:old"),
            ),
            State::Broken
        );
        assert_eq!(
            parse_error_observation(NativeParseError::Malformed),
            State::Unreadable
        );
        let absent =
            NativeRegistration::parse_bounded(br#"{"available":true,"present":false}"#).unwrap();
        assert_eq!(
            absent.classify(
                ProjectionTargetId::Codex,
                None,
                &desired,
                Some("rev1"),
                &required,
                &path("sha256:old"),
            ),
            State::Absent
        );
        let unavailable = NativeRegistration::parse_bounded(br#"{"available":false}"#).unwrap();
        assert_eq!(
            unavailable.classify(
                ProjectionTargetId::Codex,
                None,
                &desired,
                Some("rev1"),
                &required,
                &path("sha256:old"),
            ),
            State::Unavailable
        );
        assert_eq!(
            observation("sha256:tampered").classify(
                ProjectionTargetId::Codex,
                Some(&committed("sha256:old")),
                &desired,
                Some("rev1"),
                &required,
                &path("sha256:old"),
            ),
            State::Drifted
        );
    }

    #[test]
    fn provider_observe_rejects_oversized_json() {
        assert_eq!(
            NativeRegistration::parse_bounded(&vec![b' '; MAX_NATIVE_OBSERVATION_BYTES + 1]),
            Err(NativeParseError::TooLarge)
        );
    }

    #[test]
    fn provider_native_recovery_uses_observed_state_before_mutation() {
        let desired = generation("sha256:new");
        let previous = generation("sha256:old");
        assert_eq!(
            recovery_action(State::Desired, &desired, Some(&desired)),
            NativeRecoveryAction::CommitObserved
        );
        assert_eq!(
            recovery_action(State::Absent, &desired, None),
            NativeRecoveryAction::Register
        );
        assert_eq!(
            recovery_action(State::OwnedOld, &desired, Some(&previous)),
            NativeRecoveryAction::UpgradeOwned
        );
        for state in [
            State::External,
            State::Disabled,
            State::Broken,
            State::Unreadable,
            State::Drifted,
            State::Unavailable,
        ] {
            assert_eq!(
                recovery_action(state, &desired, Some(&previous)),
                NativeRecoveryAction::PreservePartial,
                "{state:?} must preserve the current registration"
            );
        }
        assert_eq!(
            recovery_action(State::Desired, &desired, None),
            NativeRecoveryAction::PreservePartial,
            "desired-looking state without generation proof must not be committed"
        );
    }
}

#[cfg(test)]
mod p7_t06_native_ownership_tests {
    use super::*;
    use ccync_foundation::provider::{
        ClientScope, ComponentKind, GenerationId, IntentPhase, OwnershipFingerprint,
        ProviderSourceKey,
    };

    fn capabilities() -> CapabilityInventory {
        [ComponentKind::Skill].into_iter().collect()
    }

    /// Writes one copied package under `root/demo` and returns the generation
    /// plus the fresh path evidence that ccync observed for those bytes.
    fn install_copy(root: &Path, body: &str) -> (ProviderGeneration, NativePathEvidence) {
        fs::create_dir_all(root.join("demo")).unwrap();
        fs::write(root.join("demo").join("SKILL.md"), body).unwrap();
        let evidence = observe_copied_path(root, "demo");
        let NativePathEvidence::Digest(digest) = evidence.clone() else {
            panic!("copied package must produce a digest, got {evidence:?}");
        };
        let generation = ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g1").unwrap(),
            package_digest: digest,
        };
        (generation, evidence)
    }

    fn intent_for(selector: &str, generation: &ProviderGeneration) -> PendingIntent {
        PendingIntent {
            target: ProjectionTargetId::Codex,
            ownership_fingerprints: vec![OwnershipFingerprint {
                selector: NativeSelector::new(selector).unwrap(),
                package_digest: generation.package_digest.clone(),
            }],
            scoped_paths: Vec::new(),
            preimage_refs: Vec::new(),
            previous_generation: None,
            desired_generation: generation.clone(),
            phase: IntentPhase::Verifying,
            client_scope: Some(ClientScope::new("demo-client").unwrap()),
            source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
            view_digest: Some(PackageDigest::new("sha256:view").unwrap()),
            observed_preimage: None,
            enablement_intent: Some(EnablementIntent::Enabled),
            group_substep: None,
        }
    }

    /// A listed registration for `demo@ccync` with `extra` JSON fields appended.
    fn listed(extra: &str) -> NativeRegistration {
        NativeRegistration::parse_bounded(
            format!(
                r#"{{"available":true,"present":true,"enabled":true,"selector":"demo@ccync","capabilities":["skill"]{extra}}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn p7_t06_first_install_needs_matching_intent_and_fresh_content() {
        let dir = tempfile::tempdir().unwrap();
        let (generation, path) = install_copy(dir.path(), "v1");
        let intent = intent_for("demo@ccync", &generation);
        assert_eq!(
            listed("").classify_pending(
                ProjectionTargetId::Codex,
                &intent,
                &generation,
                Some("rev1"),
                &capabilities(),
                &path,
            ),
            NativeObservation::Desired
        );
    }

    #[test]
    fn p7_t06_same_name_without_matching_intent_stays_external() {
        let dir = tempfile::tempdir().unwrap();
        let (generation, path) = install_copy(dir.path(), "v1");
        let other = intent_for("other@ccync", &generation);
        assert_eq!(
            listed("").classify_pending(
                ProjectionTargetId::Codex,
                &other,
                &generation,
                None,
                &capabilities(),
                &path,
            ),
            NativeObservation::External,
            "an intent for a different selector must not claim this registration"
        );
        assert_eq!(
            listed("").classify(
                ProjectionTargetId::Codex,
                None,
                &generation,
                None,
                &capabilities(),
                &path,
            ),
            NativeObservation::External,
            "a same-name registration without committed proof is external"
        );
    }

    #[test]
    fn p7_t06_copied_digest_mismatch_stays_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let (generation, path) = install_copy(dir.path(), "v1");
        let intent = intent_for("demo@ccync", &generation);
        assert_eq!(
            listed(r#","packageDigest":"sha256:forged""#).classify_pending(
                ProjectionTargetId::Codex,
                &intent,
                &generation,
                None,
                &capabilities(),
                &path,
            ),
            NativeObservation::Drifted,
            "a CLI-reported digest that differs from the intent must block"
        );
        let other_bytes = NativePathEvidence::Digest(PackageDigest::new("sha256:copied").unwrap());
        assert_eq!(
            listed("").classify_pending(
                ProjectionTargetId::Codex,
                &intent,
                &generation,
                None,
                &capabilities(),
                &other_bytes,
            ),
            NativeObservation::Drifted,
            "copied bytes that differ from the desired digest must block"
        );
    }

    #[test]
    fn p7_t06_post_intent_foreign_change_stays_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let (generation, _) = install_copy(dir.path(), "v1");
        let intent = intent_for("demo@ccync", &generation);
        fs::write(dir.path().join("demo").join("extra.md"), "foreign").unwrap();
        let changed = observe_copied_path(dir.path(), "demo");
        assert_ne!(
            changed,
            NativePathEvidence::Digest(generation.package_digest.clone())
        );
        assert_eq!(
            listed("").classify_pending(
                ProjectionTargetId::Codex,
                &intent,
                &generation,
                None,
                &capabilities(),
                &changed,
            ),
            NativeObservation::Drifted
        );
        fs::remove_dir_all(dir.path().join("demo")).unwrap();
        assert_eq!(
            listed("").classify_pending(
                ProjectionTargetId::Codex,
                &intent,
                &generation,
                None,
                &capabilities(),
                &observe_copied_path(dir.path(), "demo"),
            ),
            NativeObservation::Drifted,
            "a registration whose copied path vanished must block"
        );
    }

    #[test]
    fn p7_t06_copied_paths_resolve_only_inside_the_approved_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_copied_path(dir.path(), "demo/SKILL.md"),
            Some(dir.path().join("demo").join("SKILL.md"))
        );
        for escape in ["", "../outside", "demo/../../outside", "/etc/passwd"] {
            assert_eq!(resolve_copied_path(dir.path(), escape), None, "{escape:?}");
        }
        assert_eq!(
            observe_copied_path(dir.path(), "../outside"),
            NativePathEvidence::Unreadable
        );
    }

    #[test]
    fn p7_t06_unknown_or_missing_fields_never_imply_enabled_or_owned() {
        let dir = tempfile::tempdir().unwrap();
        let (generation, path) = install_copy(dir.path(), "v1");
        let intent = intent_for("demo@ccync", &generation);
        let unknown = NativeRegistration::parse_bounded(
            br#"{"available":true,"present":true,"selector":"demo@ccync","owner":"ccync","enabledByDefault":true}"#,
        )
        .unwrap();
        assert!(!unknown.enabled, "a missing enabled field must not enable");
        assert_eq!(
            unknown.classify_pending(
                ProjectionTargetId::Codex,
                &intent,
                &generation,
                None,
                &capabilities(),
                &path,
            ),
            NativeObservation::Disabled
        );
        let unreachable = NativeRegistration::parse_bounded(
            br#"{"present":true,"enabled":true,"selector":"demo@ccync"}"#,
        )
        .unwrap();
        assert!(
            !unreachable.available,
            "a missing available field must not report availability"
        );
        assert_eq!(
            unreachable.classify_pending(
                ProjectionTargetId::Codex,
                &intent,
                &generation,
                None,
                &capabilities(),
                &path,
            ),
            NativeObservation::Unavailable
        );
    }
}

#[cfg(test)]
mod p7_t07_codex_adapter_tests {
    use super::*;
    use ccync_foundation::provider::{ClientScope, ComponentKind, GenerationId, ProviderSourceKey};
    use serde_json::json;
    use std::cell::RefCell;

    const ROOT: &str = "C:/ccync/marketplace";

    fn generation() -> ProviderGeneration {
        ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g1").unwrap(),
            package_digest: PackageDigest::new("sha256:new").unwrap(),
        }
    }

    fn skill() -> CapabilityInventory {
        [ComponentKind::Skill].into_iter().collect()
    }

    fn selector() -> NativeSelector {
        NativeSelector::new("demo@ccync").unwrap()
    }

    /// One installed Codex entry, shaped the way `parse_cli_registration` reads it.
    fn installed_entry(digest: &str, enabled: bool) -> Value {
        json!({
            "id": "demo@ccync",
            "enabled": enabled,
            "source": "demo",
            "upstreamRevision": "rev1",
            "packageDigest": digest,
            "capabilities": ["skill"],
        })
    }

    /// Fake Codex state. Each invocation is recorded, then applied so that
    /// later observations reflect it through the real listing parser.
    struct FakeCodex {
        installed: RefCell<Option<Value>>,
        marketplace: RefCell<Option<String>>,
        calls: RefCell<Vec<String>>,
        verified: bool,
        add_registers: bool,
    }

    impl FakeCodex {
        fn new(installed: Option<Value>) -> Self {
            Self {
                installed: RefCell::new(installed),
                marketplace: RefCell::new(None),
                calls: RefCell::new(Vec::new()),
                verified: true,
                add_registers: true,
            }
        }

        fn listing(&self) -> Vec<u8> {
            let installed: Vec<Value> = self.installed.borrow().iter().cloned().collect();
            serde_json::to_vec(&json!({"installed": installed, "available": []})).unwrap()
        }
    }

    impl NativeCommandRunner for FakeCodex {
        fn mutation_verified(&self, _executable: NativeExecutable) -> bool {
            self.verified
        }

        fn run(&self, executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
            self.calls
                .borrow_mut()
                .push(format!("{} {}", executable.command(), args.join(" ")));
            match args {
                ["plugin", "add", _] => {
                    if self.add_registers {
                        *self.installed.borrow_mut() = Some(installed_entry("sha256:new", true));
                    }
                }
                ["plugin", "remove", _] => *self.installed.borrow_mut() = None,
                ["plugin", "marketplace", "add", root] => {
                    *self.marketplace.borrow_mut() = Some((*root).to_string());
                }
                _ => return Err(format!("unexpected invocation {args:?}")),
            }
            Ok(Vec::new())
        }

        fn observe(
            &self,
            executable: NativeExecutable,
            selector: &NativeSelector,
        ) -> Result<NativeRegistration, String> {
            parse_cli_registration(executable, &self.listing(), selector)
        }

        fn marketplace_root(
            &self,
            _executable: NativeExecutable,
            _marketplace: &str,
        ) -> Result<Option<String>, String> {
            Ok(self.marketplace.borrow().clone())
        }
    }

    /// Files of the owned view fixture, sorted by relative path.
    fn view_entries() -> Vec<(String, Vec<u8>)> {
        vec![
            ("plugin.json".to_string(), b"{\"name\":\"demo\"}".to_vec()),
            (
                "skills/demo/SKILL.md".to_string(),
                b"# demo
"
                .to_vec(),
            ),
        ]
    }

    fn view_digest() -> PackageDigest {
        PackageDigest::new(format!("sha256:{}", view_digest_hex(&view_entries()))).unwrap()
    }

    /// Write the owned view fixture where the committed record names it.
    /// The caller holds a `TestHome`, so the render root is isolated.
    fn publish_view(ownership: &CommittedOwnership) {
        let path = committed_view_path(ownership).expect("render root resolves");
        for (rel, bytes) in view_entries() {
            let file = path.join(&rel);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, bytes).unwrap();
        }
    }

    fn committed(digest: &str) -> CommittedOwnership {
        CommittedOwnership {
            target: ProjectionTargetId::Codex,
            selector: selector(),
            plugin_name: "demo".to_string(),
            upstream_revision: Some("rev1".to_string()),
            generation: ProviderGeneration {
                package_digest: PackageDigest::new(digest).unwrap(),
                ..generation()
            },
            capabilities: skill(),
            client_scope: Some(ClientScope::new("demo-client").unwrap()),
            source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
            view_digest: Some(view_digest()),
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        }
    }

    fn request<'a>(
        selector: &'a NativeSelector,
        generation: &'a ProviderGeneration,
        capabilities: &'a CapabilityInventory,
    ) -> NativeInstall<'a> {
        NativeInstall {
            selector,
            marketplace_root: ROOT,
            generation,
            upstream_revision: Some("rev1"),
            capabilities,
        }
    }

    #[test]
    fn p7_t07_codex_install_registers_marketplace_then_exact_selector() {
        let fake = FakeCodex::new(None);
        let (selector, generation, capabilities) = (selector(), generation(), skill());
        install_native_registration(
            &fake,
            NativeExecutable::Codex,
            &request(&selector, &generation, &capabilities),
        )
        .expect("install verifies the registration");
        assert_eq!(
            *fake.calls.borrow(),
            [
                "codex plugin marketplace add C:/ccync/marketplace",
                "codex plugin add demo@ccync",
            ]
        );
    }

    #[test]
    fn p7_t07_codex_install_reuses_matching_marketplace_and_refuses_other_roots() {
        let (selector, generation, capabilities) = (selector(), generation(), skill());
        let matching = FakeCodex::new(None);
        *matching.marketplace.borrow_mut() = Some(ROOT.to_string());
        install_native_registration(
            &matching,
            NativeExecutable::Codex,
            &request(&selector, &generation, &capabilities),
        )
        .expect("install");
        assert_eq!(*matching.calls.borrow(), ["codex plugin add demo@ccync"]);

        let other = FakeCodex::new(None);
        *other.marketplace.borrow_mut() = Some("D:/elsewhere".to_string());
        assert!(matches!(
            install_native_registration(
                &other,
                NativeExecutable::Codex,
                &request(&selector, &generation, &capabilities),
            ),
            Err(NativeReconcileError::MarketplaceConflict)
        ));
        assert!(other.calls.borrow().is_empty(), "no CLI mutation");
    }

    #[test]
    fn p7_t07_foreign_and_disabled_registrations_block_install_and_removal() {
        let (selector, generation, capabilities) = (selector(), generation(), skill());
        for (entry, expected) in [
            (
                installed_entry("sha256:new", true),
                NativeObservation::External,
            ),
            (
                installed_entry("sha256:new", false),
                NativeObservation::Disabled,
            ),
        ] {
            let fake = FakeCodex::new(Some(entry));
            assert!(matches!(
                install_native_registration(
                    &fake,
                    NativeExecutable::Codex,
                    &request(&selector, &generation, &capabilities),
                ),
                Err(NativeReconcileError::Conflict(state)) if state == expected
            ));
            assert!(
                fake.calls.borrow().is_empty(),
                "install must not touch {expected:?}"
            );
        }
    }

    #[test]
    fn p7_t07_removal_refuses_foreign_registrations_without_mutation() {
        let _home = crate::test_support::TestHome::new();
        let owned = committed("sha256:new");
        publish_view(&owned);
        // Same name, different digest: the registration is not the committed one.
        let foreign = FakeCodex::new(Some(installed_entry("sha256:other", true)));
        assert!(remove_committed_registration(&foreign, NativeExecutable::Codex, &owned).is_err());
        assert!(
            foreign.calls.borrow().is_empty(),
            "foreign registration kept"
        );
    }

    #[test]
    fn p7_t07_removal_removes_a_disabled_owned_registration_with_proof() {
        let _home = crate::test_support::TestHome::new();
        let owned = committed("sha256:new");
        publish_view(&owned);
        // The committed registration, reported disabled, is still owned by proof.
        let disabled = FakeCodex::new(Some(installed_entry("sha256:new", false)));
        remove_committed_registration(&disabled, NativeExecutable::Codex, &owned)
            .expect("a disabled owned registration is removable");
        assert_eq!(*disabled.calls.borrow(), ["codex plugin remove demo@ccync"]);
    }

    #[test]
    fn p7_t07_unverified_add_stops_after_one_attempt_and_keeps_intent_state() {
        let mut fake = FakeCodex::new(None);
        fake.add_registers = false;
        let (selector, generation, capabilities) = (selector(), generation(), skill());
        assert!(matches!(
            install_native_registration(
                &fake,
                NativeExecutable::Codex,
                &request(&selector, &generation, &capabilities),
            ),
            Err(NativeReconcileError::Verification(
                NativeObservation::Absent
            ))
        ));
        assert_eq!(
            fake.calls
                .borrow()
                .iter()
                .filter(|call| call.contains("plugin add"))
                .count(),
            1,
            "a failed verification is never retried inside the adapter"
        );
    }

    #[test]
    fn p7_t07_removal_uses_the_exact_committed_selector() {
        let _home = crate::test_support::TestHome::new();
        let owned = committed("sha256:new");
        publish_view(&owned);
        let fake = FakeCodex::new(Some(installed_entry("sha256:new", true)));
        remove_committed_registration(&fake, NativeExecutable::Codex, &owned)
            .expect("exact owned registration is removed");
        assert_eq!(*fake.calls.borrow(), ["codex plugin remove demo@ccync"]);
    }

    #[test]
    fn p7_t07_update_removes_the_exact_old_generation_before_installing_the_new_one() {
        let _home = crate::test_support::TestHome::new();
        publish_view(&committed("sha256:old"));
        let fake = FakeCodex::new(Some(installed_entry("sha256:old", true)));
        *fake.marketplace.borrow_mut() = Some(ROOT.to_string());
        let (selector, new, capabilities) = (selector(), generation(), skill());
        update_native_registration(
            &fake,
            NativeExecutable::Codex,
            &committed("sha256:old"),
            &request(&selector, &new, &capabilities),
        )
        .expect("update verifies the new generation");
        assert_eq!(
            *fake.calls.borrow(),
            [
                "codex plugin remove demo@ccync",
                "codex plugin add demo@ccync"
            ]
        );
    }

    #[test]
    fn p7_t07_mutation_is_gated_per_adapter() {
        assert_eq!(
            NativeExecutable::Codex.verified_version_line(),
            CODEX_VERIFIED_VERSION_LINE
        );
        let mut fake = FakeCodex::new(Some(installed_entry("sha256:new", true)));
        fake.verified = false;
        let (selector, generation, capabilities) = (selector(), generation(), skill());
        assert!(matches!(
            install_native_registration(
                &fake,
                NativeExecutable::Codex,
                &request(&selector, &generation, &capabilities),
            ),
            Err(NativeReconcileError::OwnershipEvidenceUnverified(
                NativeExecutable::Codex
            ))
        ));
        assert!(matches!(
            remove_committed_registration(&fake, NativeExecutable::Codex, &committed("sha256:new")),
            Err(NativeReconcileError::OwnershipEvidenceUnverified(
                NativeExecutable::Codex
            ))
        ));
        assert!(
            fake.calls.borrow().is_empty(),
            "no CLI call before verification"
        );
    }

    #[test]
    fn p7_t07_available_entries_are_never_observed_as_installed() {
        let registration = parse_cli_registration(
            NativeExecutable::Codex,
            br#"{"installed":[],"available":[{"id":"demo@ccync","enabled":true}]}"#,
            &selector(),
        )
        .expect("listing parses");
        assert!(!registration.present, "an available entry is not installed");
    }

    #[test]
    fn p7_t07_bounded_output_rejects_streams_past_the_limit() {
        assert_eq!(read_bounded(&b"abc"[..], 3), Some(b"abc".to_vec()));
        assert_eq!(read_bounded(&b"abcd"[..], 3), None);
    }
}

#[cfg(test)]
mod p7_t08_claude_adapter_tests {
    use super::*;
    use ccync_foundation::provider::{ClientScope, GenerationId, ProviderSourceKey};
    use serde_json::json;
    use std::cell::RefCell;

    const ROOT: &str = "C:/ccync/claude-view";

    fn generation() -> ProviderGeneration {
        ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g1").unwrap(),
            package_digest: PackageDigest::new("sha256:new").unwrap(),
        }
    }

    fn selector() -> NativeSelector {
        NativeSelector::new("demo@ccync").unwrap()
    }

    /// One entry shaped the way `claude plugin list --json` reports it. Claude
    /// Code reports no ccync package digest, so the entry carries none.
    fn listed(scope: &str, enabled: bool) -> Value {
        json!({
            "id": "demo@ccync",
            "version": "0.3.0",
            "scope": scope,
            "enabled": enabled,
            "installPath": "C:/cache/ccync/demo/0.3.0",
            "readFromFolder": "C:/ccync/claude-view/demo",
            "folderVersion": "0.3.0",
            "projectEnabled": false,
        })
    }

    /// Fake Claude Code state. Each invocation is recorded, then applied so that
    /// later observations reflect it through the real listing parser.
    struct FakeClaude {
        installed: RefCell<Option<Value>>,
        marketplace: RefCell<Option<String>>,
        calls: RefCell<Vec<String>>,
        verified: bool,
        add_registers: bool,
    }

    impl FakeClaude {
        fn new(installed: Option<Value>) -> Self {
            Self {
                installed: RefCell::new(installed),
                marketplace: RefCell::new(None),
                calls: RefCell::new(Vec::new()),
                verified: true,
                add_registers: true,
            }
        }

        fn listing(&self) -> Vec<u8> {
            let installed: Vec<Value> = self.installed.borrow().iter().cloned().collect();
            serde_json::to_vec(&Value::Array(installed)).unwrap()
        }

        fn set_enabled(&self, enabled: bool) {
            if let Some(entry) = self.installed.borrow_mut().as_mut() {
                entry["enabled"] = json!(enabled);
            }
        }
    }

    impl NativeCommandRunner for FakeClaude {
        fn mutation_verified(&self, _executable: NativeExecutable) -> bool {
            self.verified
        }

        fn run(&self, executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
            self.calls
                .borrow_mut()
                .push(format!("{} {}", executable.command(), args.join(" ")));
            match args {
                ["plugin", "install", _, "--scope", "user"] => {
                    if self.add_registers {
                        *self.installed.borrow_mut() = Some(listed("user", true));
                    }
                }
                ["plugin", "uninstall", _, "--scope", "user"] => {
                    *self.installed.borrow_mut() = None;
                }
                ["plugin", "enable", _, "--scope", "user"] => self.set_enabled(true),
                ["plugin", "disable", _, "--scope", "user"] => self.set_enabled(false),
                ["plugin", "marketplace", "add", root, "--scope", "user"] => {
                    *self.marketplace.borrow_mut() = Some((*root).to_string());
                }
                _ => return Err(format!("unexpected invocation {args:?}")),
            }
            Ok(Vec::new())
        }

        fn observe(
            &self,
            executable: NativeExecutable,
            selector: &NativeSelector,
        ) -> Result<NativeRegistration, String> {
            parse_cli_registration(executable, &self.listing(), selector)
        }

        fn marketplace_root(
            &self,
            _executable: NativeExecutable,
            _marketplace: &str,
        ) -> Result<Option<String>, String> {
            Ok(self.marketplace.borrow().clone())
        }
    }

    fn committed(digest: &str) -> CommittedOwnership {
        CommittedOwnership {
            target: ProjectionTargetId::Claude,
            selector: selector(),
            plugin_name: "demo".to_string(),
            upstream_revision: None,
            generation: ProviderGeneration {
                package_digest: PackageDigest::new(digest).unwrap(),
                ..generation()
            },
            capabilities: CapabilityInventory::new(),
            client_scope: Some(ClientScope::new("demo-client").unwrap()),
            source_digest: Some(PackageDigest::new("sha256:src").unwrap()),
            view_digest: Some(PackageDigest::new("sha256:view").unwrap()),
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        }
    }

    fn request<'a>(
        selector: &'a NativeSelector,
        generation: &'a ProviderGeneration,
        capabilities: &'a CapabilityInventory,
    ) -> NativeInstall<'a> {
        NativeInstall {
            selector,
            marketplace_root: ROOT,
            generation,
            upstream_revision: None,
            capabilities,
        }
    }

    #[test]
    fn p7_t08_install_adds_user_scope_marketplace_then_exact_selector() {
        let fake = FakeClaude::new(None);
        let (selector, generation, capabilities) =
            (selector(), generation(), CapabilityInventory::new());
        install_native_registration(
            &fake,
            NativeExecutable::Claude,
            &request(&selector, &generation, &capabilities),
        )
        .expect("install verifies the registration");
        assert_eq!(
            *fake.calls.borrow(),
            [
                "claude plugin marketplace add C:/ccync/claude-view --scope user",
                "claude plugin install demo@ccync --scope user",
            ]
        );
    }

    #[test]
    fn p7_t08_foreign_scope_or_disabled_registration_blocks_install_without_mutation() {
        let (selector, generation, capabilities) =
            (selector(), generation(), CapabilityInventory::new());
        for (entry, expected) in [
            (listed("project", true), NativeObservation::External),
            (listed("project", false), NativeObservation::Disabled),
            (listed("user", false), NativeObservation::Disabled),
        ] {
            let fake = FakeClaude::new(Some(entry));
            assert!(matches!(
                install_native_registration(
                    &fake,
                    NativeExecutable::Claude,
                    &request(&selector, &generation, &capabilities),
                ),
                Err(NativeReconcileError::Conflict(state)) if state == expected
            ));
            assert!(
                fake.calls.borrow().is_empty(),
                "install must not touch {expected:?}"
            );
        }
    }

    #[test]
    fn p7_t08_unverified_install_stops_after_one_add_and_keeps_intent_state() {
        let mut fake = FakeClaude::new(None);
        fake.add_registers = false;
        let (selector, generation, capabilities) =
            (selector(), generation(), CapabilityInventory::new());
        assert!(matches!(
            install_native_registration(
                &fake,
                NativeExecutable::Claude,
                &request(&selector, &generation, &capabilities),
            ),
            Err(NativeReconcileError::Verification(
                NativeObservation::Absent
            ))
        ));
        assert_eq!(
            fake.calls
                .borrow()
                .iter()
                .filter(|call| call.contains("plugin install"))
                .count(),
            1,
            "a failed verification is never retried inside the adapter"
        );
    }

    #[test]
    fn p7_t08_enablement_changes_only_the_owned_user_registration() {
        let fake = FakeClaude::new(Some(listed("user", true)));
        set_native_enablement(
            &fake,
            NativeExecutable::Claude,
            &committed("sha256:new"),
            false,
        )
        .expect("owned registration is disabled");
        assert_eq!(
            *fake.calls.borrow(),
            ["claude plugin disable demo@ccync --scope user"]
        );

        set_native_enablement(
            &fake,
            NativeExecutable::Claude,
            &committed("sha256:new"),
            false,
        )
        .expect("a repeated request is a no-op");
        assert_eq!(fake.calls.borrow().len(), 1, "no second CLI mutation");

        set_native_enablement(
            &fake,
            NativeExecutable::Claude,
            &committed("sha256:new"),
            true,
        )
        .expect("owned registration is enabled again");
        assert_eq!(
            fake.calls.borrow().last().map(String::as_str),
            Some("claude plugin enable demo@ccync --scope user")
        );
    }

    #[test]
    fn p7_t08_enablement_refuses_foreign_scope_without_mutation() {
        let fake = FakeClaude::new(Some(listed("project", true)));
        assert!(matches!(
            set_native_enablement(
                &fake,
                NativeExecutable::Claude,
                &committed("sha256:new"),
                false,
            ),
            Err(NativeReconcileError::Conflict(NativeObservation::Drifted))
        ));
        assert!(fake.calls.borrow().is_empty(), "project-scope entry kept");
    }

    #[test]
    fn p7_t08_enablement_requires_a_verified_adapter_and_a_native_command() {
        let mut fake = FakeClaude::new(Some(listed("user", true)));
        fake.verified = false;
        assert!(matches!(
            set_native_enablement(
                &fake,
                NativeExecutable::Claude,
                &committed("sha256:new"),
                false,
            ),
            Err(NativeReconcileError::OwnershipEvidenceUnverified(
                NativeExecutable::Claude
            ))
        ));
        assert!(fake.calls.borrow().is_empty(), "no CLI call before proof");

        let codex = FakeClaude::new(Some(listed("user", true)));
        assert!(matches!(
            set_native_enablement(
                &codex,
                NativeExecutable::Codex,
                &committed("sha256:new"),
                false,
            ),
            Err(NativeReconcileError::EnablementUnsupported(
                NativeExecutable::Codex
            ))
        ));
    }

    #[test]
    fn p7_t08_removal_refuses_without_a_native_digest_and_keeps_registration() {
        // Claude Code reports no package digest, so the exact digest check that
        // gates removal cannot pass. The registration stays, with no CLI call.
        let fake = FakeClaude::new(Some(listed("user", true)));
        assert!(matches!(
            remove_committed_registration(
                &fake,
                NativeExecutable::Claude,
                &committed("sha256:new")
            ),
            Err(NativeReconcileError::Conflict(NativeObservation::Drifted))
        ));
        assert!(fake.calls.borrow().is_empty(), "registration kept");
    }

    #[test]
    fn p7_t08_unverified_adapter_makes_no_claude_call() {
        let mut fake = FakeClaude::new(None);
        fake.verified = false;
        let (selector, generation, capabilities) =
            (selector(), generation(), CapabilityInventory::new());
        assert!(matches!(
            install_native_registration(
                &fake,
                NativeExecutable::Claude,
                &request(&selector, &generation, &capabilities),
            ),
            Err(NativeReconcileError::OwnershipEvidenceUnverified(
                NativeExecutable::Claude
            ))
        ));
        assert!(fake.calls.borrow().is_empty(), "no CLI call before proof");
    }

    #[test]
    fn p7_t08_listing_names_only_the_user_scope_registration() {
        let listing = br#"[
            {"id":"demo@ccync","scope":"user","enabled":true},
            {"id":"other@ccync","scope":"project","enabled":true}
        ]"#;
        let owned = parse_cli_registration(NativeExecutable::Claude, listing, &selector())
            .expect("listing parses");
        assert!(owned.present && owned.enabled);
        assert_eq!(owned.selector, Some(selector()));

        let foreign = br#"[{"id":"demo@ccync","scope":"local","enabled":true}]"#;
        let registration = parse_cli_registration(NativeExecutable::Claude, foreign, &selector())
            .expect("listing parses");
        assert!(registration.present, "a foreign-scope entry is present");
        assert_eq!(
            registration.selector, None,
            "but it never names an owned selector"
        );
    }

    #[test]
    fn p7_t08_marketplace_listing_resolves_the_owned_directory_root() {
        let listing = br#"[
            {"name":"ccync","source":"directory","path":"C:\\ccync\\claude-view","installLocation":"C:\\ccync\\claude-view"},
            {"name":"brag","source":"git","url":"https://example.invalid/brag.git","installLocation":"C:\\cache\\brag"}
        ]"#;
        assert_eq!(
            parse_marketplace_root(NativeExecutable::Claude, listing, "ccync")
                .expect("listing parses"),
            Some(r"C:\ccync\claude-view".to_string())
        );
        assert_eq!(
            parse_marketplace_root(NativeExecutable::Claude, listing, "missing")
                .expect("listing parses"),
            None
        );
        // Backslash is a path separator only on Windows.
        #[cfg(windows)]
        assert!(same_root(r"C:\ccync\claude-view", "C:/ccync/claude-view"));
        assert!(same_root("/ccync/claude-view", "/ccync/claude-view/"));
        assert!(!same_root(r"D:\elsewhere", "C:/ccync/claude-view"));
    }
}

#[cfg(test)]
mod p7_t12_view_digest_tests {
    use super::*;

    /// The observed digest of a view equals the digest the render layer
    /// publishes for the same files: the view digest domain, then each file's
    /// length-prefixed path and bytes in sorted path order.
    #[test]
    fn p7_t12_view_digest_matches_the_render_encoding() {
        let scratch = tempfile::TempDir::new().unwrap();
        fs::write(scratch.path().join("plugin.json"), "{\"name\":\"demo\"}").unwrap();
        fs::create_dir_all(scratch.path().join("skills/demo")).unwrap();
        fs::write(scratch.path().join("skills/demo/SKILL.md"), "# demo\n").unwrap();

        assert_eq!(
            observe_view_digest(scratch.path()),
            NativePathEvidence::Digest(
                PackageDigest::new(
                    "sha256:dc71e389030547abc6444be2886a2c2dc2c46ab19f8e57e151c4c76ff8940257"
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn p7_t12_missing_view_is_missing_not_matching() {
        let scratch = tempfile::TempDir::new().unwrap();
        assert_eq!(
            observe_view_digest(&scratch.path().join("absent")),
            NativePathEvidence::Missing
        );
    }
}

#[cfg(test)]
mod p7_t12_eligibility_tests {
    use super::*;
    use ccync_foundation::provider::{ClientScope, ComponentKind, GenerationId, ProviderSourceKey};
    use std::cell::RefCell;

    const ROOT: &str = "C:/ccync/marketplace";

    /// Runner that implements only the required methods, so every default applies.
    struct BareRunner {
        calls: RefCell<Vec<String>>,
    }

    impl NativeCommandRunner for BareRunner {
        fn run(&self, executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
            self.calls
                .borrow_mut()
                .push(format!("{} {}", executable.command(), args.join(" ")));
            Err("bare runner runs nothing".to_string())
        }

        fn observe(
            &self,
            _executable: NativeExecutable,
            _selector: &NativeSelector,
        ) -> Result<NativeRegistration, String> {
            Err("bare runner observes nothing".to_string())
        }

        fn marketplace_root(
            &self,
            _executable: NativeExecutable,
            _marketplace: &str,
        ) -> Result<Option<String>, String> {
            Err("bare runner resolves nothing".to_string())
        }
    }

    /// Fake whose CLI reports `version` from `--version`. Its eligibility is
    /// judged by the same rule the production runner applies to that output.
    /// Every other call is recorded and refused, so a mutating call is visible.
    struct VersionedRunner {
        version: &'static str,
        calls: RefCell<Vec<String>>,
    }

    impl VersionedRunner {
        fn new(version: &'static str) -> Self {
            Self {
                version,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl NativeCommandRunner for VersionedRunner {
        fn mutation_verified(&self, executable: NativeExecutable) -> bool {
            judge_adapter_version(executable, self.version.as_bytes())
                == AdapterVersionVerdict::Verified
        }

        fn mutation_block_reason(&self, executable: NativeExecutable) -> String {
            judge_adapter_version(executable, self.version.as_bytes()).reason()
        }

        fn run(&self, executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
            self.calls
                .borrow_mut()
                .push(format!("{} {}", executable.command(), args.join(" ")));
            Err("refused by the versioned fake".to_string())
        }

        fn observe(
            &self,
            _executable: NativeExecutable,
            _selector: &NativeSelector,
        ) -> Result<NativeRegistration, String> {
            Ok(NativeRegistration {
                available: true,
                present: false,
                enabled: false,
                broken: false,
                selector: None,
                source: None,
                upstream_revision: None,
                package_digest: None,
                capabilities: CapabilityInventory::new(),
            })
        }

        fn marketplace_root(
            &self,
            _executable: NativeExecutable,
            _marketplace: &str,
        ) -> Result<Option<String>, String> {
            Ok(None)
        }
    }

    fn generation() -> ProviderGeneration {
        ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g1").unwrap(),
            package_digest: PackageDigest::new("sha256:new").unwrap(),
        }
    }

    fn skill() -> CapabilityInventory {
        [ComponentKind::Skill].into_iter().collect()
    }

    fn selector() -> NativeSelector {
        NativeSelector::new("demo@ccync").unwrap()
    }

    fn committed_claude() -> CommittedOwnership {
        CommittedOwnership {
            target: ProjectionTargetId::Claude,
            selector: selector(),
            plugin_name: "demo".to_string(),
            upstream_revision: Some("rev1".to_string()),
            generation: generation(),
            capabilities: skill(),
            client_scope: Some(ClientScope::new("demo-client").unwrap()),
            source_digest: None,
            view_digest: None,
            observed_preimage: None,
            enablement_intent: None,
            group_substep: None,
        }
    }

    #[test]
    fn p7_t12_default_trait_fails_closed() {
        let bare = BareRunner {
            calls: RefCell::new(Vec::new()),
        };
        for executable in NativeExecutable::ALL {
            assert!(!bare.mutation_verified(executable), "{executable:?}");
            assert_eq!(
                bare.mutation_block_reason(executable),
                format!("{} has no verified loader probe", executable.command())
            );
        }
        let (selector, generation, capabilities) = (selector(), generation(), skill());
        let install = NativeInstall {
            selector: &selector,
            marketplace_root: ROOT,
            generation: &generation,
            upstream_revision: Some("rev1"),
            capabilities: &capabilities,
        };
        assert!(matches!(
            install_native_registration(&bare, NativeExecutable::Codex, &install),
            Err(NativeReconcileError::OwnershipEvidenceUnverified(
                NativeExecutable::Codex
            ))
        ));
        assert!(
            bare.calls.borrow().is_empty(),
            "a fail-closed runner makes no CLI call"
        );
    }

    #[test]
    fn p7_t12_codex_version_line_gates_eligibility() {
        let eligible = VersionedRunner::new("codex-cli 0.169.0\n");
        assert!(eligible.mutation_verified(NativeExecutable::Codex));
        let same_line = VersionedRunner::new("codex-cli 0.169.7");
        assert!(same_line.mutation_verified(NativeExecutable::Codex));

        let previous_line = VersionedRunner::new("codex-cli 0.163.0");
        assert!(!previous_line.mutation_verified(NativeExecutable::Codex));

        let ineligible = VersionedRunner::new("codex-cli 0.170.0\n");
        assert!(!ineligible.mutation_verified(NativeExecutable::Codex));
        assert_eq!(
            ineligible.mutation_block_reason(NativeExecutable::Codex),
            "unverified adapter version 0.170.0"
        );
    }

    #[test]
    fn p7_t12_claude_version_line_gates_eligibility() {
        let eligible = VersionedRunner::new("2.1.306 (Claude Code)\n");
        assert!(eligible.mutation_verified(NativeExecutable::Claude));

        let ineligible = VersionedRunner::new("2.2.0 (Claude Code)\n");
        assert!(!ineligible.mutation_verified(NativeExecutable::Claude));
        assert_eq!(
            ineligible.mutation_block_reason(NativeExecutable::Claude),
            "unverified adapter version 2.2.0"
        );
    }

    #[test]
    fn p7_t12_unreadable_or_foreign_versions_are_never_eligible() {
        for (executable, output) in [
            (NativeExecutable::Codex, "not a version"),
            (NativeExecutable::Codex, ""),
            (NativeExecutable::Codex, "2.1.306 (Claude Code)"),
            (NativeExecutable::Claude, "codex-cli 0.169.0"),
            (NativeExecutable::Claude, "2.1 (Claude Code)"),
            (NativeExecutable::Claude, "2.1.x (Claude Code)"),
        ] {
            assert_eq!(
                judge_adapter_version(executable, output.as_bytes()),
                AdapterVersionVerdict::Unreadable,
                "{executable:?} {output:?}"
            );
            let runner = VersionedRunner::new(output);
            assert!(!runner.mutation_verified(executable), "{output:?}");
            assert_eq!(
                runner.mutation_block_reason(executable),
                "unreadable adapter version"
            );
        }
        assert_eq!(
            judge_adapter_version(NativeExecutable::Codex, &[0xff, 0xfe]),
            AdapterVersionVerdict::Unreadable
        );
    }

    #[test]
    fn p7_t12_ineligible_adapter_makes_no_mutating_call_and_keeps_reason() {
        let runner = VersionedRunner::new("codex-cli 0.170.0");
        let (selector, generation, capabilities) = (selector(), generation(), skill());
        let install = NativeInstall {
            selector: &selector,
            marketplace_root: ROOT,
            generation: &generation,
            upstream_revision: Some("rev1"),
            capabilities: &capabilities,
        };
        assert!(matches!(
            install_native_registration(&runner, NativeExecutable::Codex, &install),
            Err(NativeReconcileError::OwnershipEvidenceUnverified(
                NativeExecutable::Codex
            ))
        ));
        assert!(
            runner.calls.borrow().is_empty(),
            "an ineligible adapter must not run any CLI command"
        );
        assert_eq!(
            runner.mutation_block_reason(NativeExecutable::Codex),
            "unverified adapter version 0.170.0"
        );

        let claude = VersionedRunner::new("2.2.0 (Claude Code)");
        assert!(matches!(
            remove_committed_registration(&claude, NativeExecutable::Claude, &committed_claude()),
            Err(NativeReconcileError::OwnershipEvidenceUnverified(
                NativeExecutable::Claude
            ))
        ));
        assert!(
            claude.calls.borrow().is_empty(),
            "an ineligible removal must not run any CLI command"
        );
    }
}

#[cfg(test)]
mod p7_t12_fallback_reason_tests {
    use super::*;
    use crate::delivery::NativeAdapterState;
    use crate::delivery_apply::native_adapter_states;

    /// Adapter whose loader probe reports an unverified CLI version line.
    struct IneligibleCli;

    impl NativeCommandRunner for IneligibleCli {
        fn mutation_verified(&self, _executable: NativeExecutable) -> bool {
            false
        }

        fn mutation_block_reason(&self, _executable: NativeExecutable) -> String {
            "unverified adapter version 0.170.0".to_string()
        }

        fn run(&self, _executable: NativeExecutable, _args: &[&str]) -> Result<Vec<u8>, String> {
            Err("no CLI call is allowed for an ineligible adapter".to_string())
        }

        fn observe(
            &self,
            _executable: NativeExecutable,
            _selector: &NativeSelector,
        ) -> Result<NativeRegistration, String> {
            Err("no CLI call is allowed for an ineligible adapter".to_string())
        }

        fn marketplace_root(
            &self,
            _executable: NativeExecutable,
            _marketplace: &str,
        ) -> Result<Option<String>, String> {
            Err("no CLI call is allowed for an ineligible adapter".to_string())
        }
    }

    #[test]
    fn p7_t12_ineligible_sync_fallback_reports_version_reason() {
        let adapters = native_adapter_states(&IneligibleCli, &[ProjectionTargetId::Codex]);
        match adapters.get(&ProjectionTargetId::Codex) {
            Some(NativeAdapterState::Failed { reason }) => {
                assert!(
                    reason.contains("unverified adapter version"),
                    "fallback reason was {reason:?}"
                );
                assert_eq!(reason, "unverified adapter version 0.170.0");
            }
            other => panic!("expected a failed adapter state, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod p7_t12_verification_tests {
    use super::*;
    use ccync_foundation::provider::{GenerationId, ProviderSourceKey};
    use std::cell::RefCell;

    const ROOT: &str = "C:/ccync/marketplace";

    fn generation() -> ProviderGeneration {
        ProviderGeneration {
            source: ProviderSourceKey::new("demo").unwrap(),
            generation: GenerationId::new("g1").unwrap(),
            package_digest: PackageDigest::new("sha256:new").unwrap(),
        }
    }

    /// Fake Claude CLI whose mutations exit 0 while its listing stays empty.
    #[derive(Default)]
    struct SilentCli {
        calls: RefCell<Vec<String>>,
    }

    impl NativeCommandRunner for SilentCli {
        fn mutation_verified(&self, _executable: NativeExecutable) -> bool {
            true
        }

        fn run(&self, _executable: NativeExecutable, args: &[&str]) -> Result<Vec<u8>, String> {
            self.calls.borrow_mut().push(args.join(" "));
            Ok(Vec::new())
        }

        fn observe(
            &self,
            _executable: NativeExecutable,
            _selector: &NativeSelector,
        ) -> Result<NativeRegistration, String> {
            Ok(NativeRegistration {
                available: true,
                present: false,
                enabled: false,
                broken: false,
                selector: None,
                source: None,
                upstream_revision: None,
                package_digest: None,
                capabilities: CapabilityInventory::new(),
            })
        }

        fn marketplace_root(
            &self,
            _executable: NativeExecutable,
            _marketplace: &str,
        ) -> Result<Option<String>, String> {
            Ok(Some(ROOT.to_string()))
        }
    }

    #[test]
    fn p7_t12_exit_zero_install_without_listed_registration_is_not_success() {
        let cli = SilentCli::default();
        let selector = NativeSelector::new("demo@ccync").unwrap();
        let generation = generation();
        let capabilities = CapabilityInventory::new();
        let install = NativeInstall {
            selector: &selector,
            marketplace_root: ROOT,
            generation: &generation,
            upstream_revision: Some("rev1"),
            capabilities: &capabilities,
        };

        let result = install_native_registration(&cli, NativeExecutable::Claude, &install);

        assert!(
            matches!(
                result,
                Err(NativeReconcileError::Verification(
                    NativeObservation::Absent
                ))
            ),
            "exit 0 with an unlisted registration must fail verification, got {result:?}"
        );
        assert_eq!(
            *cli.calls.borrow(),
            ["plugin install demo@ccync --scope user"],
            "exactly one add was attempted"
        );
    }
}
