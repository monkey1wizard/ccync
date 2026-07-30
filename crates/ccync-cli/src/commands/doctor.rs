//! Doctor command — aggregates ccync management health checks
//! (MCP projection, machine setup, Claude plugin cache, skills projection).

use ccync_engine::ExitCode;
use ccync_foundation::health::HealthCheck;
use std::path::PathBuf;

/// Run `ccync doctor [--dry-run] [--release-gate]` — read-only health checks.
pub(crate) fn cmd_doctor(args: &[String]) -> ExitCode {
    use ccync_engine::doctor::{run_doctor, DoctorOptions};

    if let Err(code) = super::args::reject_unknown_flags(
        "doctor",
        args,
        &["--dry-run", "--release-gate"],
        &[],
        0,
        "Usage: ccync doctor [--dry-run] [--release-gate]",
    ) {
        return code;
    }

    let dry_run = args.iter().any(|a| a == "--dry-run");
    let release_gate = args.iter().any(|a| a == "--release-gate");

    let opts = DoctorOptions {
        dry_run,
        release_gate,
    };
    let mut report = run_doctor(&opts);

    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(check) = mcp::McpProjectionHealthCheck::from_standard_path() {
        report.findings.extend(check.check());
    }
    report
        .findings
        .extend(setup::health::SetupHealthCheck { repo_root }.check());
    if let Some(canonical_root) = ccync_foundation::paths::canonical_plugin_root() {
        report
            .findings
            .extend(setup::health::ClaudePluginCacheCheck::from_user_home(canonical_root).check());
    }
    if let Some(shared_skills_root) = ccync_foundation::paths::shared_agents_skills_path() {
        report
            .findings
            .extend(projection::SkillsProjectionHealthCheck::with_path(shared_skills_root).check());
    }

    if report.findings.is_empty() {
        println!("ccync doctor: all checks passed.");
    } else {
        for f in &report.findings {
            println!("{f}");
        }
    }

    if report.has_errors() {
        ExitCode::Error
    } else {
        ExitCode::Success
    }
}

#[cfg(test)]
mod tests {
    use ccync_foundation::health::HealthCheck;
    use setup::health::ClaudePluginCacheCheck;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    /// Proves `ClaudePluginCacheCheck` (as wired into `cmd_doctor` against the real
    /// canonical root) is no longer a permanent no-op: a `canonical_root` that is
    /// newer than `cache_dir` must produce a stale-cache warning.
    ///
    /// Uses real elapsed-time ordering (create cache_dir, sleep, then create
    /// canonical_root) rather than forcing directory mtimes via `set_modified`,
    /// which is permission-restricted on Windows for directory handles.
    #[test]
    fn claude_plugin_cache_check_flags_stale_cache_against_fake_canonical_root() {
        let temp = tempfile::tempdir().unwrap();

        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();

        // Ensure a filesystem-observable mtime gap regardless of timestamp resolution.
        thread::sleep(Duration::from_millis(2100));

        let canonical_root = temp.path().join("canonical");
        fs::create_dir_all(&canonical_root).unwrap();

        let check = ClaudePluginCacheCheck {
            canonical_root,
            cache_dir: Some(cache_dir),
        };
        let findings = check.check();

        assert!(
            findings.iter().any(|f| f.message.contains("stale")),
            "intentionally-stale cache_dir vs. newer canonical_root must produce a \
             stale-cache warning — the check is not a permanent no-op: {findings:?}"
        );
    }
}
