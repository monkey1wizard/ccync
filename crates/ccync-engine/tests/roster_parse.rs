use ccync_engine::roster::{parse_roster, RosterEntry, RosterParseError};
use serde_json::json;

#[test]
fn parse_roster_refuses_duplicate_pinless_and_short_pin_entries() {
    // 1. Refuses duplicate pluginId
    let duplicate_roster = json!({
        "plugins": [
            {
                "pluginId": "sample-plugin",
                "source": "https://github.com/example/sample.git",
                "pin": "0123456789abcdef0123456789abcdef01234567"
            },
            {
                "pluginId": "sample-plugin",
                "source": "https://github.com/example/other.git",
                "pin": "abcdef0123456789abcdef0123456789abcdef01"
            }
        ]
    });
    let dup_res = parse_roster(&duplicate_roster);
    assert_eq!(
        dup_res,
        Err(RosterParseError::DuplicatePluginId(
            "sample-plugin".to_string()
        )),
        "duplicate pluginId must produce DuplicatePluginId error"
    );

    // 2. Refuses pinless entry
    let pinless_roster = json!({
        "plugins": [
            {
                "pluginId": "pinless-plugin",
                "source": "https://github.com/example/pinless.git"
            }
        ]
    });
    let pinless_res = parse_roster(&pinless_roster);
    assert_eq!(
        pinless_res,
        Err(RosterParseError::EntryMissingField {
            index: 0,
            field: "pin",
        }),
        "missing pin must produce EntryMissingField error"
    );

    // 3. Refuses 7-char abbreviated pin
    let short_pin_roster = json!({
        "plugins": [
            {
                "pluginId": "short-pin-plugin",
                "source": "https://github.com/example/short.git",
                "pin": "0123456"
            }
        ]
    });
    let short_pin_res = parse_roster(&short_pin_roster);
    assert_eq!(
        short_pin_res,
        Err(RosterParseError::InvalidPin {
            plugin_id: "short-pin-plugin".to_string(),
            pin: "0123456".to_string(),
        }),
        "7-char abbreviated pin must produce InvalidPin error"
    );

    // 4. Valid roster produces N entries
    let valid_roster = json!({
        "plugins": [
            {
                "pluginId": "plugin-a",
                "source": "https://github.com/example/a.git",
                "pin": "0123456789abcdef0123456789abcdef01234567",
                "displayName": "Plugin Alpha"
            },
            {
                "pluginId": "plugin-b",
                "source": "https://github.com/example/b.git",
                "pin": "abcdef0123456789abcdef0123456789abcdef01"
            }
        ]
    });
    let entries = parse_roster(&valid_roster).expect("valid roster should parse");
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0],
        RosterEntry {
            plugin_id: "plugin-a".to_string(),
            source: "https://github.com/example/a.git".to_string(),
            pin: "0123456789abcdef0123456789abcdef01234567".to_string(),
            display_name: Some("Plugin Alpha".to_string()),
        }
    );
    assert_eq!(
        entries[1],
        RosterEntry {
            plugin_id: "plugin-b".to_string(),
            source: "https://github.com/example/b.git".to_string(),
            pin: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            display_name: None,
        }
    );
}
