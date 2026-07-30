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
/// 1. Runs the one-shot layout migration (best-effort).
/// 2. Loads config.
/// 3. Delegates to `run_projection_core` — canonical render + machine
///    projection + MCP projection — without any first-run gate or catalog
///    resolve (those belong to the public `sync` lifecycle).
///
/// Idempotent: calling twice with the same state produces byte-identical
/// derived outputs.
pub(crate) fn refresh_with() -> Result<(), String> {
    // Layout migration before any state consumption (best-effort).
    if let Some(home) = ccync_foundation::paths::user_home() {
        if let Err(e) = crate::commands::lifecycle::run_layout_migration(&home) {
            eprintln!("ccync refresh: layout migration (best-effort): {e}");
        }
    }

    let config = ccync_foundation::config::CcyncConfig::load();
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
        let orig_home;
        #[cfg(windows)]
        {
            orig_home = std::env::var("USERPROFILE").ok();
            unsafe {
                std::env::set_var("USERPROFILE", tmp.path());
                std::env::set_var("HOME", tmp.path());
            }
        }
        #[cfg(not(windows))]
        {
            orig_home = std::env::var("HOME").ok();
            unsafe {
                std::env::set_var("HOME", tmp.path());
            }
        }

        let render_dir = tmp.path().join(".ccync").join("build").join("render");

        // First run — must succeed and write the render dir.
        let r1 = refresh_with();

        // Capture render dir contents after first run.
        let first_contents = dir_snapshot(&render_dir);

        // Second run — must succeed (idempotent).
        let r2 = refresh_with();

        // Capture render dir contents after second run.
        let second_contents = dir_snapshot(&render_dir);

        #[cfg(windows)]
        unsafe {
            match &orig_home {
                Some(v) => {
                    std::env::set_var("USERPROFILE", v);
                    std::env::set_var("HOME", v);
                }
                None => {
                    std::env::remove_var("USERPROFILE");
                    std::env::remove_var("HOME");
                }
            }
        }
        #[cfg(not(windows))]
        unsafe {
            match &orig_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

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
