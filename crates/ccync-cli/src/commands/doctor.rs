//! Doctor command — aggregates ccync management health checks
//! (MCP projection, machine setup, Claude plugin cache, skills projection).

use ccync_engine::ExitCode;
use ccync_foundation::health::{DoctorFinding, HealthCheck};
use std::path::PathBuf;

/// Run `ccync doctor [--dry-run] [--release-gate]` — read-only health checks.
pub(crate) fn cmd_doctor(args: &[String]) -> ExitCode {
    use ccync_engine::doctor::{run_doctor, DoctorOptions};

    if let Err(code) = super::args::reject_unknown_flags(
        "doctor",
        args,
        &["--dry-run", "--release-gate"],
        &["--mcp-evidence"],
        0,
        "Usage: ccync doctor [--dry-run] [--release-gate] [--mcp-evidence <path>]",
    ) {
        return code;
    }

    let mut evidence_path = None;
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        let value = if arg == "--mcp-evidence" {
            i += 1;
            args.get(i).map(String::as_str)
        } else {
            arg.strip_prefix("--mcp-evidence=")
        };
        if arg == "--mcp-evidence" || arg.starts_with("--mcp-evidence=") {
            match value {
                Some(path)
                    if !path.is_empty() && !path.starts_with('-') && evidence_path.is_none() =>
                {
                    evidence_path = Some(path);
                }
                _ => {
                    eprintln!("ccync doctor: --mcp-evidence requires one path");
                    return ExitCode::Usage;
                }
            }
        }
        i += 1;
    }
    if let Some(path) = evidence_path {
        return report_mcp_evidence(path);
    }

    if let Err(error) = ccync_engine::tool_manager::lock::check_restore_pending() {
        eprintln!("ccync doctor: {error}");
        return ExitCode::Error;
    }

    let layout = match ccync_foundation::paths::EffectiveLayout::load_or_legacy() {
        Ok(layout) => layout,
        Err(error) => {
            eprintln!("ccync doctor: cannot load storage layout: {error}");
            return ExitCode::Error;
        }
    };

    let dry_run = args.iter().any(|a| a == "--dry-run");
    let release_gate = args.iter().any(|a| a == "--release-gate");

    let opts = DoctorOptions {
        dry_run,
        release_gate,
    };
    let mut report = run_doctor(&opts);

    match super::lifecycle::current_mcp_decisions(&layout) {
        Ok(decisions) => {
            let (lines, blocked) = super::lifecycle::mcp_decision_lines(&decisions);
            for line in lines {
                println!("{line}");
            }
            if blocked {
                report.findings.push(DoctorFinding::error(
                    "MCP declarations blocked",
                    "resolve the reported MCP entries and run ccync sync",
                ));
            }
        }
        Err(error) => report.findings.push(DoctorFinding::error(
            format!("MCP inspection blocked: {error}"),
            "run ccync sync after resolving the reported issue",
        )),
    }

    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let check = mcp::McpProjectionHealthCheck::with_paths(
        layout.generated_mcp_path(),
        layout.machine_config_path(),
    );
    report.findings.extend(check.check());
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
    let config =
        ccync_foundation::config::CcyncConfig::load_strict_from_path(&layout.machine_config_path());
    report
        .findings
        .extend(instruction_findings(layout.agents_md_path(), config));
    report.findings.extend(legacy_instruction_findings());

    if report.findings.is_empty() {
        println!("ccync doctor: all checks passed.");
    } else {
        for f in &report.findings {
            println!("{f}");
        }
    }
    // Route facts are informational: each line carries its own state and
    // action, so they never add a finding or change the exit code. An
    // unreadable record is already reported as a finding above.
    for line in ccync_engine::doctor::route_evidence_lines().unwrap_or_default() {
        println!("{}", super::lifecycle::label_standalone_route(line));
    }

    if report.has_errors() {
        ExitCode::Error
    } else {
        ExitCode::Success
    }
}

fn report_mcp_evidence(path: &str) -> ExitCode {
    use std::io::Read;

    const MAX_EVIDENCE_BYTES: u64 = 256 * 1024;
    let mut bytes = Vec::new();
    let result = std::fs::File::open(path)
        .and_then(|file| file.take(MAX_EVIDENCE_BYTES + 1).read_to_end(&mut bytes));
    match result {
        Ok(_) => {}
        Err(error) => {
            eprintln!("ccync doctor: cannot read MCP evidence: {error}");
            return ExitCode::Error;
        }
    }
    if bytes.len() as u64 > MAX_EVIDENCE_BYTES {
        eprintln!("ccync doctor: MCP evidence exceeds the 256 KiB limit");
        return ExitCode::Error;
    }
    let report = match mcp::analyze_mcp_evidence(&bytes) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("ccync doctor: {error}");
            return ExitCode::Error;
        }
    };
    println!("MCP evidence input: unproven; run: {}", report.run());
    if let Some(first) = report.attempts().first() {
        println!(
            "host: {} version {}; session: {}; failing item: {}; operation: {}",
            first.host(),
            first.host_version(),
            first.session(),
            first.server(),
            first.operation()
        );
    } else {
        println!("host: unknown; failing item: unknown");
    }
    println!("observed attempts: {}", report.attempts().len());
    for attempt in report.attempts() {
        println!(
            "attempt {}: initiator {:?}; trigger {:?}",
            attempt.attempt(),
            attempt.initiator(),
            attempt.trigger()
        );
    }
    println!("stop state: {:?}", report.stop_state());
    println!("experiments: not established by this event format");
    for attempt in report.attempts() {
        println!(
            "attempt {}: event IDs {:?}; sources {:?}; readers {:?}",
            attempt.attempt(),
            attempt.events(),
            attempt.sources(),
            attempt.readers()
        );
    }
    if report.gaps().is_empty() {
        println!("gaps: none reported");
    } else {
        for gap in report.gaps() {
            println!("gap: {gap}");
        }
    }
    ExitCode::Success
}

/// Evaluate user-scope instruction health. A config that cannot be loaded
/// strictly, or whose target selection is rejected, yields errors only and
/// skips health evaluation so no verdict is built on a guessed selection.
fn instruction_findings(
    input_path: PathBuf,
    config: Result<
        ccync_foundation::config::CcyncConfig,
        ccync_foundation::config::CcyncConfigLoadError,
    >,
) -> Vec<DoctorFinding> {
    use ccync_foundation::config::LegacyState;

    let config = match config {
        Ok(config) => config,
        Err(error) => {
            return vec![DoctorFinding::error(
                format!("instruction config rejected: {error}"),
                "fix or create ~/.ccync/config.json with `ccync init <master>`",
            )];
        }
    };
    let resolution = config.resolve_projection_targets();
    if resolution.state == LegacyState::Both {
        return vec![DoctorFinding::error(
            "instruction config rejected: both selectedRuntimes and selectedProjectionTargets are set",
            "run `ccync init <master>` to choose one selection",
        )];
    }
    if !resolution.rejected.is_empty() {
        return vec![DoctorFinding::error(
            format!(
                "instruction config rejected: rejected target selection value(s): {}",
                resolution.rejected.join(", ")
            ),
            "remove the rejected values or run `ccync init <master>` to select valid targets",
        )];
    }

    let selected = resolution.targets;
    let targets = ccync_foundation::target::CANONICAL_PROJECTION_TARGETS
        .iter()
        .filter_map(|target| {
            ccync_foundation::paths::user_scope_instruction_write_path(*target).map(|path| {
                projection::managed_block::TargetResolution {
                    target: *target,
                    path,
                    selected: selected.contains(target),
                }
            })
        })
        .collect();
    projection::managed_block::ManagedBlockHealthCheck::new(input_path, targets).check()
}

/// Diagnose the retired Claude instruction file independently of config health.
fn legacy_instruction_findings() -> Vec<DoctorFinding> {
    use ccync_foundation::health::Severity;
    use projection::managed_block::{classify, BlockClassification};

    let Some(path) = ccync_foundation::paths::legacy_claude_user_instruction_path() else {
        return Vec::new();
    };
    let recovery = "Run `ccync sync --instructions-only --yes` when Claude is selected. Otherwise remove the content between the two ccync markers, including the markers, by hand, or run `ccync uninstall`.";
    match classify(&path, None) {
        BlockClassification::Prune => vec![DoctorFinding {
            severity: Severity::Warning,
            message: format!("legacy Claude managed block remains: {}", path.display()),
            fix_hint: Some(recovery.to_string()),
        }],
        BlockClassification::TargetRead => vec![DoctorFinding::error(
            format!(
                "legacy Claude instruction target cannot be read: {}",
                path.display()
            ),
            "check the target file permissions",
        )],
        BlockClassification::MalformedMarker => vec![DoctorFinding::error(
            format!(
                "legacy Claude managed block is malformed: {}",
                path.display()
            ),
            "repair or remove the ccync markers in the target file",
        )],
        BlockClassification::DanglingLink => vec![DoctorFinding::error(
            format!(
                "legacy Claude instruction target is a dangling symbolic link: {}",
                path.display()
            ),
            "preserve the existing symbolic link, manually inspect its missing target and access, restore access, then rerun `ccync doctor`",
        )],
        BlockClassification::Create
        | BlockClassification::Append
        | BlockClassification::Rewrite
        | BlockClassification::Unchanged => Vec::new(),
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
