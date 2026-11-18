use crate::host_render::HostEntry;
use ccync_foundation::mcp::McpServer;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn composite_identity(
    shim: &[u8],
    package: &[u8],
    bin: &str,
    payload: &[u8],
    interpreter: &[u8],
    launchers: &[&[u8]],
) -> String {
    use sha2::{Digest, Sha256};
    let mut encoded = Vec::new();
    let mut item = |tag: &str, bytes: &[u8]| {
        encoded.extend_from_slice(&(tag.len() as u32).to_be_bytes());
        encoded.extend_from_slice(tag.as_bytes());
        encoded.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        encoded.extend_from_slice(bytes);
    };
    item("format", b"ccync-win-identity-v1");
    item("shim", shim);
    item("package", package);
    item("bin", bin.as_bytes());
    item("payload", payload);
    item("interpreter", interpreter);
    for (index, launcher) in launchers.iter().enumerate() {
        item(&format!("launcher:{index}"), launcher);
    }
    format!("sha256:{}", hex::encode(Sha256::digest(encoded)))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SupportRow {
    host: String,
    host_version: String,
    host_digest: String,
    entry_version: String,
    command_class: String,
    cwd_class: String,
    evidence_id: String,
    windowless: bool,
    max_attempts: Option<u32>,
    termination_ms: Option<u64>,
    observation_grace_ms: Option<u64>,
    healthy_after_stop: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalFacts {
    host_version: String,
    host_digest: String,
    entry_version: String,
    command_class: String,
    cwd_class: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeStopSettings {
    max_attempts: u32,
    termination_ms: u64,
    observation_grace_ms: u64,
}

fn stop_settings(_host: &str, _host_version: &str) -> Option<NativeStopSettings> {
    None
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

fn canonical_json(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let items: Option<Vec<_>> = keys
                .into_iter()
                .map(|key| {
                    Some(format!(
                        "{}:{}",
                        serde_json::to_string(key).ok()?,
                        canonical_json(&map[key])?
                    ))
                })
                .collect();
            Some(format!("{{{}}}", items?.join(",")))
        }
        serde_json::Value::Array(values) => {
            let items: Option<Vec<_>> = values.iter().map(canonical_json).collect();
            Some(format!("[{}]", items?.join(",")))
        }
        _ => serde_json::to_string(value).ok(),
    }
}

fn host_identity(host: &str, path_dirs: &[PathBuf]) -> Option<(String, String)> {
    if !matches!(host, "claude" | "codex" | "copilot" | "opencode" | "agy") {
        return None;
    }
    for dir in path_dirs {
        for ext in ["exe", "cmd"] {
            let path = dir.join(format!("{host}.{ext}"));
            if path.is_file() {
                return if ext == "exe" {
                    Some((pe_file_version(&path)?, sha256(&std::fs::read(&path).ok()?)))
                } else {
                    npm_identity(&path, host, path_dirs)
                };
            }
        }
    }
    None
}

fn npm_identity(path: &Path, host: &str, path_dirs: &[PathBuf]) -> Option<(String, String)> {
    let shim = std::fs::read(path).ok()?;
    let text = std::str::from_utf8(&shim).ok()?;
    let prefix = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\";
    let target = text.strip_prefix(prefix)?.strip_suffix("\" %*\r\n")?;
    let parts: Vec<_> = target.split('\\').collect();
    let package_parts = if parts.first()?.starts_with('@') {
        2
    } else {
        1
    };
    if parts.len() <= package_parts
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains(['/', ':', '%', '"']))
    {
        return None;
    }
    let base = path.parent()?;
    let package_dir = parts[..package_parts]
        .iter()
        .fold(base.join("node_modules"), |p, part| p.join(part));
    let package = std::fs::read(package_dir.join("package.json")).ok()?;
    let metadata: serde_json::Value = serde_json::from_slice(&package).ok()?;
    let version = metadata
        .get("version")?
        .as_str()
        .filter(|v| !v.is_empty())?;
    let mapping = metadata.get("bin")?;
    let bin = if let Some(value) = mapping.as_str() {
        value
    } else {
        mapping.as_object()?.get(host)?.as_str()?
    };
    let normalized = bin.strip_prefix("./").unwrap_or(bin).replace('/', "\\");
    if normalized != parts[package_parts..].join("\\") {
        return None;
    }
    let payload = std::fs::read(
        parts[package_parts..]
            .iter()
            .fold(package_dir, |p, part| p.join(part)),
    )
    .ok()?;
    let local_node = base.join("node.exe");
    let interpreter_path = if local_node.is_file() {
        local_node
    } else {
        path_dirs
            .iter()
            .map(|p| p.join("node.exe"))
            .find(|p| p.is_file())?
    };
    let interpreter = std::fs::read(interpreter_path).ok()?;
    Some((
        version.to_owned(),
        composite_identity(&shim, &package, bin, &payload, &interpreter, &[]),
    ))
}

#[cfg(windows)]
fn pe_file_version(path: &Path) -> Option<String> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "version")]
    unsafe extern "system" {
        fn GetFileVersionInfoSizeW(filename: *const u16, handle: *mut u32) -> u32;
        fn GetFileVersionInfoW(
            filename: *const u16,
            handle: u32,
            len: u32,
            data: *mut c_void,
        ) -> i32;
        fn VerQueryValueW(
            block: *const c_void,
            sub: *const u16,
            value: *mut *mut c_void,
            len: *mut u32,
        ) -> i32;
    }
    let filename: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(filename.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(filename.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let query = |name: &str| -> Option<(*mut c_void, u32)> {
            let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            let mut ptr = std::ptr::null_mut();
            let mut len = 0;
            if VerQueryValueW(data.as_ptr().cast(), name.as_ptr(), &mut ptr, &mut len) == 0
                || ptr.is_null()
            {
                None
            } else {
                Some((ptr, len))
            }
        };
        let (translations, len) = query("\\VarFileInfo\\Translation")?;
        if len != 4 {
            return None;
        }
        let lang = std::ptr::read_unaligned(translations.cast::<u16>());
        let codepage = std::ptr::read_unaligned(translations.cast::<u16>().add(1));
        let (value, len) = query(&format!(
            "\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileVersion"
        ))?;
        if len < 2 {
            return None;
        }
        let chars = std::slice::from_raw_parts(value.cast::<u16>(), len as usize);
        if chars.last() != Some(&0) || chars[..chars.len() - 1].contains(&0) {
            return None;
        }
        String::from_utf16(&chars[..chars.len() - 1])
            .ok()
            .filter(|v| !v.is_empty())
    }
}

#[cfg(not(windows))]
fn pe_file_version(_path: &Path) -> Option<String> {
    None
}

fn path_class(path: &str, allow_bare: bool) -> Option<&'static str> {
    if path.is_empty() {
        return None;
    }
    if Path::new(path).is_absolute() {
        Some("absolute")
    } else if allow_bare && !path.contains(['/', '\\']) {
        Some("bare")
    } else {
        Some("relative")
    }
}

fn local_facts(
    host: &str,
    server: &McpServer,
    name: &str,
    entry: &HostEntry,
    path_dirs: &[PathBuf],
) -> Option<LocalFacts> {
    let (host_version, host_digest) = host_identity(host, path_dirs)?;
    let value = match entry {
        HostEntry::Json(value) => value.clone(),
        HostEntry::CodexToml(text) => {
            let value: toml::Value = toml::from_str(text).ok()?;
            serde_json::to_value(value.get("mcp_servers")?.get(name)?).ok()?
        }
    };
    if !value.is_object() {
        return None;
    }
    Some(LocalFacts {
        host_version,
        host_digest,
        entry_version: sha256(canonical_json(&value)?.as_bytes()),
        command_class: path_class(server.command.as_deref()?, true)?.to_owned(),
        cwd_class: match server.cwd.as_deref() {
            Some(cwd) => path_class(cwd, false)?,
            None => "none",
        }
        .to_owned(),
    })
}

fn match_row<'a>(
    rows: &'a [SupportRow],
    host: &str,
    facts: Option<&LocalFacts>,
    settings_source: fn(&str, &str) -> Option<NativeStopSettings>,
) -> Option<&'a str> {
    facts
        .and_then(|facts| {
            rows.iter().find(|row| {
                settings_source(host, &facts.host_version).is_some_and(|settings| {
                    row.max_attempts == Some(settings.max_attempts)
                        && row.termination_ms == Some(settings.termination_ms)
                        && row.observation_grace_ms == Some(settings.observation_grace_ms)
                }) && row.host == host
                    && row.host_version == facts.host_version
                    && !row.host_version.is_empty()
                    && row.host_digest == facts.host_digest
                    && !row.host_digest.is_empty()
                    && row.entry_version == facts.entry_version
                    && !row.entry_version.is_empty()
                    && row.command_class == facts.command_class
                    && !row.command_class.is_empty()
                    && row.cwd_class == facts.cwd_class
                    && !row.cwd_class.is_empty()
                    && !row.evidence_id.is_empty()
                    && row.windowless
                    && row.max_attempts.is_some_and(|value| value > 0)
                    && row.termination_ms.is_some()
                    && row.observation_grace_ms.is_some()
                    && row.healthy_after_stop
            })
        })
        .map(|row| row.evidence_id.as_str())
}

pub(crate) fn decision(
    host: &str,
    server: &McpServer,
    name: &str,
    entry: &HostEntry,
) -> Option<String> {
    let rows: Vec<SupportRow> =
        serde_json::from_str(include_str!("windows_support.json")).unwrap_or_default();
    let dirs: Vec<_> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    let facts = local_facts(host, server, name, entry, &dirs);
    match_row(&rows, host, facts.as_ref(), stop_settings).map(str::to_owned)
}

pub(crate) fn blocked_reason(host: &str) -> &'static str {
    let rows: Vec<SupportRow> =
        serde_json::from_str(include_str!("windows_support.json")).unwrap_or_default();
    blocked_reason_for_rows(&rows, host)
}

fn blocked_reason_for_rows(rows: &[SupportRow], host: &str) -> &'static str {
    if rows.iter().any(|row| row.host == host) {
        "Windows stdio qualification: missing facts"
    } else {
        "Windows stdio qualification: no qualification row"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_settings(_: &str, _: &str) -> Option<NativeStopSettings> {
        Some(NativeStopSettings {
            max_attempts: 2,
            termination_ms: 100,
            observation_grace_ms: 100,
        })
    }

    #[test]
    fn resolves_path_candidates_and_npm_chain() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![dir.path().to_path_buf()];
        assert_eq!(host_identity("claude", &paths), None);
        let shim_path = dir.path().join("claude.cmd");
        std::fs::write(&shim_path, b"unknown").unwrap();
        assert_eq!(host_identity("claude", &paths), None);
        let shim = "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\demo\\bin\\cli.js\" %*\r\n";
        std::fs::write(&shim_path, shim).unwrap();
        let package = dir.path().join("node_modules/demo");
        std::fs::create_dir_all(package.join("bin")).unwrap();
        std::fs::write(
            package.join("package.json"),
            br#"{"version":"1.0","bin":{"claude":"bin/cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(package.join("bin/cli.js"), b"payload").unwrap();
        std::fs::write(dir.path().join("node.exe"), b"node").unwrap();
        let identity = host_identity("claude", &paths).unwrap();
        assert_eq!(identity.0, "1.0");
        let mut server: McpServer =
            serde_json::from_value(serde_json::json!({"command":"node"})).unwrap();
        let entry = HostEntry::Json(serde_json::json!({"type":"stdio","command":"node","args":[]}));
        let facts = local_facts("claude", &server, "example", &entry, &paths).unwrap();
        assert_eq!(facts.cwd_class, "none");
        assert_eq!(facts.command_class, "bare");
        assert_eq!(
            facts.entry_version,
            "sha256:dd2e536fd1ce22738b251b6fa2264b2b1b392c2f4378d9430bb7964632ab450a"
        );
        server.cwd = Some("work".into());
        assert_eq!(
            local_facts("claude", &server, "example", &entry, &paths)
                .unwrap()
                .cwd_class,
            "relative"
        );
        std::fs::write(package.join("bin/cli.js"), b"changed").unwrap();
        assert_ne!(identity, host_identity("claude", &paths).unwrap());
        std::fs::write(dir.path().join("claude.exe"), b"not a PE").unwrap();
        assert_eq!(host_identity("claude", &paths), None);
    }

    #[test]
    fn classifies_paths_and_requires_production_settings() {
        assert_eq!(path_class("node", true), Some("bare"));
        assert_eq!(path_class("dir/node", true), Some("relative"));
        assert_eq!(path_class("dir", false), Some("relative"));
        assert_eq!(path_class("", false), None);
        let absolute = std::env::current_dir().unwrap();
        assert_eq!(
            path_class(absolute.to_str().unwrap(), true),
            Some("absolute")
        );
        assert_eq!(
            path_class(absolute.to_str().unwrap(), false),
            Some("absolute")
        );
        let (row, facts) = sample();
        assert_eq!(stop_settings("claude", "1"), None);
        assert_eq!(
            match_row(&[row.clone()], "claude", Some(&facts), stop_settings),
            None
        );
        assert_eq!(
            match_row(&[row], "claude", Some(&facts), synthetic_settings),
            Some("evidence")
        );
    }

    #[test]
    fn composite_identity_golden_vector_and_drift() {
        let digest = composite_identity(
            b"@echo shim\r\n",
            br#"{"name":"demo","version":"1.0.0"}"#,
            "bin/demo.js",
            b"console.log(1)\n",
            b"MZnode",
            &[],
        );
        assert_eq!(
            digest,
            "sha256:df2ad062517432a02a838ff8682c46f2ee1727d5c815c387c6127383ff17d516"
        );
        assert_ne!(
            digest,
            composite_identity(
                b"@echo shim\r\n",
                br#"{"name":"demo","version":"1.0.0"}"#,
                "bin/demo.js",
                b"console.log(1)\n",
                b"MZnode",
                &[b"launcher"]
            )
        );
    }

    fn sample() -> (SupportRow, LocalFacts) {
        let row = SupportRow {
            host: "claude".into(),
            host_version: "1".into(),
            host_digest: "digest".into(),
            entry_version: "1".into(),
            command_class: "native".into(),
            cwd_class: "inherited".into(),
            evidence_id: "evidence".into(),
            windowless: true,
            max_attempts: Some(2),
            termination_ms: Some(100),
            observation_grace_ms: Some(100),
            healthy_after_stop: true,
        };
        let facts = LocalFacts {
            host_version: row.host_version.clone(),
            host_digest: row.host_digest.clone(),
            entry_version: row.entry_version.clone(),
            command_class: row.command_class.clone(),
            cwd_class: row.cwd_class.clone(),
        };
        (row, facts)
    }

    #[test]
    fn reports_missing_row_or_facts() {
        assert_eq!(
            blocked_reason_for_rows(&[], "claude"),
            "Windows stdio qualification: no qualification row"
        );
        assert_eq!(
            blocked_reason_for_rows(&[sample().0], "claude"),
            "Windows stdio qualification: missing facts"
        );
    }

    #[test]
    fn requires_exact_facts_and_complete_evidence() {
        let (row, facts) = sample();
        assert_eq!(
            match_row(&[row.clone()], "claude", Some(&facts), synthetic_settings),
            Some("evidence")
        );
        let mut changed = facts.clone();
        changed.host_version = "1.1".into();
        assert_eq!(
            match_row(&[row.clone()], "claude", Some(&changed), synthetic_settings),
            None
        );
        changed = facts.clone();
        changed.cwd_class = "relative".into();
        assert_eq!(
            match_row(&[row.clone()], "claude", Some(&changed), synthetic_settings),
            None
        );
        let mut missing_stop = row;
        missing_stop.max_attempts = None;
        assert_eq!(
            match_row(&[missing_stop], "claude", Some(&facts), synthetic_settings),
            None
        );
        assert_eq!(match_row(&[], "claude", None, synthetic_settings), None);
    }

    #[test]
    fn rejects_each_fact_drift_and_missing_evidence() {
        let (row, facts) = sample();
        assert_eq!(
            match_row(&[row.clone()], "codex", Some(&facts), synthetic_settings),
            None
        );
        assert_eq!(
            match_row(&[row.clone()], "claude", None, synthetic_settings),
            None
        );
        for field in 0..5 {
            let mut changed = facts.clone();
            match field {
                0 => changed.host_version.clear(),
                1 => changed.host_digest.push('x'),
                2 => changed.entry_version.push('x'),
                3 => changed.command_class.push('x'),
                _ => changed.cwd_class.push('x'),
            }
            assert_eq!(
                match_row(&[row.clone()], "claude", Some(&changed), synthetic_settings),
                None
            );
        }
        for field in 0..6 {
            let mut incomplete = row.clone();
            match field {
                0 => incomplete.evidence_id.clear(),
                1 => incomplete.windowless = false,
                2 => incomplete.max_attempts = Some(0),
                3 => incomplete.termination_ms = None,
                4 => incomplete.observation_grace_ms = None,
                _ => incomplete.healthy_after_stop = false,
            }
            assert_eq!(
                match_row(&[incomplete], "claude", Some(&facts), synthetic_settings),
                None
            );
        }
        for field in 0..5 {
            let mut empty_row = row.clone();
            let mut empty_facts = facts.clone();
            match field {
                0 => {
                    empty_row.host_version.clear();
                    empty_facts.host_version.clear();
                }
                1 => {
                    empty_row.host_digest.clear();
                    empty_facts.host_digest.clear();
                }
                2 => {
                    empty_row.entry_version.clear();
                    empty_facts.entry_version.clear();
                }
                3 => {
                    empty_row.command_class.clear();
                    empty_facts.command_class.clear();
                }
                _ => {
                    empty_row.cwd_class.clear();
                    empty_facts.cwd_class.clear();
                }
            }
            assert_eq!(
                match_row(
                    &[empty_row],
                    "claude",
                    Some(&empty_facts),
                    synthetic_settings
                ),
                None
            );
        }
    }
}
