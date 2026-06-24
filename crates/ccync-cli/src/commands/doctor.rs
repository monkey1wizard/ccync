//! Doctor command — aggregates ccync management health checks
//! (MCP projection, machine setup, Claude plugin cache, skills projection).

use ccync_engine::ExitCode;
use ccync_foundation::health::HealthCheck;
use std::path::PathBuf;

/// Run `ccync doctor [--dry-run] [--release-gate]` — read-only health checks.
pub(crate) fn cmd_doctor(args: &[String]) -> ExitCode {
    use ccync_engine::doctor::{run_doctor, DoctorOptions};

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
    if let Some(canonical_root) = ccync_foundation::paths::ccync_plugin_root("ccync") {
        report
            .findings
            .extend(setup::health::ClaudePluginCacheCheck::from_user_home(canonical_root).check());
    }
    if let Some(home) = ccync_foundation::paths::user_home() {
        let shared_skills_root = home.join(".agents").join("skills");
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
