use ccync_engine::install::{read_personal_catalog, write_json_atomic};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn engine_personal_catalog_read_write_round_trips_atomically() {
    let dir = tempdir().expect("tempdir");
    let catalog_path = dir.path().join("sub").join("plugins.json");
    let sample = json!({
        "schemaVersion": 1,
        "plugins": [
            {
                "pluginId": "sample-plugin",
                "source": "https://github.com/example/sample.git",
                "pin": "0123456789abcdef0123456789abcdef01234567"
            }
        ]
    });

    write_json_atomic(&catalog_path, &sample).expect("write_json_atomic succeeds");

    // Check no .tmp residue
    let tmp_path = catalog_path.with_extension("tmp");
    assert!(
        !tmp_path.exists(),
        "temporary file must not linger after atomic write"
    );

    let read_back = read_personal_catalog(&catalog_path);
    assert_eq!(read_back, Some(sample));
}
