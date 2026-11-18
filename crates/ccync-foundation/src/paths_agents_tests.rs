use std::path::PathBuf;

fn home_var() -> &'static str {
    if cfg!(windows) {
        "USERPROFILE"
    } else {
        "HOME"
    }
}

fn filesystem_snapshot(root: &std::path::Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if entry.file_type().unwrap().is_dir() {
                out.push((relative, Vec::new()));
                visit(root, &path, out);
            } else if entry.file_type().unwrap().is_file() {
                out.push((relative, std::fs::read(path).unwrap()));
            } else {
                out.push((relative, Vec::new()));
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

#[test]
fn effective_layout_loads_authority_and_path_matrix_without_writes() {
    use crate::paths::{EffectiveLayout, EffectiveLayoutAuthority, RootPlatform, StorageRoots};
    use crate::provider::{LayoutRootIdentity, StorageLayoutCommit};
    use std::collections::HashMap;

    fn fixture(base: &std::path::Path) -> (HashMap<String, PathBuf>, StorageRoots, PathBuf) {
        let home = base.join("home");
        let config = base.join("config");
        let state = base.join("state");
        let cache = base.join("cache");
        for path in [&home, &config, &state, &cache] {
            std::fs::create_dir_all(path).unwrap();
        }
        let env = [
            ("HOME".to_string(), home.clone()),
            ("XDG_CONFIG_HOME".to_string(), config),
            ("XDG_STATE_HOME".to_string(), state),
            ("XDG_CACHE_HOME".to_string(), cache),
        ]
        .into_iter()
        .collect::<HashMap<_, _>>();
        let roots = StorageRoots::resolve_for(RootPlatform::Unix, &env).unwrap();
        (env, roots, home)
    }

    fn commit(roots: &StorageRoots) -> StorageLayoutCommit {
        StorageLayoutCommit {
            layout_version: 1,
            roots: vec![
                LayoutRootIdentity {
                    name: "portable".into(),
                    path: roots.portable.clone(),
                },
                LayoutRootIdentity {
                    name: "config".into(),
                    path: roots.config.clone(),
                },
                LayoutRootIdentity {
                    name: "state".into(),
                    path: roots.state.clone(),
                },
                LayoutRootIdentity {
                    name: "cache".into(),
                    path: roots.cache.clone(),
                },
            ],
        }
    }

    let fresh_temp = tempfile::TempDir::new().unwrap();
    let (fresh_env, fresh_roots, fresh_home) = fixture(fresh_temp.path());
    std::fs::create_dir_all(fresh_home.join(".ccync").join("build")).unwrap();
    std::fs::write(
        fresh_home
            .join(".ccync")
            .join("build")
            .join("lifecycle.lock"),
        b"ignored",
    )
    .unwrap();
    std::fs::create_dir_all(fresh_home.join(".ccync").join("install-receipts")).unwrap();
    std::fs::write(
        fresh_home
            .join(".ccync")
            .join("install-receipts")
            .join("receipt.json"),
        b"ignored",
    )
    .unwrap();
    std::fs::create_dir_all(fresh_roots.state.join("empty")).unwrap();
    let before = filesystem_snapshot(fresh_temp.path());
    let fresh = EffectiveLayout::load_for(RootPlatform::Unix, &fresh_env).unwrap();
    assert_eq!(fresh.authority(), EffectiveLayoutAuthority::Fresh);
    assert_eq!(fresh.state_base(), fresh_home.join(".ccync").join("build"));
    assert_eq!(fresh.plugins_path(), fresh_home.join(".ccync/plugins.json"));
    assert_eq!(fresh.config_path(), fresh_home.join(".ccync/config.json"));
    assert_eq!(fresh.agents_md_path(), fresh_home.join(".ccync/AGENTS.md"));
    assert_eq!(fresh.mcp_path(), fresh_home.join(".ccync/mcp.json"));
    assert_eq!(fresh.tools_path(), fresh_home.join(".ccync/tools.json"));
    assert_eq!(
        fresh.build_lock_path(),
        fresh_home.join(".ccync/build/lock.json")
    );
    assert_eq!(fresh.sources_export_path(), None);
    assert_eq!(fresh.lock_export_path(), None);
    assert_eq!(
        fresh.state_lock_path(),
        fresh_roots.state.join("lifecycle.lock")
    );
    assert_eq!(
        fresh.legacy_lock_path(),
        fresh_home.join(".ccync/build/lifecycle.lock")
    );
    assert_eq!(fresh.layout_path(), fresh_roots.state.join("layout.json"));
    assert_eq!(
        fresh.migration_journal_path(),
        fresh_roots.state.join("migration.json")
    );
    assert_eq!(filesystem_snapshot(fresh_temp.path()), before);

    let legacy_temp = tempfile::TempDir::new().unwrap();
    let (legacy_env, _, legacy_home) = fixture(legacy_temp.path());
    std::fs::create_dir_all(legacy_home.join(".ccync")).unwrap();
    std::fs::write(legacy_home.join(".ccync").join("config.json"), b"legacy").unwrap();
    let before = filesystem_snapshot(legacy_temp.path());
    let legacy = EffectiveLayout::load_for(RootPlatform::Unix, &legacy_env).unwrap();
    assert_eq!(legacy.authority(), EffectiveLayoutAuthority::Legacy);
    assert_eq!(legacy.config_path(), legacy_home.join(".ccync/config.json"));
    assert_eq!(legacy.sources_export_path(), None);
    assert_eq!(legacy.lock_export_path(), None);
    assert_eq!(filesystem_snapshot(legacy_temp.path()), before);

    let committed_temp = tempfile::TempDir::new().unwrap();
    let (committed_env, committed_roots, committed_home) = fixture(committed_temp.path());
    let serialized = serde_json::to_vec(&commit(&committed_roots)).unwrap();
    std::fs::create_dir_all(committed_home.join(".ccync")).unwrap();
    std::fs::create_dir_all(&committed_roots.state).unwrap();
    std::fs::write(committed_home.join(".ccync/migration.json"), &serialized).unwrap();
    std::fs::write(committed_roots.state.join("layout.json"), &serialized).unwrap();
    let before = filesystem_snapshot(committed_temp.path());
    let committed = EffectiveLayout::load_for(RootPlatform::Unix, &committed_env).unwrap();
    assert_eq!(committed.authority(), EffectiveLayoutAuthority::Committed);
    assert_eq!(committed.state_base(), committed_roots.state.join("build"));
    assert_eq!(
        committed.plugins_path(),
        committed_roots.config.join("plugins.json")
    );
    assert_eq!(
        committed.config_path(),
        committed_roots.config.join("config.json")
    );
    assert_eq!(
        committed.agents_md_path(),
        committed_roots.config.join("AGENTS.md")
    );
    assert_eq!(
        committed.mcp_path(),
        committed_roots.config.join("mcp.json")
    );
    assert_eq!(
        committed.tools_path(),
        committed_roots.config.join("tools.json")
    );
    assert_eq!(
        committed.build_lock_path(),
        committed_roots.state.join("build/lock.json")
    );
    assert_eq!(
        committed.sources_export_path(),
        Some(committed_roots.portable.join("sources.json"))
    );
    assert_eq!(
        committed.lock_export_path(),
        Some(committed_roots.portable.join("lock.json"))
    );
    assert_eq!(
        committed.cache_path(),
        committed_roots.state.join("sources")
    );
    assert_eq!(committed.dist_path(), committed_roots.state.join("dist"));
    assert_eq!(
        committed.backups_path(),
        committed_roots.state.join("backups")
    );
    assert_eq!(filesystem_snapshot(committed_temp.path()), before);

    let blocked_temp = tempfile::TempDir::new().unwrap();
    let (blocked_env, blocked_roots, blocked_home) = fixture(blocked_temp.path());
    std::fs::create_dir_all(blocked_home.join(".ccync")).unwrap();
    std::fs::write(blocked_home.join(".ccync/migration.json"), b"not json").unwrap();
    let before = filesystem_snapshot(blocked_temp.path());
    let blocked = EffectiveLayout::load_for(RootPlatform::Unix, &blocked_env).unwrap();
    assert_eq!(
        blocked.authority(),
        EffectiveLayoutAuthority::MutationBlocked
    );
    assert_eq!(blocked.state_base(), blocked_home.join(".ccync/build"));
    assert_eq!(
        blocked.layout_path(),
        blocked_roots.state.join("layout.json")
    );
    assert_eq!(filesystem_snapshot(blocked_temp.path()), before);

    let invalid_temp = tempfile::TempDir::new().unwrap();
    let (mut invalid_env, _, invalid_home) = fixture(invalid_temp.path());
    invalid_env.insert("XDG_CONFIG_HOME".into(), PathBuf::from("relative"));
    std::fs::create_dir_all(invalid_home.join(".ccync")).unwrap();
    std::fs::write(invalid_home.join(".ccync/legacy"), b"data").unwrap();
    let before = filesystem_snapshot(invalid_temp.path());
    let invalid_legacy = EffectiveLayout::load_for(RootPlatform::Unix, &invalid_env).unwrap();
    assert_eq!(invalid_legacy.authority(), EffectiveLayoutAuthority::Legacy);
    assert!(invalid_legacy.root_error().is_some());
    assert_eq!(
        invalid_legacy.state_base(),
        invalid_home.join(".ccync/build")
    );
    assert_eq!(filesystem_snapshot(invalid_temp.path()), before);

    let invalid_sentinel_temp = tempfile::TempDir::new().unwrap();
    let (mut invalid_sentinel_env, _, invalid_sentinel_home) =
        fixture(invalid_sentinel_temp.path());
    invalid_sentinel_env.insert("XDG_CONFIG_HOME".into(), PathBuf::from("relative"));
    std::fs::create_dir_all(invalid_sentinel_home.join(".ccync")).unwrap();
    std::fs::write(invalid_sentinel_home.join(".ccync/migration.json"), b"{}").unwrap();
    let before = filesystem_snapshot(invalid_sentinel_temp.path());
    let invalid_blocked =
        EffectiveLayout::load_for(RootPlatform::Unix, &invalid_sentinel_env).unwrap();
    assert_eq!(
        invalid_blocked.authority(),
        EffectiveLayoutAuthority::MutationBlocked
    );
    assert!(invalid_blocked.root_error().is_some());
    assert_eq!(filesystem_snapshot(invalid_sentinel_temp.path()), before);

    let invalid_fresh_temp = tempfile::TempDir::new().unwrap();
    let (mut invalid_fresh_env, _, _) = fixture(invalid_fresh_temp.path());
    invalid_fresh_env.insert("XDG_CONFIG_HOME".into(), PathBuf::from("relative"));
    let before = filesystem_snapshot(invalid_fresh_temp.path());
    assert!(EffectiveLayout::load_for(RootPlatform::Unix, &invalid_fresh_env).is_err());
    assert_eq!(filesystem_snapshot(invalid_fresh_temp.path()), before);

    // Existing path getters intentionally continue to resolve the legacy map.
    let _guard = crate::test_support::env_guard();
    let previous = [
        (home_var(), std::env::var_os(home_var())),
        ("XDG_CONFIG_HOME", std::env::var_os("XDG_CONFIG_HOME")),
        ("XDG_STATE_HOME", std::env::var_os("XDG_STATE_HOME")),
        ("XDG_CACHE_HOME", std::env::var_os("XDG_CACHE_HOME")),
    ];
    std::env::set_var(home_var(), &committed_home);
    std::env::set_var(
        "XDG_CONFIG_HOME",
        committed_temp.path().join("getter-config"),
    );
    std::env::set_var("XDG_STATE_HOME", committed_temp.path().join("getter-state"));
    std::env::set_var("XDG_CACHE_HOME", committed_temp.path().join("getter-cache"));
    assert_eq!(
        crate::paths::ccync_build_root(),
        Some(committed_home.join(".ccync/build"))
    );
    assert_eq!(
        crate::paths::lifecycle_lock_path(),
        Some(committed_home.join(".ccync/build/lifecycle.lock"))
    );
    for (key, value) in previous {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    println!(
        "effective_layout evidence: Fresh/Legacy/Committed/MutationBlocked; invalid roots Legacy+MutationBlocked+error; accessors checked; fixture snapshots unchanged"
    );
}

#[test]
fn ccync_agents_md_path_derives_from_ccync_home() {
    let _guard = crate::test_support::env_guard();
    let var = home_var();
    let prior = std::env::var_os(var);
    let fake_home = tempfile::TempDir::new().unwrap();

    std::env::set_var(var, fake_home.path());
    assert_eq!(
        crate::paths::ccync_agents_md_path(),
        Some(fake_home.path().join(".ccync").join("AGENTS.md"))
    );

    match prior {
        Some(value) => std::env::set_var(var, value),
        None => std::env::remove_var(var),
    }
}

#[test]
fn ccync_agents_md_path_is_none_without_home() {
    let _guard = crate::test_support::env_guard();
    let var = home_var();
    let prior = std::env::var_os(var);

    std::env::remove_var(var);
    assert_eq!(crate::paths::ccync_agents_md_path(), None::<PathBuf>);

    match prior {
        Some(value) => std::env::set_var(var, value),
        None => std::env::remove_var(var),
    }
}

#[test]
fn portable_roots_resolve_without_writes_and_reject_aliases() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    use crate::paths::{RootPlatform, StorageRoots};
    let resolve = |platform, entries: &[(&str, PathBuf)]| {
        StorageRoots::resolve_for(
            platform,
            &entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        )
    };
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let cache = temp.path().join("cache");
    for path in [&config, &state, &cache] {
        std::fs::create_dir_all(path).unwrap();
    }
    let roots = resolve(
        RootPlatform::Unix,
        &[
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", config.clone()),
            ("XDG_STATE_HOME", state.clone()),
            ("XDG_CACHE_HOME", cache.clone()),
        ],
    )
    .unwrap();
    assert_eq!(roots.portable, home.join(".agents").join("ccync"));
    assert_eq!(roots.config, config.join("ccync"));
    assert_eq!(roots.state, state.join("ccync"));
    assert_eq!(roots.cache, cache.join("ccync"));
    assert!(
        !roots.portable.exists()
            && !roots.config.exists()
            && !roots.state.exists()
            && !roots.cache.exists()
    );

    std::fs::write(temp.path().join("sentinel"), b"unchanged").unwrap();
    let filesystem_before = filesystem_snapshot(temp.path());
    assert!(matches!(
        resolve(
            RootPlatform::Unix,
            &[
                ("HOME", home.clone()),
                ("XDG_CONFIG_HOME", PathBuf::from("relative"))
            ]
        ),
        Err(crate::paths::RootValidationError::InvalidRoot(_))
    ));
    assert!(matches!(
        resolve(
            RootPlatform::Windows,
            &[("USERPROFILE", home.clone()), ("APPDATA", PathBuf::new())]
        ),
        Err(crate::paths::RootValidationError::InvalidRoot(_))
    ));
    assert_eq!(filesystem_snapshot(temp.path()), filesystem_before);
    let empty_xdg = resolve(
        RootPlatform::Unix,
        &[("HOME", home.clone()), ("XDG_CONFIG_HOME", PathBuf::new())],
    )
    .unwrap();
    assert_eq!(empty_xdg.config, home.join(".config").join("ccync"));

    let windows_defaults =
        resolve(RootPlatform::Windows, &[("USERPROFILE", home.clone())]).unwrap();
    assert_eq!(
        windows_defaults.config,
        home.join("AppData").join("Roaming").join("ccync")
    );
    assert_eq!(
        windows_defaults.state,
        home.join("AppData")
            .join("Local")
            .join("ccync")
            .join("state")
    );
    assert_eq!(
        windows_defaults.cache,
        home.join("AppData")
            .join("Local")
            .join("ccync")
            .join("cache")
    );
    let windows_overrides = resolve(
        RootPlatform::Windows,
        &[
            ("USERPROFILE", home.clone()),
            ("APPDATA", config.clone()),
            ("LOCALAPPDATA", state.clone()),
        ],
    )
    .unwrap();
    assert_eq!(windows_overrides.config, config.join("ccync"));
    assert_eq!(windows_overrides.state, state.join("ccync").join("state"));
    assert_eq!(windows_overrides.cache, state.join("ccync").join("cache"));
    let unix_defaults = resolve(RootPlatform::Unix, &[("HOME", home.clone())]).unwrap();
    assert_eq!(unix_defaults.config, home.join(".config").join("ccync"));
    assert_eq!(
        unix_defaults.state,
        home.join(".local").join("state").join("ccync")
    );
    assert_eq!(unix_defaults.cache, home.join(".cache").join("ccync"));

    // Overlap is rejected under both platform comparison rules.
    for platform in [RootPlatform::Windows, RootPlatform::Unix] {
        let vals = if platform == RootPlatform::Windows {
            vec![
                ("USERPROFILE", home.clone()),
                ("APPDATA", config.clone()),
                ("LOCALAPPDATA", config.clone()),
            ]
        } else {
            vec![
                ("HOME", home.clone()),
                ("XDG_CONFIG_HOME", config.clone()),
                ("XDG_STATE_HOME", config.clone()),
                ("XDG_CACHE_HOME", cache.clone()),
            ]
        };
        assert!(matches!(
            resolve(platform, &vals),
            Err(crate::paths::RootValidationError::OverlappingRoots(_, _))
        ));
    }
    // Rejection compares conservatively even on case-sensitive filesystems.
    let case_alias = resolve(
        RootPlatform::Unix,
        &[
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", home.join(".AGENTS")),
            ("XDG_STATE_HOME", state.clone()),
            ("XDG_CACHE_HOME", cache.clone()),
        ],
    );
    assert!(matches!(
        case_alias,
        Err(crate::paths::RootValidationError::OverlappingRoots(_, _))
    ));
    assert_eq!(filesystem_snapshot(temp.path()), filesystem_before);
    // Windows path comparison folds case even when exercised on a Unix host.
    let upper = temp.path().join("Alias");
    let lower = temp.path().join("alias");
    let win_alias = [
        ("USERPROFILE", home.clone()),
        ("APPDATA", upper),
        ("LOCALAPPDATA", lower),
    ];
    assert!(matches!(
        resolve(RootPlatform::Windows, &win_alias),
        Err(crate::paths::RootValidationError::OverlappingRoots(_, _))
    ));
    assert_eq!(filesystem_snapshot(temp.path()), filesystem_before);

    // Windows APIs treat a missing component ending in a dot or space as its
    // trimmed spelling. Reject it before it can alias the portable root.
    let absent_agents = home.join(".agents");
    assert!(!absent_agents.exists());
    let windows_ambiguous = resolve(
        RootPlatform::Windows,
        &[
            ("USERPROFILE", home.clone()),
            ("APPDATA", home.join(".agents.")),
        ],
    );
    assert!(matches!(
        windows_ambiguous,
        Err(crate::paths::RootValidationError::InvalidRoot(_))
    ));
    assert_eq!(filesystem_snapshot(temp.path()), filesystem_before);

    // Empty required roots and missing home are invalid without filesystem changes.
    assert!(matches!(
        resolve(RootPlatform::Unix, &[("HOME", PathBuf::new())]),
        Err(crate::paths::RootValidationError::InvalidRoot(_))
    ));
    assert!(matches!(
        resolve(RootPlatform::Unix, &[]),
        Err(crate::paths::RootValidationError::MissingRoot)
    ));
    assert_eq!(filesystem_snapshot(temp.path()), filesystem_before);

    // Missing root components use portable ASCII names, including when a
    // Windows platform is injected on a host with different path rules.
    for component in [
        "café-state",
        "cafe\u{301}-state",
        "cache space",
        "cache~name",
    ] {
        let rejected = resolve(
            RootPlatform::Windows,
            &[
                ("USERPROFILE", home.clone()),
                ("LOCALAPPDATA", temp.path().join(component)),
            ],
        );
        assert!(
            matches!(
                rejected,
                Err(crate::paths::RootValidationError::InvalidRoot(_))
            ),
            "component {component:?} should be rejected"
        );
        assert_eq!(filesystem_snapshot(temp.path()), filesystem_before);
    }

    let existing_unicode = temp.path().join("existing-café");
    std::fs::create_dir_all(&existing_unicode).unwrap();
    let before_existing_unicode = filesystem_snapshot(temp.path());
    let accepted_existing_unicode = resolve(
        RootPlatform::Windows,
        &[
            ("USERPROFILE", home.clone()),
            ("APPDATA", existing_unicode.clone()),
        ],
    )
    .unwrap();
    assert_eq!(
        accepted_existing_unicode.config,
        existing_unicode.join("ccync")
    );
    assert_eq!(filesystem_snapshot(temp.path()), before_existing_unicode);
    println!(
        "portable_roots evidence: unix={:?}; windows_fallback={:?}; windows_override={:?}; invalid/alias cases rejected; fixture_entries={} unchanged=true",
        (roots.portable, roots.config, roots.state, roots.cache),
        (windows_defaults.portable, windows_defaults.config, windows_defaults.state, windows_defaults.cache),
        (windows_overrides.portable, windows_overrides.config, windows_overrides.state, windows_overrides.cache),
        filesystem_before.len(),
    );
}

#[test]
fn machine_roots_agree() {
    use crate::paths::{RootPlatform, StorageRoots};
    use crate::provider::PathRoot;
    use std::collections::HashMap;

    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let cache = temp.path().join("cache");
    for path in [&home, &config, &state, &cache] {
        std::fs::create_dir_all(path).unwrap();
    }

    for (platform, entries) in [
        (
            RootPlatform::Windows,
            vec![
                ("USERPROFILE", home.clone()),
                ("APPDATA", config.clone()),
                ("LOCALAPPDATA", state.clone()),
            ],
        ),
        (
            RootPlatform::Unix,
            vec![
                ("HOME", home.clone()),
                ("XDG_CONFIG_HOME", config.clone()),
                ("XDG_STATE_HOME", state.clone()),
                ("XDG_CACHE_HOME", cache.clone()),
            ],
        ),
    ] {
        let env: HashMap<_, _> = entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        let roots = StorageRoots::resolve_for(platform, &env).unwrap();
        for (path_root, expected) in [
            (PathRoot::MachineConfig, &roots.config),
            (PathRoot::MachineState, &roots.state),
            (PathRoot::MachineCache, &roots.cache),
        ] {
            let resolved = path_root.resolve_for(platform, &env).unwrap();
            assert_eq!(resolved, *expected, "{platform:?} {path_root:?}");
            println!(
                "machine_roots_agree: {platform:?} {path_root:?} {resolved:?} == {expected:?}"
            );
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn validated_scoped_path_rejects_literal_backslash_and_case_sibling_links() {
    use crate::provider::{PathRoot, RelativePath, ScopedPath};
    let _guard = crate::test_support::env_guard();
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    let root = home.join(".agents").join("ccync");
    let backslash_sibling = home.join(".agents").join("ccync\\outside");
    let case_sibling = home.join(".agents").join("CCYNC");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backslash_sibling).unwrap();
    std::fs::create_dir_all(&case_sibling).unwrap();
    std::fs::write(backslash_sibling.join("marker"), b"backslash").unwrap();
    std::fs::write(case_sibling.join("marker"), b"case").unwrap();
    std::os::unix::fs::symlink(&backslash_sibling, root.join("backslash-link")).unwrap();
    std::os::unix::fs::symlink(&case_sibling, root.join("case-link")).unwrap();
    let prior = std::env::var_os("HOME");
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME");
    let xdg_state = std::env::var_os("XDG_STATE_HOME");
    let xdg_cache = std::env::var_os("XDG_CACHE_HOME");
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", temp.path().join("config"));
    std::env::set_var("XDG_STATE_HOME", temp.path().join("state"));
    std::env::set_var("XDG_CACHE_HOME", temp.path().join("cache"));
    let before = filesystem_snapshot(temp.path());
    for link in ["backslash-link", "case-link"] {
        let scoped = ScopedPath {
            root: PathRoot::Portable,
            path: RelativePath::new(format!("{link}/marker")).unwrap(),
        };
        assert!(scoped.resolve_validated().is_err(), "{link}");
    }
    assert_eq!(filesystem_snapshot(temp.path()), before);
    for (key, value) in [
        ("HOME", prior),
        ("XDG_CONFIG_HOME", xdg_config),
        ("XDG_STATE_HOME", xdg_state),
        ("XDG_CACHE_HOME", xdg_cache),
    ] {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

#[cfg(unix)]
#[test]
fn validated_scoped_path_rejects_symlink_escape_without_writes() {
    use crate::provider::{PathRoot, RelativePath, ScopedPath};
    let _guard = crate::test_support::env_guard();
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let link = home.join(".agents").join("ccync");
    std::fs::create_dir_all(&link).unwrap();
    let escaped_link = link.join("owned");
    std::os::unix::fs::symlink(&outside, &escaped_link).unwrap();
    let dangling_home = temp.path().join("dangling-home");
    std::fs::create_dir_all(&dangling_home).unwrap();
    std::fs::create_dir_all(dangling_home.join(".agents").join("ccync")).unwrap();
    std::os::unix::fs::symlink(
        temp.path().join("missing-target"),
        dangling_home.join(".agents").join("ccync").join("owned"),
    )
    .unwrap();
    let prior = std::env::var_os("HOME");
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME");
    let xdg_state = std::env::var_os("XDG_STATE_HOME");
    let xdg_cache = std::env::var_os("XDG_CACHE_HOME");
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", temp.path().join("config"));
    std::env::set_var("XDG_STATE_HOME", temp.path().join("state"));
    std::env::set_var("XDG_CACHE_HOME", temp.path().join("cache"));
    let before = filesystem_snapshot(temp.path());
    let scoped = ScopedPath {
        root: PathRoot::Portable,
        path: RelativePath::new("owned/file").unwrap(),
    };
    assert!(scoped.resolve_validated().is_err());
    std::env::set_var("HOME", &dangling_home);
    assert!(scoped.resolve_validated().is_err());
    assert_eq!(filesystem_snapshot(temp.path()), before);
    for (key, value) in [
        ("HOME", prior),
        ("XDG_CONFIG_HOME", xdg_config),
        ("XDG_STATE_HOME", xdg_state),
        ("XDG_CACHE_HOME", xdg_cache),
    ] {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

#[cfg(unix)]
#[test]
fn validated_scoped_path_uses_storage_roots_empty_xdg_fallback() {
    use crate::provider::{PathRoot, RelativePath, ScopedPath};
    let _guard = crate::test_support::env_guard();
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let prior = std::env::var_os("HOME");
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME");
    let xdg_state = std::env::var_os("XDG_STATE_HOME");
    let xdg_cache = std::env::var_os("XDG_CACHE_HOME");
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", "");
    std::env::set_var("XDG_STATE_HOME", "");
    std::env::set_var("XDG_CACHE_HOME", "");
    let before = filesystem_snapshot(temp.path());
    let scoped = ScopedPath {
        root: PathRoot::MachineConfig,
        path: RelativePath::new("settings.json").unwrap(),
    };
    let resolved = scoped.resolve_validated().unwrap().unwrap();
    assert!(resolved.is_absolute());
    assert_eq!(resolved, home.join(".config/ccync/settings.json"));
    assert_eq!(filesystem_snapshot(temp.path()), before);
    for (key, value) in [
        ("HOME", prior),
        ("XDG_CONFIG_HOME", xdg_config),
        ("XDG_STATE_HOME", xdg_state),
        ("XDG_CACHE_HOME", xdg_cache),
    ] {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

#[cfg(windows)]
#[test]
fn validated_scoped_path_rejects_junction_escape_without_writes() {
    use crate::provider::{PathRoot, RelativePath, ScopedPath};
    let _guard = crate::test_support::env_guard();
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path().join("home");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(home.join(".agents").join("ccync")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let link = home.join(".agents").join("ccync").join("owned");
    crate::platform::create_dir_link(&outside, &link).unwrap();
    let prior_home = std::env::var_os("USERPROFILE");
    let prior_appdata = std::env::var_os("APPDATA");
    let prior_local = std::env::var_os("LOCALAPPDATA");
    std::env::set_var("USERPROFILE", &home);
    std::env::set_var("APPDATA", temp.path().join("config"));
    std::env::set_var("LOCALAPPDATA", temp.path().join("local"));
    let before = filesystem_snapshot(temp.path());
    let scoped = ScopedPath {
        root: PathRoot::Portable,
        path: RelativePath::new("owned/file").unwrap(),
    };
    assert!(scoped.resolve_validated().is_err());
    assert_eq!(filesystem_snapshot(temp.path()), before);
    for (key, value) in [
        ("USERPROFILE", prior_home),
        ("APPDATA", prior_appdata),
        ("LOCALAPPDATA", prior_local),
    ] {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
