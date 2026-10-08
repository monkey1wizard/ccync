//! Regression coverage for restore notices about unresolved MCP secrets.

use super::lifecycle::restore_secret_notice_lines;

#[test]
fn restore_secret_notice_lines_lists_missing_variables_by_server() {
    let lock = serde_json::json!({
        "_mcpServers": {
            "zeta": {
                "env": {
                    "TOKEN": "${ZETA_TOKEN}",
                    "API_KEY": "${ZETA_API_KEY}"
                }
            },
            "alpha": {
                "headers": {
                    "AUTH_TOKEN": "${ALPHA_AUTH_TOKEN}"
                }
            }
        }
    });

    assert_eq!(
        restore_secret_notice_lines(&lock),
        vec![
            "restore: MCP server alpha needs ALPHA_AUTH_TOKEN — add to config.json#secrets or the environment",
            "restore: MCP server zeta needs ZETA_API_KEY, ZETA_TOKEN — add to config.json#secrets or the environment",
        ]
    );
}

#[test]
fn restore_secret_notice_lines_ignores_resolved_variables() {
    let lock = serde_json::json!({
        "_mcpServers": {
            "healthy": {
                "env": {
                    "API_TOKEN": "available"
                }
            }
        }
    });

    assert!(restore_secret_notice_lines(&lock).is_empty());
}

#[test]
fn restore_secret_notice_lines_is_empty_without_or_with_invalid_mcp_servers() {
    assert!(restore_secret_notice_lines(&serde_json::json!({})).is_empty());
    assert!(restore_secret_notice_lines(&serde_json::json!({
        "_mcpServers": "not a manifest"
    }))
    .is_empty());
}
