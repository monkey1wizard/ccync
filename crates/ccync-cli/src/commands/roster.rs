//! `ccync roster` subcommand handler.
//!
//! Subcommands:
//! - `ccync roster dump [--out <path>]`
//! - `ccync roster install [--in <path>] [--dry-run] [--no-sync] [--yes]`

use std::path::PathBuf;

use ccync_engine::ExitCode;

pub(crate) const ROSTER_USAGE: &str =
    "Usage: ccync roster dump [--out <path>] | ccync roster install [--in <path>] [--dry-run] [--no-sync] [--yes]";

pub(crate) fn cmd_roster(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "roster",
        args,
        &["--dry-run", "--no-sync", "--yes"],
        &["--out", "--in"],
        1,
        ROSTER_USAGE,
    ) {
        return code;
    }

    match args.get(1).map(String::as_str) {
        Some("dump") => {
            if let Err(code) = super::args::reject_unknown_flags(
                "roster dump",
                args,
                &[],
                &["--out"],
                1,
                ROSTER_USAGE,
            ) {
                return code;
            }
            cmd_roster_dump(args)
        }
        Some("install") => {
            if let Err(code) = super::args::reject_unknown_flags(
                "roster install",
                args,
                &["--dry-run", "--no-sync", "--yes"],
                &["--in"],
                1,
                ROSTER_USAGE,
            ) {
                return code;
            }
            cmd_roster_install(args)
        }
        _ => {
            eprintln!("{ROSTER_USAGE}");
            ExitCode::Usage
        }
    }
}

fn cmd_roster_dump(args: &[String]) -> ExitCode {
    let mut out_path = PathBuf::from("./ccync.roster.json");
    let mut i = 1usize;
    while i < args.len() {
        let arg = &args[i];
        if let Some(val) = arg.strip_prefix("--out=") {
            out_path = PathBuf::from(val);
        } else if arg == "--out" {
            i += 1;
            if let Some(val) = args.get(i) {
                out_path = PathBuf::from(val);
            } else {
                eprintln!("ccync roster dump: --out requires a path");
                return ExitCode::Usage;
            }
        }
        i += 1;
    }

    let catalog_path = match ccync_foundation::paths::personal_catalog_path() {
        Some(path) => path,
        None => {
            eprintln!("ccync roster dump: cannot determine home directory");
            return ExitCode::Error;
        }
    };

    let catalog = ccync_engine::install::read_personal_catalog(&catalog_path)
        .unwrap_or_else(|| serde_json::json!({ "schemaVersion": 1, "plugins": [] }));

    match ccync_engine::roster::dump_roster(&catalog, &out_path) {
        Ok(report) => {
            for (id, reason) in &report.skipped {
                eprintln!("ccync roster dump: skipped '{id}': {reason}");
            }
            println!(
                "Wrote {} entries to {} (skipped {}).",
                report.written.len(),
                out_path.display(),
                report.skipped.len()
            );
            ExitCode::Success
        }
        Err(e) => {
            eprintln!("ccync roster dump: {e}");
            ExitCode::Error
        }
    }
}

fn cmd_roster_install(args: &[String]) -> ExitCode {
    let mut in_path = PathBuf::from("./ccync.roster.json");
    let mut dry_run = false;

    let mut i = 1usize;
    while i < args.len() {
        let arg = &args[i];
        if let Some(val) = arg.strip_prefix("--in=") {
            in_path = PathBuf::from(val);
        } else if arg == "--in" {
            i += 1;
            if let Some(val) = args.get(i) {
                in_path = PathBuf::from(val);
            } else {
                eprintln!("ccync roster install: --in requires a path");
                return ExitCode::Usage;
            }
        } else if arg == "--dry-run" {
            dry_run = true;
        }
        i += 1;
    }

    let no_sync = args.iter().any(|a| a == "--no-sync");
    let assume_yes = args.iter().any(|a| a == "--yes");

    if !in_path.exists() {
        let resolved = match std::fs::canonicalize(&in_path) {
            Ok(p) => p,
            Err(_) => match std::env::current_dir() {
                Ok(cwd) => cwd.join(&in_path),
                Err(_) => in_path.clone(),
            },
        };
        eprintln!(
            "ccync roster install: roster not found at {}",
            resolved.display()
        );
        return ExitCode::Error;
    }

    let roster_content = match std::fs::read_to_string(&in_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "ccync roster install: failed to read {}: {e}",
                in_path.display()
            );
            return ExitCode::Error;
        }
    };

    let roster_json: serde_json::Value = match serde_json::from_str(&roster_content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "ccync roster install: failed to parse JSON in {}: {e}",
                in_path.display()
            );
            return ExitCode::Error;
        }
    };

    let entries = match ccync_engine::roster::parse_roster(&roster_json) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ccync roster install: {e}");
            return ExitCode::Error;
        }
    };

    let catalog_path = match ccync_foundation::paths::personal_catalog_path() {
        Some(p) => p,
        None => {
            eprintln!("ccync roster install: cannot determine home directory");
            return ExitCode::Error;
        }
    };

    let local_catalog = ccync_engine::install::read_personal_catalog(&catalog_path)
        .unwrap_or_else(|| serde_json::json!({ "schemaVersion": 1, "plugins": [] }));

    let local_plugins = local_catalog
        .get("plugins")
        .and_then(|v| v.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    if dry_run {
        let mut converged_count = 0usize;
        let mut held_count = 0usize;
        let mut would_change_count = 0usize;
        let mut rejected_count = 0usize;

        for entry in &entries {
            let local_entry = local_plugins.iter().find(|p| {
                p.get("pluginId").and_then(|id| id.as_str()) == Some(entry.plugin_id.as_str())
            });

            let classification = ccync_engine::roster::classify_entry(entry, local_entry);
            match classification {
                ccync_engine::roster::Classification::Rejected(reason) => {
                    rejected_count += 1;
                    println!("rejected '{}': {reason}", entry.plugin_id);
                }
                ccync_engine::roster::Classification::Missing => {
                    would_change_count += 1;
                    println!(
                        "missing '{}' (would fetch {} @ {})",
                        entry.plugin_id, entry.source, entry.pin
                    );
                }
                ccync_engine::roster::Classification::PinMismatched { current_sha } => {
                    would_change_count += 1;
                    let current = current_sha.as_deref().unwrap_or("(unpinned)");
                    println!(
                        "pin mismatch '{}': {current} -> {}",
                        entry.plugin_id, entry.pin
                    );
                }
                ccync_engine::roster::Classification::SourceMismatched {
                    current_source, ..
                } => {
                    would_change_count += 1;
                    println!(
                        "source mismatch '{}': {current_source} -> {}",
                        entry.plugin_id, entry.source
                    );
                }
                ccync_engine::roster::Classification::Held {
                    current_sha,
                    current_source,
                    diverged,
                } => {
                    held_count += 1;
                    if diverged {
                        let current = current_sha.as_deref().unwrap_or("(unpinned)");
                        let mut line = format!(
                            "held '{}': current {current} vs roster {}",
                            entry.plugin_id, entry.pin
                        );
                        if let Some(src) = current_source.as_deref() {
                            if src != entry.source {
                                line.push_str(&format!("; source {src} vs {}", entry.source));
                            }
                        }
                        line.push_str(&format!(
                            " — release with ccync pin --remove {}",
                            entry.plugin_id
                        ));
                        println!("{line}");
                    } else {
                        println!("held '{}': at roster pin (held, current)", entry.plugin_id);
                    }
                }
                ccync_engine::roster::Classification::Converged => {
                    converged_count += 1;
                }
            }
        }

        println!(
            "roster: {converged_count} converged, {held_count} held, {would_change_count} would change, {rejected_count} rejected"
        );

        if rejected_count > 0 {
            ExitCode::Error
        } else if would_change_count > 0 {
            ExitCode::UpdatesAvailable
        } else {
            ExitCode::Success
        }
    } else {
        let mut classified_entries = Vec::new();
        let mut has_rejected = false;

        for entry in &entries {
            let local_entry = local_plugins.iter().find(|p| {
                p.get("pluginId").and_then(|id| id.as_str()) == Some(entry.plugin_id.as_str())
            });

            let classification = ccync_engine::roster::classify_entry(entry, local_entry);
            if let ccync_engine::roster::Classification::Rejected(reason) = &classification {
                has_rejected = true;
                println!("rejected '{}': {reason}", entry.plugin_id);
            }
            classified_entries.push(ccync_engine::roster::ClassifiedEntry {
                entry: entry.clone(),
                class: classification,
            });
        }

        if has_rejected {
            return ExitCode::Error;
        }

        if !no_sync {
            if let Err(code) = super::lifecycle::require_initialized_machine() {
                return code;
            }
        }

        let cache_root = match ccync_foundation::paths::ccync_cache_root() {
            Some(p) => p,
            None => {
                eprintln!("ccync roster install: cannot determine cache directory");
                return ExitCode::Error;
            }
        };

        let pin_files: Vec<PathBuf> = [
            ccync_foundation::paths::plugins_lock_path(),
            ccync_foundation::paths::personal_catalog_path(),
        ]
        .into_iter()
        .flatten()
        .filter(|p| p.exists())
        .collect();

        let opts = ccync_engine::roster::InstallOptions {
            cache_root,
            catalog_path,
            pin_files,
        };

        let report = ccync_engine::roster::execute_install(&classified_entries, &opts);

        for (id, url) in &report.fetched {
            println!("  Fetched '{id}' from {url}");
        }

        for (id, old, new) in &report.source_rewrites {
            println!("  Rewrote source for '{id}': {old} -> {new}");
        }

        for (id, reason) in &report.failed {
            eprintln!("ccync roster install: '{id}' failed: {reason}");
        }

        for classified in &classified_entries {
            if let ccync_engine::roster::Classification::Held {
                current_sha,
                current_source,
                diverged: true,
            } = &classified.class
            {
                let current = current_sha.as_deref().unwrap_or("(unpinned)");
                let mut line = format!(
                    "held '{}': current {current} vs roster {}",
                    classified.entry.plugin_id, classified.entry.pin
                );
                if let Some(src) = current_source.as_deref() {
                    if src != classified.entry.source {
                        line.push_str(&format!("; source {src} vs {}", classified.entry.source));
                    }
                }
                line.push_str(&format!(
                    " — release with ccync pin --remove {}",
                    classified.entry.plugin_id
                ));
                println!("{line}");
            }
        }

        if let Err(e) = super::plugin::run_personal_resolve() {
            eprintln!("ccync roster install: failed to resolve catalog: {e}");
            return ExitCode::Error;
        }

        if !no_sync {
            let proj_code = super::lifecycle::run_unified_projection(
                false,
                assume_yes,
                false,
                Some(&super::lifecycle::SYNC_GATE_ABORT_CONTEXT),
            );
            if proj_code != ExitCode::Success {
                return ExitCode::Error;
            }
        } else {
            println!(
                "Roster applied (--no-sync). Run ccync sync to apply ccync-managed settings to selected agents."
            );
        }

        if !report.failed.is_empty() {
            ExitCode::Error
        } else {
            ExitCode::Success
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn isolate_env(tmp: &std::path::Path) -> Vec<(String, Option<String>)> {
        let keys = [
            "USERPROFILE",
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
        ];
        let mut saved = Vec::new();
        for k in keys {
            saved.push((k.to_string(), std::env::var(k).ok()));
            unsafe {
                std::env::set_var(k, tmp);
            }
        }
        saved
    }

    fn restore_saved_env(saved: &[(String, Option<String>)]) {
        for (k, v) in saved {
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn roster_dump_cli_writes_ccync_roster_json_and_reports_counts() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().expect("home tempdir");
        let saved = isolate_env(home.path());

        let out_dir = TempDir::new().expect("out tempdir");
        let out_path = out_dir.path().join("ccync.roster.json");

        let ccync_dir = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).expect("create .ccync dir");

        let catalog = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "qualifying-plugin",
                    "displayName": "Qualifying Plugin",
                    "sourceType": "curated-upstream",
                    "installStrategy": "personal-git-clone",
                    "source": "https://github.com/example/qualifying.git",
                    "pin": "0123456789abcdef0123456789abcdef01234567"
                },
                {
                    "pluginId": "local-plugin",
                    "displayName": "Local Plugin",
                    "source": "./local/path",
                    "pin": "0123456789abcdef0123456789abcdef01234567"
                }
            ]
        });
        std::fs::write(
            ccync_dir.join("plugins.json"),
            serde_json::to_string_pretty(&catalog).expect("serialize catalog"),
        )
        .expect("write plugins.json");

        let args = vec![
            "roster".to_string(),
            "dump".to_string(),
            "--out".to_string(),
            out_path.to_str().expect("out path str").to_string(),
        ];

        let code = crate::run(&args);
        restore_saved_env(&saved);

        assert_eq!(code, ExitCode::Success, "roster dump should return Success");
        assert!(out_path.exists(), "output file should exist");

        let content = std::fs::read_to_string(&out_path).expect("read roster file");
        let doc: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        assert_eq!(doc["schemaVersion"], 1);

        let plugins = doc["plugins"].as_array().expect("plugins array");
        assert_eq!(plugins.len(), 1, "only qualifying plugins should be dumped");
        assert_eq!(plugins[0]["pluginId"], "qualifying-plugin");
    }

    #[test]
    fn roster_install_dry_run_exit_matches_class_table() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().expect("home tempdir");
        let saved = isolate_env(home.path());

        let ccync_dir = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).expect("create .ccync dir");

        let catalog = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "converged-plugin",
                    "displayName": "Converged Plugin",
                    "sourceType": "curated-upstream",
                    "installStrategy": "personal-git-clone",
                    "source": "https://github.com/example/converged.git",
                    "pin": "0123456789abcdef0123456789abcdef01234567"
                }
            ]
        });
        std::fs::write(
            ccync_dir.join("plugins.json"),
            serde_json::to_string_pretty(&catalog).expect("serialize catalog"),
        )
        .expect("write plugins.json");

        let roster_dir = TempDir::new().expect("roster tempdir");

        // 1. Roster with only missing entry -> updates available (3)
        let roster_missing_path = roster_dir.path().join("missing.roster.json");
        let roster_missing = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "missing-plugin",
                    "displayName": "Missing Plugin",
                    "sourceType": "curated-upstream",
                    "installStrategy": "personal-git-clone",
                    "source": "https://github.com/example/missing.git",
                    "pin": "1234567890abcdef1234567890abcdef12345678"
                }
            ]
        });
        std::fs::write(
            &roster_missing_path,
            serde_json::to_string_pretty(&roster_missing).unwrap(),
        )
        .expect("write missing roster");

        // 2. Roster with a rejected entry -> error (1)
        let roster_rejected_path = roster_dir.path().join("rejected.roster.json");
        let roster_rejected = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "rejected-plugin",
                    "displayName": "Rejected Plugin",
                    "sourceType": "curated-upstream",
                    "installStrategy": "personal-git-clone",
                    "source": "https://user:pass@github.com/example/rejected.git",
                    "pin": "1234567890abcdef1234567890abcdef12345678"
                }
            ]
        });
        std::fs::write(
            &roster_rejected_path,
            serde_json::to_string_pretty(&roster_rejected).unwrap(),
        )
        .expect("write rejected roster");

        // 3. Roster with only converged entry -> success (0)
        let roster_converged_path = roster_dir.path().join("converged.roster.json");
        let roster_converged = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "converged-plugin",
                    "displayName": "Converged Plugin",
                    "sourceType": "curated-upstream",
                    "installStrategy": "personal-git-clone",
                    "source": "https://github.com/example/converged.git",
                    "pin": "0123456789abcdef0123456789abcdef01234567"
                }
            ]
        });
        std::fs::write(
            &roster_converged_path,
            serde_json::to_string_pretty(&roster_converged).unwrap(),
        )
        .expect("write converged roster");

        fn snapshot_dir(root: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
            let mut files = Vec::new();
            fn collect(
                dir: &std::path::Path,
                base: &std::path::Path,
                out: &mut Vec<(PathBuf, Vec<u8>)>,
            ) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.is_dir() {
                            collect(&p, base, out);
                        } else if p.is_file() {
                            let rel = p.strip_prefix(base).unwrap().to_path_buf();
                            let bytes = std::fs::read(&p).unwrap_or_default();
                            out.push((rel, bytes));
                        }
                    }
                }
            }
            collect(root, root, &mut files);
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        }

        let home_before = snapshot_dir(home.path());

        let code_missing = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--dry-run".to_string(),
            "--in".to_string(),
            roster_missing_path.to_str().unwrap().to_string(),
        ]);

        let code_rejected = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--dry-run".to_string(),
            "--in".to_string(),
            roster_rejected_path.to_str().unwrap().to_string(),
        ]);

        let code_converged = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--dry-run".to_string(),
            "--in".to_string(),
            roster_converged_path.to_str().unwrap().to_string(),
        ]);

        let home_after = snapshot_dir(home.path());
        restore_saved_env(&saved);

        assert_eq!(
            home_before, home_after,
            "dry-run must perform no writes under sandboxed HOME"
        );
        assert_eq!(
            code_missing,
            ExitCode::UpdatesAvailable,
            "only missing entries must exit with UpdatesAvailable (3)"
        );
        assert_eq!(
            code_rejected,
            ExitCode::Error,
            "rejected entries must exit with Error (1)"
        );
        assert_eq!(
            code_converged,
            ExitCode::Success,
            "all converged entries must exit with Success (0)"
        );
    }

    struct EnvGuard(Vec<(String, Option<String>)>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            restore_saved_env(&self.0);
        }
    }

    #[test]
    fn roster_install_rejected_entry_exits_one_and_fetches_nothing() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().expect("home tempdir");
        let saved = isolate_env(home.path());
        let _env_guard = EnvGuard(saved);

        let ccync_dir = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).expect("create .ccync dir");

        let catalog_path = ccync_dir.join("plugins.json");
        let initial_catalog = serde_json::json!({
            "schemaVersion": 1,
            "plugins": []
        });
        std::fs::write(
            &catalog_path,
            serde_json::to_string_pretty(&initial_catalog).expect("serialize catalog"),
        )
        .expect("write plugins.json");

        let origin_dir = TempDir::new().expect("origin git repo");
        let origin_path = origin_dir.path();
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(origin_path)
                .args(args)
                .output()
                .expect("git fixture setup");
            assert!(
                out.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run_git(&["init"]);
        run_git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        let sha = run_git(&["rev-parse", "HEAD"]);

        // `classify_entry` rejects any source that `is_remote_url` does not
        // recognize as remote, and a bare local filesystem path is
        // deliberately not remote. To exercise the real Missing ->
        // execute_install -> real `git clone` path end to end without
        // network access, redirect a real `https://` URL to the local
        // fixture via a `url.<file-url>.insteadOf` entry in the sandboxed
        // HOME's own `~/.gitconfig` (isolate_env already points HOME at
        // `home.path()`) -- git resolves the rewrite transparently, so the
        // clone is genuinely local while the roster's `source` string still
        // satisfies `is_remote_url`.
        let file_origin_url = format!(
            "file://{}",
            origin_path.display().to_string().replace('\\', "/")
        );
        let fake_https_url = "https://example.invalid/roster-fixture.git".to_string();
        run_git(&[
            "config",
            "--global",
            &format!("url.{file_origin_url}.insteadOf"),
            &fake_https_url,
        ]);
        let origin_url = fake_https_url;

        let roster_dir = TempDir::new().expect("roster tempdir");

        // 1. Roster with one rejected plus one missing entry
        let roster_mixed_path = roster_dir.path().join("mixed.roster.json");
        let roster_mixed = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "rejected-plugin",
                    "displayName": "Rejected Plugin",
                    "source": "https://user:pass@github.com/example/rejected.git",
                    "pin": "1234567890abcdef1234567890abcdef12345678"
                },
                {
                    "pluginId": "missing-plugin",
                    "displayName": "Missing Plugin",
                    "source": origin_url,
                    "pin": sha
                }
            ]
        });
        std::fs::write(
            &roster_mixed_path,
            serde_json::to_string_pretty(&roster_mixed).unwrap(),
        )
        .expect("write mixed roster");

        let cache_dir = ccync_dir.join("cache");

        let code_mixed = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--no-sync".to_string(),
            "--in".to_string(),
            roster_mixed_path.to_str().unwrap().to_string(),
        ]);

        assert_eq!(
            code_mixed,
            ExitCode::Error,
            "roster with rejected entry must exit with Error"
        );
        assert!(
            !cache_dir.exists(),
            "no cache directory should be created when rejected entry exists"
        );
        let catalog_after_rejected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
        assert_eq!(
            catalog_after_rejected, initial_catalog,
            "catalog must remain unchanged when rejected entry exists"
        );

        // 2. Roster with only one missing entry
        let roster_missing_path = roster_dir.path().join("missing.roster.json");
        let roster_missing = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "missing-plugin",
                    "displayName": "Missing Plugin",
                    "source": origin_url,
                    "pin": sha
                }
            ]
        });
        std::fs::write(
            &roster_missing_path,
            serde_json::to_string_pretty(&roster_missing).unwrap(),
        )
        .expect("write missing roster");

        let code_missing = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--no-sync".to_string(),
            "--in".to_string(),
            roster_missing_path.to_str().unwrap().to_string(),
        ]);

        assert_eq!(
            code_missing,
            ExitCode::Success,
            "one-missing roster under --no-sync must exit with Success"
        );
        let catalog_after_success: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
        let plugins = catalog_after_success["plugins"]
            .as_array()
            .expect("plugins array");
        assert_eq!(plugins.len(), 1, "catalog must have exactly 1 plugin entry");
        assert_eq!(plugins[0]["pluginId"], "missing-plugin");
        assert_eq!(plugins[0]["pin"], sha);
    }

    /// A machine whose only divergence is held clears its own gate.
    ///
    /// Regression guard for "a gate whose own remedy cannot clear it": a held
    /// entry contributes to neither the rejected count nor the would-change
    /// count, so `roster install` leaves it untouched and the immediately
    /// following `roster install --dry-run` exits `0` instead of staying red
    /// forever.
    #[test]
    fn roster_install_held_only_divergence_then_dry_run_exits_zero() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().expect("home tempdir");
        let saved = isolate_env(home.path());
        let _env_guard = EnvGuard(saved);

        let ccync_dir = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).expect("create .ccync dir");

        let source = "https://github.com/example/held.git";
        let held_sha = "0123456789abcdef0123456789abcdef01234567";
        let roster_sha = "89abcdef0123456789abcdef0123456789abcdef";

        let catalog_path = ccync_dir.join("plugins.json");
        let initial_catalog = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "held-plugin",
                    "displayName": "Held Plugin",
                    "sourceType": "curated-upstream",
                    "installStrategy": "personal-git-clone",
                    "source": source,
                    "pin": held_sha,
                    "held": true
                }
            ]
        });
        std::fs::write(
            &catalog_path,
            serde_json::to_string_pretty(&initial_catalog).expect("serialize catalog"),
        )
        .expect("write plugins.json");

        let roster_dir = TempDir::new().expect("roster tempdir");
        let roster_path = roster_dir.path().join("held.roster.json");
        let roster = serde_json::json!({
            "schemaVersion": 1,
            "plugins": [
                {
                    "pluginId": "held-plugin",
                    "displayName": "Held Plugin",
                    "sourceType": "curated-upstream",
                    "installStrategy": "personal-git-clone",
                    "source": source,
                    "pin": roster_sha
                }
            ]
        });
        std::fs::write(
            &roster_path,
            serde_json::to_string_pretty(&roster).expect("serialize roster"),
        )
        .expect("write roster");

        // The remedy the gate tells the operator to run.
        let code_install = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--no-sync".to_string(),
            "--in".to_string(),
            roster_path.to_str().unwrap().to_string(),
        ]);
        assert_eq!(
            code_install,
            ExitCode::Success,
            "held-only divergence must not fail roster install"
        );

        // The held entry is left exactly as it was: no fetch, no re-pin.
        assert!(
            !ccync_dir.join("cache").exists(),
            "a held entry must never be fetched"
        );
        let catalog_after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
        assert_eq!(
            catalog_after, initial_catalog,
            "a held entry must never be re-pinned or rewritten"
        );

        // The gate that remedy is supposed to clear.
        let code_dry_run = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--dry-run".to_string(),
            "--in".to_string(),
            roster_path.to_str().unwrap().to_string(),
        ]);
        assert_eq!(
            code_dry_run,
            ExitCode::Success,
            "held-only divergence must not keep the dry-run gate red after install"
        );
    }

    /// A re-pin lands on all three surfaces, not just the catalog.
    ///
    /// Regression guard for "reporting convergence while projecting the old
    /// commit": after `roster install` moves an entry from `<id>@<old-sha>` to
    /// `<new-sha>`, the personal catalog `pin`, the lockfile's
    /// `_personalPlugins` pin, and the rendered content under
    /// `~/.ccync/build/render/` must all follow. The old cache directory is
    /// deliberately left on disk so the pin-exact cache selection is genuinely
    /// exercised rather than passing because only one candidate exists.
    #[test]
    fn roster_install_repin_propagates_to_catalog_lock_and_render() {
        let _guard = crate::test_io_lock();

        let home = TempDir::new().expect("home tempdir");
        let saved = isolate_env(home.path());
        let _env_guard = EnvGuard(saved);

        let ccync_dir = home.path().join(".ccync");
        std::fs::create_dir_all(&ccync_dir).expect("create .ccync dir");

        // A machine that has been through `ccync init`, so the projecting run
        // below is not refused by the initialized-machine preflight.
        std::fs::write(
            ccync_dir.join("config.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schemaVersion": 1,
                "selectedProjectionTargets": ["claude"]
            }))
            .expect("serialize config"),
        )
        .expect("write config.json");

        // Origin repo carrying one plugin skill whose body differs between the
        // two commits, so the rendered bytes name which commit was projected.
        let origin_dir = TempDir::new().expect("origin git repo");
        let origin_path = origin_dir.path();
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(origin_path)
                .args(args)
                .output()
                .expect("git fixture setup");
            assert!(
                out.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let skill_dir = origin_path.join("skills").join("demo");
        std::fs::create_dir_all(&skill_dir).expect("create skill dir");
        let skill_file = skill_dir.join("SKILL.md");
        let skill_body = |marker: &str| {
            format!("---\nname: demo\ndescription: demo skill\n---\n\nbody {marker}\n")
        };
        std::fs::write(&skill_file, skill_body("marker-a")).expect("write skill A");
        run_git(&["init"]);
        run_git(&["add", "-A"]);
        run_git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "commit A",
        ]);
        let sha_a = run_git(&["rev-parse", "HEAD"]);

        std::fs::write(&skill_file, skill_body("marker-b")).expect("write skill B");
        run_git(&["add", "-A"]);
        run_git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "commit B",
        ]);
        let sha_b = run_git(&["rev-parse", "HEAD"]);
        assert_ne!(sha_a, sha_b, "fixture must produce two distinct commits");

        // Same insteadOf redirection the rejected-entry test uses: the roster
        // source stays remote-shaped for is_remote_url while git resolves it to
        // the local fixture, so no network access is needed.
        let file_origin_url = format!(
            "file://{}",
            origin_path.display().to_string().replace('\\', "/")
        );
        let origin_url = "https://example.invalid/roster-repin-fixture.git".to_string();
        run_git(&[
            "config",
            "--global",
            &format!("url.{file_origin_url}.insteadOf"),
            &origin_url,
        ]);

        let roster_dir = TempDir::new().expect("roster tempdir");
        let write_roster = |name: &str, pin: &str| {
            let path = roster_dir.path().join(name);
            let doc = serde_json::json!({
                "schemaVersion": 1,
                "plugins": [
                    {
                        "pluginId": "repin-plugin",
                        "displayName": "Repin Plugin",
                        "sourceType": "curated-upstream",
                        "installStrategy": "personal-git-clone",
                        "source": origin_url,
                        "pin": pin
                    }
                ]
            });
            std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap())
                .expect("write roster");
            path
        };

        // Phase 1 — reach the old-sha state the way a real machine would.
        let roster_a = write_roster("a.roster.json", &sha_a);
        let code_a = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--yes".to_string(),
            "--in".to_string(),
            roster_a.to_str().unwrap().to_string(),
        ]);
        assert_eq!(code_a, ExitCode::Success, "phase 1 install must succeed");

        let rendered = ccync_dir
            .join("build")
            .join("render")
            .join("skills")
            .join("demo")
            .join("SKILL.md");
        let rendered_before = std::fs::read_to_string(&rendered)
            .unwrap_or_else(|e| panic!("read rendered skill at {}: {e}", rendered.display()));
        assert!(
            rendered_before.contains("marker-a"),
            "sanity: before the re-pin the render must carry the old-sha commit, got: {rendered_before}"
        );

        let cache_root = ccync_dir.join("cache");
        let old_cache_dir = cache_root.join(format!("repin-plugin@{sha_a}"));
        assert!(
            old_cache_dir.is_dir(),
            "phase 1 must materialize the old-sha cache dir at {}",
            old_cache_dir.display()
        );

        // Phase 2 — the re-pin under test, with projection enabled.
        let roster_b = write_roster("b.roster.json", &sha_b);
        let code_b = crate::run(&[
            "roster".to_string(),
            "install".to_string(),
            "--yes".to_string(),
            "--in".to_string(),
            roster_b.to_str().unwrap().to_string(),
        ]);
        assert_eq!(
            code_b,
            ExitCode::Success,
            "phase 2 re-pin install must succeed"
        );

        // Surface 1 — the personal catalog.
        let catalog: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(ccync_dir.join("plugins.json")).expect("read catalog"),
        )
        .expect("parse catalog");
        let entry = catalog["plugins"]
            .as_array()
            .expect("plugins array")
            .iter()
            .find(|p| p["pluginId"] == "repin-plugin")
            .expect("catalog keeps the entry");
        assert_eq!(entry["pin"], sha_b, "catalog pin must follow the re-pin");

        // Surface 2 — the lockfile _personalPlugins pin.
        let lock: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(ccync_dir.join("build").join("lock.json"))
                .expect("read lock.json"),
        )
        .expect("parse lock.json");
        let locked = lock["_personalPlugins"]
            .as_array()
            .expect("_personalPlugins array")
            .iter()
            .find(|p| p["pluginId"] == "repin-plugin")
            .expect("lockfile keeps the entry");
        assert_eq!(
            locked["pin"], sha_b,
            "lockfile _personalPlugins pin must follow the re-pin"
        );

        // The old cache dir is still present, so surface 3 below proves
        // pin-exact selection rather than "only one candidate existed".
        assert!(
            old_cache_dir.is_dir(),
            "old-sha cache dir must survive the re-pin"
        );
        assert!(
            cache_root.join(format!("repin-plugin@{sha_b}")).is_dir(),
            "new-sha cache dir must be materialized"
        );

        // Surface 3 — the rendered content under ~/.ccync/build/render/.
        let rendered_body = std::fs::read_to_string(&rendered)
            .unwrap_or_else(|e| panic!("read rendered skill at {}: {e}", rendered.display()));
        assert!(
            rendered_body.contains("marker-b"),
            "render must project the new-sha commit, got: {rendered_body}"
        );
        assert!(
            !rendered_body.contains("marker-a"),
            "render must not still project the old-sha commit, got: {rendered_body}"
        );
    }
}
