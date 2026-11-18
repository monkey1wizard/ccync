use ccync_engine::plugin_upgrade::{apply_upgrade_rewriting_source, UpgradeChange};
use serde_json::json;
use std::fs;
use tempfile::tempdir;

#[test]
fn apply_upgrade_rewriting_source_writes_pin_and_source_in_one_pass() {
    let dir = tempdir().expect("tempdir");
    let cache_root = dir.path().join("cache");
    fs::create_dir_all(&cache_root).expect("create cache root");

    let lock_path = dir.path().join("lock.json");
    let initial_json = json!({
        "_personalPlugins": [
            {
                "pluginId": "sample-plugin",
                "installStrategy": "personal-git-clone",
                "source": "https://github.com/old-org/sample-plugin.git",
                "pin": "1111111111111111111111111111111111111111"
            }
        ]
    });
    fs::write(
        &lock_path,
        serde_json::to_string_pretty(&initial_json).expect("serialize initial json"),
    )
    .expect("write lock.json");

    let change = UpgradeChange {
        plugin_id: "sample-plugin".to_string(),
        source: "https://github.com/new-org/sample-plugin.git".to_string(),
        old_sha: Some("1111111111111111111111111111111111111111".to_string()),
        new_sha: "2222222222222222222222222222222222222222".to_string(),
    };

    // Fake clone closure that creates dest directory and dummy file to pass landed check
    let res = apply_upgrade_rewriting_source(
        &change,
        &cache_root,
        &[lock_path.as_path()],
        |_url, dest| {
            fs::create_dir_all(dest).map_err(|e| e.to_string())?;
            fs::write(dest.join("plugin.json"), b"{}").map_err(|e| e.to_string())?;
            Ok(())
        },
    );
    assert!(
        res.is_ok(),
        "apply_upgrade_rewriting_source returned error: {res:?}"
    );

    let content = fs::read_to_string(&lock_path).expect("read back lock.json");
    let updated_json: serde_json::Value =
        serde_json::from_str(&content).expect("parse updated lock.json");

    let entry = &updated_json["_personalPlugins"][0];
    assert_eq!(entry["pin"], change.new_sha, "pin must match new_sha");
    assert_eq!(
        entry["source"], change.source,
        "source must be rewritten to change.source"
    );
}
