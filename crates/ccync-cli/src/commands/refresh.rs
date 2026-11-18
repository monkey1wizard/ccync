//! `ccync refresh` — internal/dev derived-layer rebuild.
//!
//! Reuses the shared projection core from `lifecycle` to materialize
//! changes to local machine state. Idempotent: byte-identical re-runs produce
//! no observable difference. Not in public `--help`.

use ccync_engine::ExitCode;

/// `ccync refresh`
///
/// Parses `--help`. Calls `refresh_with`.
pub(crate) fn cmd_refresh(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "refresh",
        args,
        &["--help", "-h"],
        &[],
        0,
        "Usage: ccync refresh",
    ) {
        return code;
    }
    let args = &args[1..]; // skip "refresh" verb

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("ccync refresh");
        println!();
        println!("Internal/dev: rebuild derived layer from ~/.ccync state without running full sync lifecycle.");
        return ExitCode::Success;
    }

    match refresh_with() {
        Ok(()) => ExitCode::Success,
        Err(e) => {
            eprintln!("ccync refresh: {e}");
            ExitCode::Error
        }
    }
}

/// Testable seam: rebuild derived layer using the shared projection core.
///
/// The rebuild reads `~/.ccync` state directly. The function:
/// 1. Applies the older sublayout fixup while the caller holds lifecycle locks.
/// 2. Loads config from the validated effective layout.
/// 3. Delegates to `run_projection_core` — canonical render + machine
///    projection + MCP projection — without any first-run gate or catalog
///    resolve (those belong to the public `sync` lifecycle).
///
/// Idempotent: calling twice with the same state produces byte-identical
/// derived outputs.
pub(crate) fn refresh_with() -> Result<(), String> {
    let layout = ccync_foundation::paths::EffectiveLayout::load_or_legacy()
        .map_err(|error| format!("cannot load effective storage layout: {error}"))?;
    if layout.authority() != ccync_foundation::paths::EffectiveLayoutAuthority::Committed {
        if layout.authority() == ccync_foundation::paths::EffectiveLayoutAuthority::Legacy {
            eprintln!("ccync refresh: legacy storage layout detected; run `ccync sync` to migrate");
        }
        if let Some(home) = ccync_foundation::paths::user_home() {
            crate::commands::lifecycle::run_layout_migration(&home)
                .map_err(|error| format!("older sublayout fixup failed: {error}"))?;
        }
    }

    let config =
        ccync_foundation::config::CcyncConfig::load_from_path(&layout.machine_config_path());
    let result = crate::commands::lifecycle::run_projection_core(&config, "ccync refresh");
    match result {
        ExitCode::Success => Ok(()),
        _ => Err("projection failed".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_with_writes_canonical_and_is_idempotent() {
        let _guard = crate::test_io_lock();

        let tmp = tempfile::TempDir::new().unwrap();
        let _env = IsolatedHome::new(tmp.path());

        let render_dir = tmp.path().join(".ccync").join("build").join("render");

        // First run — must succeed and write the render dir.
        let r1 = refresh_with();

        // Capture render dir contents after first run.
        let first_contents = dir_snapshot(&render_dir);

        // Second run — must succeed (idempotent).
        let r2 = refresh_with();

        // Capture render dir contents after second run.
        let second_contents = dir_snapshot(&render_dir);

        assert!(r1.is_ok(), "refresh_with first run must succeed: {:?}", r1);
        assert!(r2.is_ok(), "refresh_with second run must succeed: {:?}", r2);
        assert!(
            render_dir.is_dir(),
            "canonical render dir must exist after refresh"
        );
        assert_eq!(
            first_contents, second_contents,
            "refresh_with must be idempotent: render dir contents differ between runs"
        );
    }

    struct IsolatedHome(Vec<(String, Option<String>)>);

    impl IsolatedHome {
        fn new(home: &std::path::Path) -> Self {
            let keys = [
                "USERPROFILE",
                "HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ];
            let mut saved = Vec::new();
            for key in keys {
                saved.push((key.to_string(), std::env::var(key).ok()));
                let value = match key {
                    "USERPROFILE" | "HOME" => home.to_path_buf(),
                    "APPDATA" => home.join("AppData").join("Roaming"),
                    "LOCALAPPDATA" => home.join("AppData").join("Local"),
                    "XDG_CONFIG_HOME" => home.join(".config"),
                    "XDG_STATE_HOME" => home.join(".local").join("state"),
                    "XDG_CACHE_HOME" => home.join(".cache"),
                    _ => unreachable!("unexpected home environment key"),
                };
                unsafe { std::env::set_var(key, value) };
            }
            Self(saved)
        }
    }

    impl Drop for IsolatedHome {
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

    /// Collect a sorted snapshot of (relative_path, file_size) pairs under `dir`.
    fn dir_snapshot(dir: &std::path::Path) -> Vec<(String, u64)> {
        if !dir.is_dir() {
            return vec![];
        }
        let mut entries = Vec::new();
        collect_dir(dir, dir, &mut entries);
        entries.sort();
        entries
    }

    fn collect_dir(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if path.is_file() {
                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                out.push((rel, size));
            } else if path.is_dir() {
                collect_dir(base, &path, out);
            }
        }
    }
}
