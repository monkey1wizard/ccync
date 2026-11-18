use ccync_engine::roster::{classify_entry, Classification, RosterEntry};
use serde_json::json;

#[test]
fn classify_entry_orders_rejected_before_held() {
    // 1. Locally held with a local-path source -> Rejected (order: Rejected beats Held)
    let entry_local_path = RosterEntry {
        plugin_id: "test-plugin".to_string(),
        source: "./local/path".to_string(),
        pin: "0123456789abcdef0123456789abcdef01234567".to_string(),
        display_name: None,
    };
    let local_held_local_path = json!({
        "pluginId": "test-plugin",
        "source": "./local/path",
        "pin": "0123456789abcdef0123456789abcdef01234567",
        "held": true,
    });
    let c1 = classify_entry(&entry_local_path, Some(&local_held_local_path));
    assert!(
        matches!(c1, Classification::Rejected(_)),
        "held locally with a local-path source must be Rejected, got {:?}",
        c1
    );

    // 2. Source with userinfo -> Rejected (order: Rejected beats Missing)
    let entry_userinfo = RosterEntry {
        plugin_id: "userinfo-plugin".to_string(),
        source: "https://user:pass@github.com/example/repo.git".to_string(),
        pin: "0123456789abcdef0123456789abcdef01234567".to_string(),
        display_name: None,
    };
    let c2 = classify_entry(&entry_userinfo, None);
    assert!(
        matches!(c2, Classification::Rejected(_)),
        "source with userinfo must be Rejected, got {:?}",
        c2
    );

    // 3. No local id -> Missing
    let entry_valid = RosterEntry {
        plugin_id: "valid-plugin".to_string(),
        source: "https://github.com/example/valid.git".to_string(),
        pin: "0123456789abcdef0123456789abcdef01234567".to_string(),
        display_name: None,
    };
    let c3 = classify_entry(&entry_valid, None);
    assert_eq!(
        c3,
        Classification::Missing,
        "missing local entry must be Missing"
    );

    // 4. Held with a different pin -> Held { diverged: true }
    let local_held_diff_pin = json!({
        "pluginId": "valid-plugin",
        "source": "https://github.com/example/valid.git",
        "pin": "1111111111111111111111111111111111111111",
        "held": true,
    });
    let c4 = classify_entry(&entry_valid, Some(&local_held_diff_pin));
    assert_eq!(
        c4,
        Classification::Held {
            current_sha: Some("1111111111111111111111111111111111111111".to_string()),
            current_source: Some("https://github.com/example/valid.git".to_string()),
            diverged: true,
        },
        "held with different pin must be Held {{ diverged: true }}"
    );

    // 5. Held with same pin and source -> Held { diverged: false }
    let local_held_same = json!({
        "pluginId": "valid-plugin",
        "source": "https://github.com/example/valid.git",
        "pin": "0123456789abcdef0123456789abcdef01234567",
        "held": true,
    });
    let c5 = classify_entry(&entry_valid, Some(&local_held_same));
    assert_eq!(
        c5,
        Classification::Held {
            current_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            current_source: Some("https://github.com/example/valid.git".to_string()),
            diverged: false,
        },
        "held with same pin and source must be Held {{ diverged: false }}"
    );

    // 6. Held with different source -> Held { diverged: true }
    let local_held_diff_source = json!({
        "pluginId": "valid-plugin",
        "source": "https://github.com/other/valid.git",
        "pin": "0123456789abcdef0123456789abcdef01234567",
        "held": true,
    });
    let c6 = classify_entry(&entry_valid, Some(&local_held_diff_source));
    assert_eq!(
        c6,
        Classification::Held {
            current_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            current_source: Some("https://github.com/other/valid.git".to_string()),
            diverged: true,
        },
        "held with different source must be Held {{ diverged: true }}"
    );

    // 7. Different local source -> SourceMismatched
    let local_diff_source = json!({
        "pluginId": "valid-plugin",
        "source": "https://github.com/other/valid.git",
        "pin": "0123456789abcdef0123456789abcdef01234567",
    });
    let c7 = classify_entry(&entry_valid, Some(&local_diff_source));
    assert_eq!(
        c7,
        Classification::SourceMismatched {
            current_source: "https://github.com/other/valid.git".to_string(),
            current_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        },
        "different local source must be SourceMismatched"
    );

    // 8. No local pin key -> PinMismatched { current_sha: None }
    let local_no_pin = json!({
        "pluginId": "valid-plugin",
        "source": "https://github.com/example/valid.git",
    });
    let c8 = classify_entry(&entry_valid, Some(&local_no_pin));
    assert_eq!(
        c8,
        Classification::PinMismatched { current_sha: None },
        "no local pin must be PinMismatched {{ current_sha: None }}"
    );

    // 9. Different local pin -> PinMismatched { current_sha: Some(...) }
    let local_diff_pin = json!({
        "pluginId": "valid-plugin",
        "source": "https://github.com/example/valid.git",
        "pin": "1111111111111111111111111111111111111111",
    });
    let c9 = classify_entry(&entry_valid, Some(&local_diff_pin));
    assert_eq!(
        c9,
        Classification::PinMismatched {
            current_sha: Some("1111111111111111111111111111111111111111".to_string()),
        },
        "different local pin must be PinMismatched {{ current_sha: Some(...) }}"
    );

    // 10. Same source and pin -> Converged
    let local_converged = json!({
        "pluginId": "valid-plugin",
        "source": "https://github.com/example/valid.git",
        "pin": "0123456789abcdef0123456789abcdef01234567",
    });
    let c10 = classify_entry(&entry_valid, Some(&local_converged));
    assert_eq!(
        c10,
        Classification::Converged,
        "same source and pin must be Converged"
    );
}
