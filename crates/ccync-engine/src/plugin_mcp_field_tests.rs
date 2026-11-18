use super::install::read_plugin_mcp_servers;
use serde_json::json;
use std::fs;

#[test]
fn plugin_inventory_keeps_unresolved_fields_from_preferred_manifest() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".mcp.json"),
        json!({"mcpServers": {"demo": {
            "command": "${PLUGIN_ROOT}/run", "args": ["${TOKEN}"], "cwd": "${PLUGIN_DATA}",
            "custom": "kept"
        }}})
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.path().join("mcp.json"),
        json!({"mcpServers": {"other": {"command": "ignored"}}}).to_string(),
    )
    .unwrap();
    let read = read_plugin_mcp_servers("demo", dir.path())
        .unwrap()
        .unwrap();
    assert!(read.unresolved_servers.contains_key("demo"));
    assert!(!read.unresolved_servers.contains_key("other"));
    assert_eq!(
        read.unresolved_servers["demo"]["command"],
        "${PLUGIN_ROOT}/run"
    );
    assert_eq!(read.unresolved_servers["demo"]["args"][0], "${TOKEN}");
    assert_eq!(read.unresolved_servers["demo"]["cwd"], "${PLUGIN_DATA}");
    assert_eq!(read.unresolved_servers["demo"]["custom"], "kept");
}

#[test]
fn plugin_mcp_unknown_field_warning_lists_fields_in_sorted_order() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "context7": {
                    "url": "https://example.com/mcp",
                    "zeta": true,
                    "headersHelper": "helper",
                    "alpha": "value"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("context7", temp_dir.path())
        .unwrap()
        .expect("the MCP manifest should be read");

    assert_eq!(
        read.warnings,
        vec![
            "plugin 'context7': server 'context7' has unsupported fields: alpha, headersHelper, zeta; ignored"
        ]
    );
    assert_eq!(
        read.servers["context7"],
        json!({
            "url": "https://example.com/mcp",
            "zeta": true,
            "headersHelper": "helper",
            "alpha": "value"
        })
    );
}

#[test]
fn plugin_mcp_unknown_field_warning_is_emitted_per_server() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "first": {"unknownB": 1, "unknownA": 2},
                "second": {"type": "sse", "url": "https://example.com/sse"}
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("demo", temp_dir.path())
        .unwrap()
        .expect("the MCP manifest should be read");

    assert_eq!(
        read.warnings,
        vec!["plugin 'demo': server 'first' has unsupported fields: unknownA, unknownB; ignored"]
    );
}

#[test]
fn plugin_mcp_unknown_field_tp12_headers_helper_and_oauth() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "server_unsupported": {
                    "url": "https://example.com/mcp",
                    "headersHelper": "helper",
                    "oauth": true
                },
                "server_known_only": {
                    "type": "sse",
                    "url": "https://example.com/sse",
                    "headers": {"Authorization": "Bearer token"}
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("test-plugin", temp_dir.path())
        .unwrap()
        .expect("the MCP manifest should be read");

    // Exactly one warning containing plugin id, server name, headersHelper, and oauth
    assert_eq!(
        read.warnings,
        vec![
            "plugin 'test-plugin': server 'server_unsupported' has unsupported fields: headersHelper, oauth; ignored"
        ]
    );

    // Leaves the server object unchanged (unsupported fields remain untouched)
    assert_eq!(
        read.servers["server_unsupported"],
        json!({
            "url": "https://example.com/mcp",
            "headersHelper": "helper",
            "oauth": true
        })
    );

    // Known-only server is also intact and generated no warnings
    assert_eq!(
        read.servers["server_known_only"],
        json!({
            "type": "sse",
            "url": "https://example.com/sse",
            "headers": {"Authorization": "Bearer token"}
        })
    );
}

#[test]
fn plugin_mcp_unknown_field_with_bare_mcp_json_fallback() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    // Use mcp.json instead of .mcp.json
    fs::write(
        temp_dir.path().join("mcp.json"),
        json!({
            "mcpServers": {
                "server_a": {
                    "command": "run.sh",
                    "extra": 123
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("fallback-plugin", temp_dir.path())
        .unwrap()
        .expect("mcp.json fallback should be read");

    assert_eq!(
        read.warnings,
        vec!["plugin 'fallback-plugin': server 'server_a' has unsupported fields: extra; ignored"]
    );
}

#[test]
fn plugin_mcp_unknown_field_ignores_non_object_server_gracefully() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "bad_server": "not-an-object",
                "valid_server": {
                    "type": "sse",
                    "url": "https://example.com/sse"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("test-plugin", temp_dir.path())
        .unwrap()
        .expect("manifest should be read");

    assert!(read.warnings.is_empty());
}

#[test]
fn claude_plugin_placeholder_aliases_expand_in_stdio_fields() {
    let _env_guard = crate::test_support::env_guard();
    let temp_dir = tempfile::TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "stdio": {
                    "command": "${CLAUDE_PLUGIN_ROOT}/bin/tool",
                    "args": [
                        "${PLUGIN_ROOT}",
                        "${CLAUDE_PLUGIN_ROOT}",
                        "${PLUGIN_DATA}",
                        "${CLAUDE_PLUGIN_DATA}"
                    ],
                    "env": {
                        "ROOT": "${CLAUDE_PLUGIN_ROOT}",
                        "DATA": "${CLAUDE_PLUGIN_DATA}"
                    },
                    "cwd": "${CLAUDE_PLUGIN_DATA}/work"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("alias-plugin", temp_dir.path())
        .unwrap()
        .expect("the MCP manifest should be read");
    let server = &read.servers["stdio"];
    let plugin_root = temp_dir.path().display().to_string();
    let plugin_data = ccync_foundation::paths::ccync_data_root()
        .unwrap()
        .join("alias-plugin")
        .display()
        .to_string();

    assert_eq!(server["command"], json!(format!("{plugin_root}/bin/tool")));
    assert_eq!(
        server["args"],
        json!([plugin_root, plugin_root, plugin_data, plugin_data])
    );
    assert_eq!(
        server["env"],
        json!({
            "ROOT": plugin_root,
            "DATA": plugin_data,
            "PLUGIN_ROOT": temp_dir.path().display().to_string(),
            "PLUGIN_DATA": ccync_foundation::paths::ccync_data_root()
                .unwrap()
                .join("alias-plugin")
                .display()
                .to_string()
        })
    );
    assert_eq!(server["cwd"], json!(format!("{plugin_data}/work")));
}

#[test]
fn claude_plugin_placeholder_expansion_is_single_pass_and_reserved_alias_is_named() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let plugin_dir = temp_dir.path().join("root-${CLAUDE_PLUGIN_DATA}-literal");
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join(".mcp.json"),
        json!({
            "mcpServers": {
                "stdio": {
                    "command": "tool",
                    "args": ["${CLAUDE_PLUGIN_ROOT}"],
                    "env": {"CLAUDE_PLUGIN_ROOT": "/custom/root"}
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("single-pass-plugin", &plugin_dir)
        .unwrap()
        .expect("the MCP manifest should be read");
    assert!(!read.servers.contains_key("stdio"));
    assert_eq!(
        read.warnings,
        vec![
            "server 'stdio' from plugin single-pass-plugin: env must not set CLAUDE_PLUGIN_ROOT (Agent Plugins §9.2); skipped"
        ]
    );

    fs::write(
        plugin_dir.join(".mcp.json"),
        json!({
            "mcpServers": {
                "stdio": {
                    "command": "tool",
                    "args": ["${CLAUDE_PLUGIN_ROOT}"]
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let read = read_plugin_mcp_servers("single-pass-plugin", &plugin_dir)
        .unwrap()
        .expect("the MCP manifest should be read");
    assert_eq!(
        read.servers["stdio"]["args"][0],
        json!(plugin_dir.display().to_string())
    );
}

#[test]
fn claude_plugin_placeholder_tp13_exact_scenario() {
    let _env_guard = crate::test_support::env_guard();
    let temp_dir = tempfile::TempDir::new().unwrap();
    let plugin_dir = temp_dir.path();
    let plugin_root = plugin_dir.display().to_string();
    let plugin_data = ccync_foundation::paths::ccync_data_root()
        .unwrap()
        .join("test-plugin")
        .display()
        .to_string();

    fs::write(
        plugin_dir.join(".mcp.json"),
        json!({
            "mcpServers": {
                "server_valid": {
                    "command": "${CLAUDE_PLUGIN_ROOT}/bin/x",
                    "args": ["${CLAUDE_PLUGIN_DATA}/db", "${PLUGIN_ROOT}"]
                },
                "server_bad_env": {
                    "command": "tool",
                    "args": ["arg"],
                    "env": {
                        "CLAUDE_PLUGIN_DATA": "/forbidden/path"
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("test-plugin", plugin_dir)
        .unwrap()
        .expect("manifest should be read");

    // server_valid renders plugin directory and data directory in both positions
    let valid = &read.servers["server_valid"];
    assert_eq!(valid["command"], json!(format!("{plugin_root}/bin/x")));
    assert_eq!(
        valid["args"],
        json!([format!("{plugin_data}/db"), plugin_root])
    );

    // server_bad_env is dropped with warning naming CLAUDE_PLUGIN_DATA
    assert!(!read.servers.contains_key("server_bad_env"));
    assert_eq!(
        read.warnings,
        vec![
            "server 'server_bad_env' from plugin test-plugin: env must not set CLAUDE_PLUGIN_DATA (Agent Plugins §9.2); skipped"
        ]
    );
}

#[test]
fn claude_plugin_placeholder_reserved_env_keys_comprehensive() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let plugin_dir = temp_dir.path();

    fs::write(
        plugin_dir.join(".mcp.json"),
        json!({
            "mcpServers": {
                "server_root": {
                    "command": "tool",
                    "args": ["1"],
                    "env": {"CLAUDE_PLUGIN_ROOT": "/bad/root"}
                },
                "server_data": {
                    "command": "tool",
                    "args": ["2"],
                    "env": {"CLAUDE_PLUGIN_DATA": "/bad/data"}
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("reserved-keys-plugin", plugin_dir)
        .unwrap()
        .expect("manifest should be read");

    assert!(!read.servers.contains_key("server_root"));
    assert!(!read.servers.contains_key("server_data"));
    assert!(read
        .warnings
        .iter()
        .any(|w| { w.contains("server 'server_root'") && w.contains("CLAUDE_PLUGIN_ROOT") }));
    assert!(read
        .warnings
        .iter()
        .any(|w| { w.contains("server 'server_data'") && w.contains("CLAUDE_PLUGIN_DATA") }));
}

#[test]
fn claude_plugin_placeholder_command_expansion_and_dot_slash() {
    let _env_guard = crate::test_support::env_guard();
    let temp_dir = tempfile::TempDir::new().unwrap();
    let plugin_dir = temp_dir.path();
    let plugin_root = plugin_dir.display().to_string();
    let plugin_data = ccync_foundation::paths::ccync_data_root()
        .unwrap()
        .join("cmd-plugin")
        .display()
        .to_string();

    fs::write(
        plugin_dir.join(".mcp.json"),
        json!({
            "mcpServers": {
                "s_dotslash": {
                    "command": "./bin/runner",
                    "args": []
                },
                "s_claude_root": {
                    "command": "${CLAUDE_PLUGIN_ROOT}/bin/claude_tool",
                    "args": []
                },
                "s_plugin_root": {
                    "command": "${PLUGIN_ROOT}/bin/plugin_tool",
                    "args": []
                },
                "s_claude_data": {
                    "command": "${CLAUDE_PLUGIN_DATA}/bin/claude_data_tool",
                    "args": []
                },
                "s_plugin_data": {
                    "command": "${PLUGIN_DATA}/bin/plugin_data_tool",
                    "args": []
                },
                "s_verbatim": {
                    "command": "npx",
                    "args": ["-y", "my-tool"]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("cmd-plugin", plugin_dir)
        .unwrap()
        .expect("manifest should be read");

    assert_eq!(
        read.servers["s_dotslash"]["command"],
        json!(plugin_dir.join("bin/runner").display().to_string())
    );
    assert_eq!(
        read.servers["s_claude_root"]["command"],
        json!(format!("{plugin_root}/bin/claude_tool"))
    );
    assert_eq!(
        read.servers["s_plugin_root"]["command"],
        json!(format!("{plugin_root}/bin/plugin_tool"))
    );
    assert_eq!(
        read.servers["s_claude_data"]["command"],
        json!(format!("{plugin_data}/bin/claude_data_tool"))
    );
    assert_eq!(
        read.servers["s_plugin_data"]["command"],
        json!(format!("{plugin_data}/bin/plugin_data_tool"))
    );
    assert_eq!(read.servers["s_verbatim"]["command"], json!("npx"));
}

#[test]
fn claude_plugin_placeholder_single_pass_left_to_right_earliest_wins() {
    let _env_guard = crate::test_support::env_guard();
    let temp_dir = tempfile::TempDir::new().unwrap();
    // Path containing a literal placeholder token to verify no recursive rescan
    let plugin_dir = temp_dir
        .path()
        .join("dir_with_${CLAUDE_PLUGIN_DATA}_in_name");
    fs::create_dir_all(&plugin_dir).unwrap();
    let plugin_root = plugin_dir.display().to_string();
    let plugin_data = ccync_foundation::paths::ccync_data_root()
        .unwrap()
        .join("l2r-plugin")
        .display()
        .to_string();

    fs::write(
        plugin_dir.join(".mcp.json"),
        json!({
            "mcpServers": {
                "stdio": {
                    "command": "tool",
                    "args": [
                        // Earliest match wins left to right:
                        "${CLAUDE_PLUGIN_ROOT}++${CLAUDE_PLUGIN_DATA}++${PLUGIN_ROOT}++${PLUGIN_DATA}",
                        // No rescan: plugin_root itself contains literal "${CLAUDE_PLUGIN_DATA}"
                        "${CLAUDE_PLUGIN_ROOT}/file.txt"
                    ]
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("l2r-plugin", &plugin_dir)
        .unwrap()
        .expect("manifest should be read");

    let server = &read.servers["stdio"];
    assert_eq!(
        server["args"][0],
        json!(format!(
            "{plugin_root}++{plugin_data}++{plugin_root}++{plugin_data}"
        ))
    );
    // Crucial: The substituted string contains "${CLAUDE_PLUGIN_DATA}" from plugin_root,
    // which must NOT have been rescanned and replaced by plugin_data.
    assert_eq!(server["args"][1], json!(format!("{plugin_root}/file.txt")));
}

#[test]
fn leftover_placeholder_warning_lists_supported_fields_in_order() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "stdio": {
                    "command": "tool ${CLAUDE_UNKNOWN_COMMAND}",
                    "args": ["${CLAUDE_UNKNOWN_ARG}"],
                    "env": {"TOKEN": "${CLAUDE_UNKNOWN_ENV}"},
                    "cwd": "${CLAUDE_UNKNOWN_CWD}",
                    "url": "https://example.com/${CLAUDE_UNKNOWN_URL}",
                    "headers": {"Authorization": "Bearer ${CLAUDE_UNKNOWN_HEADER}"},
                    "ignored": "${CLAUDE_UNKNOWN_FIELD}"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("leftover-plugin", temp_dir.path())
        .unwrap()
        .expect("the MCP manifest should be read");

    assert_eq!(
        read.warnings,
        vec![
            "plugin 'leftover-plugin': server 'stdio' has unsupported fields: ignored; ignored",
            "plugin 'leftover-plugin': server 'stdio' has unexpanded placeholders: command=${CLAUDE_UNKNOWN_COMMAND}; args[0]=${CLAUDE_UNKNOWN_ARG}; env.TOKEN=${CLAUDE_UNKNOWN_ENV}; cwd=${CLAUDE_UNKNOWN_CWD}; url=${CLAUDE_UNKNOWN_URL}; headers.Authorization=${CLAUDE_UNKNOWN_HEADER}; server kept"
        ]
    );
}

#[test]
fn leftover_placeholder_tp14_exact_scenario() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "remote": {
                    "url": "${CLAUDE_PLUGIN_ROOT}/x"
                },
                "stdio": {
                    "command": "tool",
                    "args": ["${CLAUDE_PROJECT_DIR}"]
                },
                "no_warning": {
                    "command": "tool",
                    "args": ["${PLUGIN_ROOTDIR}"],
                    "headersHelper": "node ${CLAUDE_PLUGIN_ROOT}/h.mjs"
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("tp14-plugin", temp_dir.path())
        .unwrap()
        .expect("the MCP manifest should be read");

    // remote and stdio each yield exactly one warning naming the field and placeholder.
    assert!(read.warnings.iter().any(|w| w
        == "plugin 'tp14-plugin': server 'remote' has unexpanded placeholders: url=${CLAUDE_PLUGIN_ROOT}; server kept"));
    assert!(read.warnings.iter().any(|w| w
        == "plugin 'tp14-plugin': server 'stdio' has unexpanded placeholders: args[0]=${CLAUDE_PROJECT_DIR}; server kept"));

    // no_warning: ${PLUGIN_ROOTDIR} is not a recognized PLUGIN_ alias (only PLUGIN_ROOT/PLUGIN_DATA
    // are), and headersHelper is a distinct field name from the scanned "headers" — neither
    // produces a leftover-placeholder warning for the 'no_warning' server.
    assert!(!read
        .warnings
        .iter()
        .any(|w| w.contains("server 'no_warning' has unexpanded placeholders")));

    // Servers are kept intact
    assert_eq!(
        read.servers["remote"],
        json!({
            "url": "${CLAUDE_PLUGIN_ROOT}/x"
        })
    );
    assert_eq!(read.servers["stdio"]["command"], "tool");
    assert_eq!(
        read.servers["stdio"]["args"],
        json!(["${CLAUDE_PROJECT_DIR}"])
    );
}

#[test]
fn leftover_placeholder_servers_kept_and_multiple_elements_ordered() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    fs::write(
        temp_dir.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "multi": {
                    "command": "runner",
                    "args": ["${CLAUDE_A}", "literal", "${CLAUDE_B}"],
                    "env": {
                        "ENV_B": "${CLAUDE_ENV_B}",
                        "ENV_A": "${CLAUDE_ENV_A}"
                    },
                    "headers": {
                        "X-Second": "${CLAUDE_HEAD_2}",
                        "X-First": "${CLAUDE_HEAD_1}"
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let read = read_plugin_mcp_servers("multi-test", temp_dir.path())
        .unwrap()
        .expect("the MCP manifest should be read");

    assert_eq!(
        read.warnings,
        vec![
            "plugin 'multi-test': server 'multi' has unexpanded placeholders: args[0]=${CLAUDE_A}; args[2]=${CLAUDE_B}; env.ENV_A=${CLAUDE_ENV_A}; env.ENV_B=${CLAUDE_ENV_B}; headers.X-First=${CLAUDE_HEAD_1}; headers.X-Second=${CLAUDE_HEAD_2}; server kept"
        ]
    );

    // Server must be kept intact in read.servers
    let multi_server = &read.servers["multi"];
    assert_eq!(multi_server["command"], "runner");
    assert_eq!(
        multi_server["args"],
        json!(["${CLAUDE_A}", "literal", "${CLAUDE_B}"])
    );
    assert_eq!(multi_server["env"]["ENV_A"], "${CLAUDE_ENV_A}");
    assert_eq!(multi_server["env"]["ENV_B"], "${CLAUDE_ENV_B}");
    assert_eq!(multi_server["headers"]["X-First"], "${CLAUDE_HEAD_1}");
    assert_eq!(multi_server["headers"]["X-Second"], "${CLAUDE_HEAD_2}");
}
