use ccync_engine::roster::{
    execute_install, Classification, ClassifiedEntry, InstallOptions, RosterEntry,
};
use serde_json::json;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

fn run_git(cwd: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git command must succeed to set up test fixture");
    assert!(
        out.status.success(),
        "git {:?} in {:?} failed: stderr={}",
        args,
        cwd,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn execute_install_acts_on_missing_pin_mismatch_and_source_mismatch() {
    let origin1_dir = tempdir().expect("tempdir for origin1 repo");
    let origin1_path = origin1_dir.path();
    run_git(origin1_path, &["init"]);
    run_git(
        origin1_path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "commit 1",
        ],
    );
    let sha1_a = run_git(origin1_path, &["rev-parse", "HEAD"]);
    run_git(
        origin1_path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "commit 2",
        ],
    );
    let sha1_b = run_git(origin1_path, &["rev-parse", "HEAD"]);
    assert_ne!(sha1_a, sha1_b, "origin1 commits must have distinct SHAs");
    let origin1_url = origin1_path.to_str().expect("valid utf-8 path").to_string();

    let origin2_dir = tempdir().expect("tempdir for origin2 repo");
    let origin2_path = origin2_dir.path();
    run_git(origin2_path, &["init"]);
    run_git(
        origin2_path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--allow-empty",
            "-m",
            "commit 1",
        ],
    );
    let sha2_a = run_git(origin2_path, &["rev-parse", "HEAD"]);
    let origin2_url = origin2_path.to_str().expect("valid utf-8 path").to_string();

    let work_dir = tempdir().expect("tempdir for work");
    let cache_root = work_dir.path().join("cache");
    fs::create_dir_all(&cache_root).expect("create cache root");

    let catalog_path = work_dir.path().join("plugins.json");
    let lock_path = work_dir.path().join("lock.json");

    let initial_catalog = json!({
        "schemaVersion": 1,
        "plugins": [
            {
                "pluginId": "pin-mismatched",
                "displayName": "pin-mismatched",
                "sourceType": "curated-upstream",
                "installStrategy": "personal-git-clone",
                "source": origin1_url,
                "pin": sha1_a
            },
            {
                "pluginId": "source-mismatched",
                "displayName": "source-mismatched",
                "sourceType": "curated-upstream",
                "installStrategy": "personal-git-clone",
                "source": origin1_url,
                "pin": sha1_a
            },
            {
                "pluginId": "held-plugin",
                "displayName": "held-plugin",
                "sourceType": "curated-upstream",
                "installStrategy": "personal-git-clone",
                "source": origin1_url,
                "pin": sha1_a,
                "held": true
            }
        ]
    });
    fs::write(
        &catalog_path,
        serde_json::to_string_pretty(&initial_catalog).expect("serialize catalog"),
    )
    .expect("write plugins.json");

    let initial_held_json =
        serde_json::to_string(&initial_catalog["plugins"][2]).expect("serialize held entry");

    let initial_lock = json!({
        "schemaVersion": 1,
        "_personalPlugins": [
            {
                "pluginId": "pin-mismatched",
                "source": origin1_url,
                "pin": sha1_a
            },
            {
                "pluginId": "source-mismatched",
                "source": origin1_url,
                "pin": sha1_a
            },
            {
                "pluginId": "held-plugin",
                "source": origin1_url,
                "pin": sha1_a,
                "held": true
            }
        ]
    });
    fs::write(
        &lock_path,
        serde_json::to_string_pretty(&initial_lock).expect("serialize lock"),
    )
    .expect("write lock.json");

    let entries = vec![
        ClassifiedEntry {
            entry: RosterEntry {
                plugin_id: "missing-plugin".to_string(),
                source: origin1_url.clone(),
                pin: sha1_a.clone(),
                display_name: Some("missing-plugin".to_string()),
            },
            class: Classification::Missing,
        },
        ClassifiedEntry {
            entry: RosterEntry {
                plugin_id: "pin-mismatched".to_string(),
                source: origin1_url.clone(),
                pin: sha1_b.clone(),
                display_name: Some("pin-mismatched".to_string()),
            },
            class: Classification::PinMismatched {
                current_sha: Some(sha1_a.clone()),
            },
        },
        ClassifiedEntry {
            entry: RosterEntry {
                plugin_id: "source-mismatched".to_string(),
                source: origin2_url.clone(),
                pin: sha2_a.clone(),
                display_name: Some("source-mismatched".to_string()),
            },
            class: Classification::SourceMismatched {
                current_source: origin1_url.clone(),
                current_sha: Some(sha1_a.clone()),
            },
        },
        ClassifiedEntry {
            entry: RosterEntry {
                plugin_id: "held-plugin".to_string(),
                source: origin1_url.clone(),
                pin: sha1_b.clone(),
                display_name: Some("held-plugin".to_string()),
            },
            class: Classification::Held {
                current_sha: Some(sha1_a.clone()),
                current_source: Some(origin1_url.clone()),
                diverged: true,
            },
        },
    ];

    let opts = InstallOptions {
        cache_root: cache_root.clone(),
        catalog_path: catalog_path.clone(),
        pin_files: vec![catalog_path.clone(), lock_path.clone()],
    };

    let report = execute_install(&entries, &opts);

    // 1. Report assertions
    assert_eq!(report.added.len(), 1, "expected 1 added entry");
    assert_eq!(report.added, vec!["missing-plugin".to_string()]);
    assert_eq!(report.repinned.len(), 2, "expected 2 repinned entries");
    assert_eq!(
        report.source_rewrites.len(),
        1,
        "expected 1 source rewrite entry"
    );
    assert_eq!(
        report.source_rewrites,
        vec![(
            "source-mismatched".to_string(),
            origin1_url.clone(),
            origin2_url.clone()
        )]
    );
    assert_eq!(report.untouched, vec!["held-plugin".to_string()]);
    assert!(
        report.failed.is_empty(),
        "expected no failures, got {:?}",
        report.failed
    );

    // 2. Cache dir existence: <id>@<pin>
    assert!(
        cache_root.join(format!("missing-plugin@{sha1_a}")).is_dir(),
        "missing-plugin cache dir should exist"
    );
    assert!(
        cache_root.join(format!("pin-mismatched@{sha1_b}")).is_dir(),
        "pin-mismatched cache dir should exist"
    );
    assert!(
        cache_root
            .join(format!("source-mismatched@{sha2_a}"))
            .is_dir(),
        "source-mismatched cache dir should exist"
    );

    // 3. Updated catalog pin and source
    let catalog_str = fs::read_to_string(&catalog_path).expect("read catalog");
    let catalog_val: serde_json::Value = serde_json::from_str(&catalog_str).expect("parse catalog");
    let plugins = catalog_val["plugins"].as_array().expect("plugins array");

    let missing_entry = plugins
        .iter()
        .find(|e| e["pluginId"] == "missing-plugin")
        .expect("missing-plugin in catalog");
    assert_eq!(missing_entry["pin"], sha1_a);
    assert_eq!(missing_entry["source"], origin1_url);

    let pin_entry = plugins
        .iter()
        .find(|e| e["pluginId"] == "pin-mismatched")
        .expect("pin-mismatched in catalog");
    assert_eq!(pin_entry["pin"], sha1_b);
    assert_eq!(pin_entry["source"], origin1_url);

    let src_entry = plugins
        .iter()
        .find(|e| e["pluginId"] == "source-mismatched")
        .expect("source-mismatched in catalog");
    assert_eq!(src_entry["pin"], sha2_a);
    assert_eq!(src_entry["source"], origin2_url);

    // 4. Held entry byte-identical
    let held_entry = plugins
        .iter()
        .find(|e| e["pluginId"] == "held-plugin")
        .expect("held-plugin in catalog");
    assert_eq!(
        serde_json::to_string(held_entry).expect("serialize held entry"),
        initial_held_json,
        "held entry must remain byte-identical"
    );
}
