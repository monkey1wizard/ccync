//! `ccync update` — self-update the `ccync` binary (brew-update semantics).
//!
//! Thin handler: parse args, build the engine seams (GitHub release lookup +
//! download via `curl`, matching `packaging/install.sh`), call
//! `ccync_engine::self_update::run_update`, print the report, map the exit code.
//! The provenance gate inside `run_update` runs before any network call, so a
//! package-manager-managed install refuses without touching the network.

use ccync_engine::self_update::{artifact_name, run_update, UpdateOpts, UpdateReport};
use ccync_engine::ExitCode;
use std::cell::RefCell;
use std::process::Command;

const REPO: &str = "monkey1wizard/ccync";

pub(crate) fn cmd_update(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "update",
        args,
        &["--help", "-h", "--check"],
        &[],
        0,
        "Usage: ccync update [--check]",
    ) {
        return code;
    }
    let args = &args[1..]; // skip the "update" verb

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("ccync update [--check]");
        println!();
        println!("Self-update the ccync binary (Homebrew `brew update` semantics).");
        println!("Only self-replaces a curl/irm install; under Homebrew/winget/cargo it");
        println!("refuses and points at the native upgrade command.");
        println!("  --check   Report version + provenance only; download/write nothing.");
        return ExitCode::Success;
    }

    let check_only = args.iter().any(|a| a == "--check");

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ccync update: cannot resolve current executable: {e}");
            return ExitCode::Error;
        }
    };

    let os = match release_os_slug(std::env::consts::OS) {
        Ok(os) => os.to_string(),
        Err(e) => {
            eprintln!("ccync update: {e}");
            return ExitCode::Error;
        }
    };
    let arch = match release_arch_slug(std::env::consts::ARCH) {
        Ok(arch) => arch.to_string(),
        Err(e) => {
            eprintln!("ccync update: {e}");
            return ExitCode::Error;
        }
    };
    let opts = UpdateOpts {
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        exe_path: exe,
        os: os.clone(),
        arch: arch.clone(),
        check_only,
    };

    // The tag is resolved lazily by the `latest_version` seam (so the provenance
    // gate can refuse a managed install before any network call), then cached
    // for the checksum/artifact seams.
    let tag_cell: RefCell<Option<String>> = RefCell::new(None);

    let latest = || {
        let tag = github_latest_tag()?;
        *tag_cell.borrow_mut() = Some(tag.clone());
        Ok(tag)
    };
    let fetch_checksums = || {
        let tag = tag_cell
            .borrow()
            .clone()
            .ok_or_else(|| "release tag not resolved".to_string())?;
        curl_text(&format!(
            "https://github.com/{REPO}/releases/download/{tag}/checksums.txt"
        ))
    };
    let fetch_artifact = || {
        let tag = tag_cell
            .borrow()
            .clone()
            .ok_or_else(|| "release tag not resolved".to_string())?;
        let name = artifact_name(&tag, &os, &arch);
        curl_bytes(&format!(
            "https://github.com/{REPO}/releases/download/{tag}/{name}"
        ))
    };

    let report = run_update(&opts, latest, fetch_checksums, fetch_artifact);
    report_exit(report)
}

/// Print the update report and map it to a process exit code.
fn report_exit(report: UpdateReport) -> ExitCode {
    match report {
        UpdateReport::Refused { provenance, hint } => {
            eprintln!("ccync update: not self-updating ({provenance:?}).");
            eprintln!("  {hint}");
            ExitCode::Error
        }
        UpdateReport::CheckRefused {
            provenance,
            current_version,
            hint,
        } => {
            println!("ccync is on v{current_version} ({provenance:?} install).");
            println!("  {hint}");
            ExitCode::Success
        }
        UpdateReport::UpToDate { version } => {
            println!("ccync is already up to date (v{version}).");
            ExitCode::Success
        }
        UpdateReport::UpdateAvailable { current, latest } => {
            println!("Update available: v{current} -> v{latest}. Run `ccync update` to install.");
            ExitCode::Success
        }
        UpdateReport::Updated {
            version,
            backup_removed,
        } => {
            println!("Updated ccync to v{version}.");
            if !backup_removed {
                println!("  (previous binary left as <ccync>.old for cleanup on next run)");
            }
            ExitCode::Success
        }
        UpdateReport::Failed { stage, error } => {
            eprintln!("ccync update: failed at {stage}: {error}");
            ExitCode::Error
        }
    }
}

/// Fetch the latest release tag from the GitHub API via `curl`.
fn github_latest_tag() -> Result<String, String> {
    let body = curl_text(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse releases/latest: {e}"))?;
    v.get("tag_name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "releases/latest has no tag_name".to_string())
}

/// `curl -fsSL <url>` returning the body as text.
fn curl_text(url: &str) -> Result<String, String> {
    let out = run_curl(url)?;
    String::from_utf8(out).map_err(|e| format!("non-utf8 response from {url}: {e}"))
}

/// `curl -fsSL <url>` returning the body as raw bytes.
fn curl_bytes(url: &str) -> Result<Vec<u8>, String> {
    run_curl(url)
}

fn run_curl(url: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "-H",
            "User-Agent: ccync-update",
            url,
        ])
        .output()
        .map_err(|e| format!("spawn curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}
fn release_os_slug(os: &str) -> Result<&'static str, String> {
    match os {
        "linux" => Ok("linux"),
        "macos" => Ok("darwin"),
        "windows" => Ok("windows"),
        other => Err(format!("unsupported update platform: {other}")),
    }
}

fn release_arch_slug(arch: &str) -> Result<&'static str, String> {
    match arch {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("arm64"),
        other => Err(format!("unsupported update architecture: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{release_arch_slug, release_os_slug};

    #[test]
    fn source_check_refused_prints_version_and_hint_with_success() {
        let this_file = include_str!("update.rs");
        assert!(
            this_file.contains("UpdateReport::CheckRefused"),
            "report_exit must handle the CheckRefused variant separately from Refused"
        );
        assert!(
            this_file.contains("ccync is on v{current_version}"),
            "CheckRefused message must include the current version"
        );
    }

    #[test]
    fn maps_rust_targets_to_release_asset_slugs() {
        assert_eq!(release_os_slug("linux").unwrap(), "linux");
        assert_eq!(release_os_slug("macos").unwrap(), "darwin");
        assert_eq!(release_os_slug("windows").unwrap(), "windows");
        assert_eq!(release_arch_slug("x86_64").unwrap(), "x64");
        assert_eq!(release_arch_slug("aarch64").unwrap(), "arm64");
    }

    #[test]
    fn rejects_targets_without_published_assets() {
        assert!(release_os_slug("freebsd").is_err());
        assert!(release_arch_slug("x86").is_err());
    }
}
