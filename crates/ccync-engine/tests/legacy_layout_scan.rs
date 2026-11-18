use std::fs;
use std::path::{Path, PathBuf};

const RELOCATED_GETTERS: &[&str] = &[
    "ccync_build_root",
    "machine_config_path",
    "personal_catalog_path",
    "plugins_lock_path",
    "provider_lifecycle_journal_path",
    "tool_state_path",
    "tool_artifacts_root",
    "tool_operation_journal_path",
    "tools_json_path",
    "ccync_cache_root",
    "mcp_input_path",
    "generated_mcp_path",
    "ccync_agents_md_path",
];

#[test]
fn legacy_layout_scan_reports_no_production_violations() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("ccync-engine must be nested in the workspace");
    let violations = scan_workspace(workspace);
    assert!(
        violations.is_empty(),
        "legacy layout references remain in production code:\n{}",
        violations.join("\n")
    );
}

#[test]
fn legacy_layout_scan_detects_seeded_getter_call() {
    assert_seeded_violation("ccync_build_root()", "relocated getter");
}

#[test]
fn legacy_layout_scan_matches_free_functions_not_methods() {
    let method = scan_source(
        "crates/sample/src/example.rs",
        "fn current(layout: Layout) { layout.machine_config_path(); }",
    );
    assert!(method.is_empty(), "unexpected method finding: {method:?}");

    let free_function = scan_source(
        "crates/sample/src/example.rs",
        "fn legacy() { paths::machine_config_path(); }",
    );
    assert_eq!(
        free_function.len(),
        1,
        "expected a free function finding: {free_function:?}"
    );
    assert!(free_function[0].contains("machine_config_path"));
}

#[test]
fn legacy_layout_scan_detects_seeded_build_join() {
    assert_seeded_violation("root.join(\"build\")", "legacy build join");
}

#[test]
fn legacy_layout_scan_detects_seeded_legacy_literal() {
    assert_seeded_violation("let path = \".ccync/build\";", "legacy build literal");
}

#[test]
fn legacy_layout_scan_ignores_commented_getter_call() {
    let source = "// ccync_build_root()\n/* machine_config_path() */\nfn current() {}\n";
    let findings = scan_source("crates/sample/src/example.rs", source);
    assert!(findings.is_empty(), "unexpected findings: {findings:?}");
}

#[test]
fn legacy_layout_scan_ignores_cfg_test_items_and_allowed_live_getters() {
    let source = r#"
#[cfg(test)]
mod tests {
    fn legacy() { machine_config_path(); }
}
fn surface() { claude_config_path(); }
fn render_root() { root.join("build").join("render"); }
fn active_root() { root.join("build").join("active"); }
"#;
    let findings = scan_source("crates/sample/src/example.rs", source);
    assert!(findings.is_empty(), "unexpected findings: {findings:?}");
    let lock = scan_source(
        "crates/sample/src/lock.rs",
        "fn lock() { lifecycle_lock_path(); }",
    );
    assert!(lock.is_empty(), "unexpected lock finding: {lock:?}");
}

#[test]
fn legacy_layout_scan_skips_brace_less_cfg_test_items_before_test_modules() {
    let source = r#"
#[cfg(test)]
static TEST_FLAG: bool = false;

#[cfg(test)]
mod tests {
    fn legacy() {
        machine_config_path();
        let path = ".ccync/build";
    }
}
"#;
    let findings = scan_source("crates/sample/src/example.rs", source);
    assert!(findings.is_empty(), "unexpected findings: {findings:?}");
}

#[test]
fn legacy_layout_scan_does_not_allow_unrelated_paths_modules() {
    let findings = scan_source(
        "crates/sample/src/paths.rs",
        "fn legacy() { machine_config_path(); root.join(\"build\"); }",
    );
    assert_eq!(findings.len(), 2, "unexpected findings: {findings:?}");
}

#[test]
fn legacy_layout_scan_limits_provider_and_lock_exceptions_to_foundation() {
    let allowed_provider = scan_source(
        "crates/ccync-foundation/src/provider.rs",
        "fn legacy() { machine_config_path(); }",
    );
    assert!(
        allowed_provider.is_empty(),
        "unexpected allowed provider finding: {allowed_provider:?}"
    );

    let provider = scan_source(
        "crates/sample/src/provider.rs",
        "fn legacy() { machine_config_path(); }",
    );
    assert_eq!(
        provider.len(),
        1,
        "unexpected provider findings: {provider:?}"
    );

    let lock = scan_source(
        "crates/sample/src/other.rs",
        "fn legacy() { lifecycle_lock_path(); }",
    );
    assert_eq!(lock.len(), 1, "unexpected lock findings: {lock:?}");

    let allowed_lock = scan_source(
        "crates/ccync-engine/src/tool_manager/lock.rs",
        "fn legacy() { lifecycle_lock_path(); }",
    );
    assert!(
        allowed_lock.is_empty(),
        "unexpected allowed lock finding: {allowed_lock:?}"
    );
}

fn assert_seeded_violation(seed: &str, expected: &str) {
    let temp = temporary_workspace();
    let source_path = temp.join("crates/sample/src/example.rs");
    fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    fs::write(&source_path, format!("fn seeded() {{ {seed} }}\n")).unwrap();

    let violations = scan_workspace(&temp);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains(expected)),
        "expected {expected} violation for seed {seed:?}, got {violations:?}"
    );
    fs::remove_dir_all(temp).unwrap();
}

fn temporary_workspace() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ccync-legacy-layout-scan-{}-{id}",
        std::process::id()
    ))
}

fn scan_workspace(workspace: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    let crates = workspace.join("crates");
    let Ok(entries) = fs::read_dir(crates) else {
        return vec![format!(
            "{}: cannot read crates directory",
            workspace.display()
        )];
    };

    for entry in entries.flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            walk_rust_files(workspace, &src, &mut violations);
        }
    }
    violations.sort();
    violations
}

fn walk_rust_files(workspace: &Path, dir: &Path, violations: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        violations.push(format!(
            "{}: cannot read source directory",
            dir.strip_prefix(workspace)
                .unwrap_or(dir)
                .to_string_lossy()
                .replace('\\', "/")
        ));
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rust_files(workspace, &path, violations);
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs")
            || path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("tests.rs"))
        {
            continue;
        }
        let relative = path.strip_prefix(workspace).unwrap_or(&path);
        let relative = relative.to_string_lossy().replace('\\', "/");
        let Ok(source) = fs::read_to_string(&path) else {
            violations.push(format!("{relative}: cannot read source"));
            continue;
        };
        violations.extend(scan_source(&relative, &source));
    }
}

fn scan_source(relative_path: &str, source: &str) -> Vec<String> {
    let mut uncommented = strip_comments(source);
    remove_cfg_test_items(&mut uncommented);
    let mut violations = Vec::new();

    let code = mask_literals(&uncommented);
    if !is_getter_allowlisted(relative_path) {
        for getter in RELOCATED_GETTERS {
            if has_call(&code, getter) {
                violations.push(format!("{relative_path}: relocated getter {getter}"));
            }
        }
        if has_config_path_call(&code) {
            violations.push(format!(
                "{relative_path}: relocated getter CcyncConfig::config_path"
            ));
        }
    }
    if !relative_path.ends_with("/paths.rs")
        && !relative_path.ends_with("/lock.rs")
        && has_call(&code, "lifecycle_lock_path")
    {
        violations.push(format!(
            "{relative_path}: relocated getter lifecycle_lock_path"
        ));
    }

    if !relative_path.ends_with("crates/ccync-foundation/src/paths.rs")
        && !relative_path.ends_with("crates/ccync-engine/src/migrate.rs")
    {
        if has_legacy_build_join(&uncommented) {
            violations.push(format!("{relative_path}: legacy build join"));
        }
        if uncommented.contains(".ccync/build") {
            violations.push(format!("{relative_path}: legacy build literal"));
        }
    }
    violations
}

fn is_getter_allowlisted(path: &str) -> bool {
    path.ends_with("crates/ccync-foundation/src/paths.rs")
        || path.ends_with("crates/ccync-foundation/src/config.rs")
        || path == "crates/ccync-foundation/src/provider.rs"
        || path.ends_with("crates/ccync-engine/src/migrate.rs")
}

fn has_call(code: &str, name: &str) -> bool {
    let bytes = code.as_bytes();
    let name_bytes = name.as_bytes();
    let mut from = 0;
    while let Some(relative) = code[from..].find(name) {
        let start = from + relative;
        let end = start + name_bytes.len();
        let before_ok = start == 0 || !is_ident(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_ident(bytes[end]);
        if before_ok && after_ok {
            let mut previous = start;
            while previous > 0 && bytes[previous - 1].is_ascii_whitespace() {
                previous -= 1;
            }
            if previous > 0 && bytes[previous - 1] == b'.' {
                from = end;
                continue;
            }
            let mut next = end;
            while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                next += 1;
            }
            if bytes.get(next) == Some(&b'(') {
                return true;
            }
        }
        from = end;
    }
    false
}

fn has_config_path_call(code: &str) -> bool {
    let normalized: String = code.chars().filter(|ch| !ch.is_whitespace()).collect();
    normalized.contains("CcyncConfig::config_path(")
}

fn has_legacy_build_join(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut from = 0;
    while let Some(relative) = source[from..].find(".join") {
        let start = from + relative + ".join".len();
        let Some(open) = skip_space(bytes, start) else {
            break;
        };
        if bytes.get(open) != Some(&b'(') {
            from = start;
            continue;
        }
        let Some(close) = simple_string_argument_end(bytes, open, "build") else {
            from = open + 1;
            continue;
        };
        let Some(next_dot) = skip_space(bytes, close + 1) else {
            return true;
        };
        if bytes.get(next_dot) != Some(&b'.') {
            return true;
        }
        let Some(join_start) = bytes[next_dot + 1..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map(|offset| next_dot + 1 + offset)
        else {
            return true;
        };
        if !source[join_start..].starts_with("join") {
            return true;
        }
        let Some(second_open) = skip_space(bytes, join_start + 4) else {
            return true;
        };
        if bytes.get(second_open) != Some(&b'(') {
            return true;
        }
        let continues = simple_string_argument_end(bytes, second_open, "render").is_some()
            || simple_string_argument_end(bytes, second_open, "active").is_some();
        if !continues {
            return true;
        }
        from = close + 1;
    }
    false
}

fn simple_string_argument_end(bytes: &[u8], open: usize, expected: &str) -> Option<usize> {
    let mut index = open + 1;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    index += 1;
    if bytes.get(index..index + expected.len())? != expected.as_bytes() {
        return None;
    }
    index += expected.len();
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    index += 1;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    (bytes.get(index) == Some(&b')')).then_some(index)
}

fn skip_space(bytes: &[u8], mut index: usize) -> Option<usize> {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    (index < bytes.len()).then_some(index)
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn strip_comments(source: &str) -> String {
    let mut bytes = source.as_bytes().to_vec();
    let mut index = 0;
    let mut raw_hashes = None;
    let mut mode = 0_u8;
    let mut block_depth = 0;
    while index < bytes.len() {
        if mode == 1 {
            if bytes[index] == b'\\' {
                index = (index + 2).min(bytes.len());
            } else if bytes[index] == b'"' {
                mode = 0;
                index += 1;
            } else {
                index += 1;
            }
            continue;
        }
        if mode == 2 {
            if bytes[index] == b'\\' {
                index = (index + 2).min(bytes.len());
            } else if bytes[index] == b'\'' {
                mode = 0;
                index += 1;
            } else {
                index += 1;
            }
            continue;
        }
        if mode == 3 {
            if bytes[index] == b'"' {
                let hashes = raw_hashes.unwrap_or_default();
                let end = index + 1 + hashes;
                if bytes
                    .get(index + 1..end)
                    .is_some_and(|suffix| suffix.iter().all(|b| *b == b'#'))
                {
                    mode = 0;
                    index = end;
                    continue;
                }
            }
            index += 1;
            continue;
        }
        if mode == 4 {
            if bytes.get(index..index + 2) == Some(b"/*") {
                block_depth += 1;
                bytes[index] = b' ';
                bytes[index + 1] = b' ';
                index += 2;
            } else if bytes.get(index..index + 2) == Some(b"*/") {
                block_depth -= 1;
                bytes[index] = b' ';
                bytes[index + 1] = b' ';
                index += 2;
                if block_depth == 0 {
                    mode = 0;
                }
            } else {
                if bytes[index] != b'\n' {
                    bytes[index] = b' ';
                }
                index += 1;
            }
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                bytes[index] = b' ';
                index += 1;
            }
        } else if bytes.get(index..index + 2) == Some(b"/*") {
            mode = 4;
            block_depth = 1;
        } else if bytes[index] == b'"' {
            mode = 1;
            index += 1;
        } else if bytes[index] == b'\'' && has_char_literal_end(&bytes, index) {
            mode = 2;
            index += 1;
        } else if bytes[index] == b'r' {
            let mut quote = index + 1;
            while bytes.get(quote) == Some(&b'#') {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'"') {
                mode = 3;
                raw_hashes = Some(quote - index - 1);
                index = quote + 1;
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    String::from_utf8(bytes).expect("comment replacement preserves UTF-8")
}

fn has_char_literal_end(bytes: &[u8], start: usize) -> bool {
    let mut index = start + 1;
    match bytes.get(index) {
        Some(b'\\') => {
            index += 1;
            match bytes.get(index) {
                Some(b'u') if bytes.get(index + 1) == Some(&b'{') => {
                    index += 2;
                    while bytes
                        .get(index)
                        .is_some_and(|byte| *byte != b'}' && *byte != b'\n')
                    {
                        index += 1;
                    }
                    if bytes.get(index) != Some(&b'}') {
                        return false;
                    }
                    index += 1;
                }
                Some(_) => index += 1,
                None => return false,
            }
        }
        Some(byte) if byte.is_ascii() && *byte != b'\n' && *byte != b'\'' => index += 1,
        Some(byte) if !byte.is_ascii() => {
            let width = if byte & 0b1111_1000 == 0b1111_0000 {
                4
            } else if byte & 0b1111_0000 == 0b1110_0000 {
                3
            } else {
                2
            };
            index += width;
        }
        _ => return false,
    }
    bytes.get(index) == Some(&b'\'')
}

fn remove_cfg_test_items(source: &mut String) {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut ranges = Vec::new();
    while let Some(relative) = source[cursor..].find("#") {
        let hash = cursor + relative;
        let Some(open) = skip_space(bytes, hash + 1) else {
            break;
        };
        if bytes.get(open) != Some(&b'[') {
            cursor = hash + 1;
            continue;
        }
        let Some(attribute_end) = matching_delimiter(bytes, open, b'[', b']') else {
            break;
        };
        let attribute: String = source[open + 1..attribute_end]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        cursor = attribute_end + 1;
        if attribute != "cfg(test)" {
            continue;
        }
        let Some(item_start) = skip_space(bytes, cursor) else {
            break;
        };
        let Some(boundary) = find_item_boundary(bytes, item_start) else {
            continue;
        };
        let end = if bytes[boundary] == b'{' {
            matching_delimiter(bytes, boundary, b'{', b'}').map(|end| end + 1)
        } else {
            Some(boundary + 1)
        };
        if let Some(end) = end {
            ranges.push((hash, end));
            cursor = end;
        }
    }
    for (start, end) in ranges.into_iter().rev() {
        for byte in unsafe { source.as_bytes_mut() }[start..end].iter_mut() {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
}

fn find_item_boundary(bytes: &[u8], start: usize) -> Option<usize> {
    let masked = mask_literals_from_bytes(bytes);
    (start..bytes.len()).find(|index| masked[*index] == b'{' || masked[*index] == b';')
}

fn matching_delimiter(bytes: &[u8], open: usize, left: u8, right: u8) -> Option<usize> {
    let masked = mask_literals_from_bytes(bytes);
    let mut depth = 0;
    for (index, byte) in masked.iter().enumerate().skip(open) {
        if *byte == left {
            depth += 1;
        } else if *byte == right {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn mask_literals(source: &str) -> String {
    let mut bytes = source.as_bytes().to_vec();
    mask_literal_bytes(&mut bytes);
    String::from_utf8(bytes).expect("masking preserves UTF-8")
}

fn mask_literals_from_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut masked = bytes.to_vec();
    mask_literal_bytes(&mut masked);
    masked
}

fn mask_literal_bytes(bytes: &mut [u8]) {
    let mut index = 0;
    let mut mode = 0_u8;
    let mut raw_hashes = 0;
    while index < bytes.len() {
        if mode == 1 || mode == 2 {
            let terminator = if mode == 1 { b'"' } else { b'\'' };
            if bytes[index] == b'\\' {
                bytes[index] = b' ';
                if index + 1 < bytes.len() {
                    bytes[index + 1] = b' ';
                }
                index = (index + 2).min(bytes.len());
            } else {
                let close = bytes[index] == terminator;
                if bytes[index] != b'\n' {
                    bytes[index] = b' ';
                }
                index += 1;
                if close {
                    mode = 0;
                }
            }
        } else if mode == 3 {
            if bytes[index] == b'"'
                && bytes
                    .get(index + 1..index + 1 + raw_hashes)
                    .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
            {
                for byte in &mut bytes[index..index + 1 + raw_hashes] {
                    *byte = b' ';
                }
                index += 1 + raw_hashes;
                mode = 0;
            } else {
                if bytes[index] != b'\n' {
                    bytes[index] = b' ';
                }
                index += 1;
            }
        } else if bytes[index] == b'"' {
            bytes[index] = b' ';
            mode = 1;
            index += 1;
        } else if bytes[index] == b'\'' && has_char_literal_end(bytes, index) {
            bytes[index] = b' ';
            mode = 2;
            index += 1;
        } else if bytes[index] == b'r' {
            let mut quote = index + 1;
            while bytes.get(quote) == Some(&b'#') {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'"') {
                raw_hashes = quote - index - 1;
                for byte in &mut bytes[index..=quote] {
                    *byte = b' ';
                }
                mode = 3;
                index = quote + 1;
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
}
