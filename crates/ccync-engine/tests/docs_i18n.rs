use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const ARCHITECTURE: &str = "docs/i18n/zh-Hant/architecture.zh-Hant.md";
const TOPICS: [&str; 11] = [
    "docs/i18n/zh-Hant/README.zh-Hant.md",
    "docs/i18n/zh-Hant/SECURITY.zh-Hant.md",
    "docs/i18n/zh-Hant/setup.zh-Hant.md",
    "docs/i18n/zh-Hant/plugins.zh-Hant.md",
    "docs/i18n/zh-Hant/commands.zh-Hant.md",
    "docs/i18n/zh-Hant/mcp.zh-Hant.md",
    "docs/i18n/zh-Hant/adoption.zh-Hant.md",
    "docs/i18n/zh-Hant/contributing.zh-Hant.md",
    "docs/i18n/zh-Hant/naming.zh-Hant.md",
    "docs/i18n/zh-Hant/deferred-work.zh-Hant.md",
    "docs/i18n/zh-Hant/i18n-guide.zh-Hant.md",
];
const ADRS: [&str; 7] = [
    "docs/adr/01-product-scope-and-projection-model.md",
    "docs/adr/02-source-resolution-and-plugin-materialization.md",
    "docs/adr/03-projection-ownership-and-atomic-publication.md",
    "docs/adr/04-mcp-trust-boundary-and-recovery.md",
    "docs/adr/05-machine-lifecycle-adoption-and-cli-contract.md",
    "docs/adr/06-backup-profiles-and-secret-handling.md",
    "docs/adr/07-release-and-distribution.md",
];
const METADATA_KEYS: [&str; 7] = [
    "description",
    "locale",
    "status",
    "tags",
    "title",
    "translation_key",
    "type",
];
const ARC42_SECTIONS: [&str; 12] = [
    "## 1. 簡介與目標",
    "## 2. 限制條件",
    "## 3. 脈絡與範圍",
    "## 4. 解法策略",
    "## 5. Building Block View",
    "## 6. Runtime View",
    "## 7. Deployment View",
    "## 8. Crosscutting Concepts",
    "## 9. 架構決策",
    "## 10. 品質需求",
    "## 11. 風險與技術債",
    "## 12. 詞彙表",
];
const ENGLISH_PAIRS: [(&str, &str); 12] = [
    ("README.md", "docs/i18n/zh-Hant/README.zh-Hant.md"),
    ("SECURITY.md", "docs/i18n/zh-Hant/SECURITY.zh-Hant.md"),
    ("docs/architecture.md", ARCHITECTURE),
    ("docs/setup.md", "docs/i18n/zh-Hant/setup.zh-Hant.md"),
    ("docs/plugins.md", "docs/i18n/zh-Hant/plugins.zh-Hant.md"),
    ("docs/commands.md", "docs/i18n/zh-Hant/commands.zh-Hant.md"),
    ("docs/mcp.md", "docs/i18n/zh-Hant/mcp.zh-Hant.md"),
    ("docs/adoption.md", "docs/i18n/zh-Hant/adoption.zh-Hant.md"),
    (
        "CONTRIBUTING.md",
        "docs/i18n/zh-Hant/contributing.zh-Hant.md",
    ),
    ("docs/naming.md", "docs/i18n/zh-Hant/naming.zh-Hant.md"),
    (
        "docs/deferred-work.md",
        "docs/i18n/zh-Hant/deferred-work.zh-Hant.md",
    ),
    (
        "docs/i18n/guide.md",
        "docs/i18n/zh-Hant/i18n-guide.zh-Hant.md",
    ),
];
const ENGLISH_ARC42_SECTIONS: [&str; 12] = [
    "## 1. Introduction and Goals",
    "## 2. Constraints",
    "## 3. Context and Scope",
    "## 4. Solution Strategy",
    "## 5. Building Block View",
    "## 6. Runtime View",
    "## 7. Deployment View",
    "## 8. Crosscutting Concepts",
    "## 9. Architecture Decisions",
    "## 10. Quality Requirements",
    "## 11. Risks and Technical Debt",
    "## 12. Glossary",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"))
        .replace("\r\n", "\n")
}

#[derive(Debug, PartialEq, Eq)]
enum RepositoryShape {
    Source,
    Public,
}

fn repository_shape(
    project_file: bool,
    dev_dir: bool,
    agents_file: bool,
) -> Option<RepositoryShape> {
    if project_file {
        Some(RepositoryShape::Source)
    } else if !dev_dir && !agents_file {
        Some(RepositoryShape::Public)
    } else {
        None
    }
}

fn current_repository_shape() -> RepositoryShape {
    let repository = root();
    repository_shape(
        repository.join(".dev/project.md").is_file(),
        repository.join(".dev").is_dir(),
        repository.join("AGENTS.md").is_file(),
    )
    .expect("repository must have source or public export shape")
}

#[test]
fn repository_shape_rejects_agents_without_dev_directory() {
    assert_eq!(repository_shape(false, false, true), None);
}

fn frontmatter_keys(body: &str) -> Vec<&str> {
    let mut lines = body.lines();
    assert_eq!(lines.next(), Some("---"), "missing YAML frontmatter");
    let mut keys = Vec::new();
    for line in lines {
        if line == "---" {
            break;
        }
        if !line.starts_with(' ') && !line.starts_with('-') {
            keys.push(line.split_once(':').expect("invalid metadata row").0);
        }
    }
    keys.sort_unstable();
    keys
}

fn frontmatter_value<'a>(body: &'a str, key: &str) -> &'a str {
    body.lines()
        .skip(1)
        .take_while(|line| *line != "---")
        .find_map(|line| line.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("missing metadata value: {key}"))
}

fn markdown_links(body: &str) -> Vec<&str> {
    body.split("](")
        .skip(1)
        .filter_map(|tail| tail.split_once(')').map(|(target, _)| target))
        .collect()
}

fn heading_shape(body: &str) -> Vec<usize> {
    body.lines()
        .filter_map(|line| {
            if line.starts_with("## ") {
                Some(2)
            } else if line.starts_with("### ") {
                Some(3)
            } else {
                None
            }
        })
        .collect()
}

fn assert_local_links_resolve(path: &str, body: &str) {
    let parent = root()
        .join(path)
        .parent()
        .expect("document parent")
        .to_owned();
    for link in markdown_links(body) {
        if link.starts_with("http://")
            || link.starts_with("https://")
            || link.starts_with('#')
            || link.starts_with("mailto:")
        {
            continue;
        }
        let target = link.split('#').next().expect("link target");
        if !target.is_empty() {
            assert!(
                parent.join(target).exists(),
                "broken link: {path} -> {target}"
            );
        }
    }
}

#[test]
fn stage_1_manifest_is_exact_and_metadata_is_valid() {
    let adr_dir = root().join("docs/i18n/zh-Hant/adr");
    assert!(!adr_dir.exists(), "retired zh-Hant ADR directory remains");

    for path in [ARCHITECTURE] {
        let body = read(path);
        assert_eq!(frontmatter_keys(&body), METADATA_KEYS, "metadata: {path}");
        assert!(body.contains("locale: zh-Hant"), "locale: {path}");
        assert!(body.contains("status: stable"), "status: {path}");
        assert!(body.ends_with('\n'), "missing final newline: {path}");
        // Git may check out this fixture with CRLF on Windows. Remove valid CRLF pairs, then reject any remaining CR.
        assert!(
            !body.replace("\r\n", "").contains('\r'),
            "invalid line ending: {path}"
        );
        assert!(body.chars().count() > 800, "document is vacuous: {path}");
        for prohibited in [
            "软件",
            "用户",
            "配置文件",
            "数据",
            "目录",
            "文件夹",
            "通过",
            "仅",
        ] {
            assert!(
                !body.contains(prohibited),
                "prohibited term {prohibited}: {path}"
            );
        }
        assert!(
            !body.contains("<a id=\""),
            "explicit HTML anchors are forbidden: {path}"
        );
    }
}

#[test]
fn stage_2_manifest_is_exact_and_topics_are_valid() {
    let root_dir = root().join("docs/i18n/zh-Hant");
    let actual: BTreeSet<_> = fs::read_dir(&root_dir)
        .expect("cannot read zh-Hant root")
        .filter_map(|entry| {
            let path = entry.expect("invalid zh-Hant entry").path();
            path.is_file().then(|| {
                path.strip_prefix(root())
                    .expect("document outside repository")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
        })
        .collect();
    let expected: BTreeSet<_> = std::iter::once(ARCHITECTURE.to_owned())
        .chain(TOPICS.map(str::to_owned))
        .collect();
    assert_eq!(actual, expected);
    assert!(!root().join("docs/i18n/zh-Hant/manual.zh-Hant.md").exists());

    let mut translation_keys = BTreeSet::new();
    for path in TOPICS {
        let body = read(path);
        if path == TOPICS[0] {
            assert_eq!(body.lines().next(), Some("# ccync"), "README H1: {path}");
            assert!(!body.starts_with("---"), "README metadata: {path}");
        } else {
            assert_eq!(frontmatter_keys(&body), METADATA_KEYS, "metadata: {path}");
            assert!(body.contains("locale: zh-Hant"), "locale: {path}");
            assert!(body.contains("status: stable"), "status: {path}");
            assert!(
                translation_keys.insert(frontmatter_value(&body, "translation_key").to_owned()),
                "duplicate translation_key: {path}"
            );
        }
        assert!(
            body.ends_with('\n') && !body.replace("\r\n", "").contains('\r'),
            "invalid line ending: {path}"
        );
        assert!(body.chars().count() > 450, "document is vacuous: {path}");
        assert!(!body.contains("<a id=\""), "explicit HTML anchor: {path}");
        assert_eq!(
            body.lines().filter(|line| line.starts_with("# ")).count(),
            1,
            "H1: {path}"
        );
        for prohibited in [
            "软件",
            "用户",
            "配置文件",
            "数据",
            "目录",
            "文件夹",
            "通过",
            "仅",
        ] {
            assert!(
                !body.contains(prohibited),
                "prohibited term {prohibited}: {path}"
            );
        }
    }
    assert_eq!(translation_keys.len(), TOPICS.len() - 1);

    let readme = read(TOPICS[0]);
    let map = readme
        .split("## 文件地圖")
        .nth(1)
        .expect("missing document map");
    let mapped: BTreeSet<_> = map
        .lines()
        .filter(|line| line.starts_with("| ["))
        .map(|line| {
            line.split_once("](")
                .and_then(|(_, tail)| tail.split_once(')'))
                .map(|(target, _)| target.to_owned())
                .expect("invalid document-map link")
        })
        .collect();
    let expected_map: BTreeSet<_> = std::iter::once(ARCHITECTURE)
        .chain(TOPICS.into_iter().skip(1))
        .map(|path| {
            path.rsplit('/')
                .next()
                .expect("document basename")
                .to_owned()
        })
        .collect();
    assert_eq!(mapped, expected_map, "README document-map membership");
    assert_eq!(
        map.lines().filter(|line| line.starts_with("| [")).count(),
        11
    );

    let commands = read("docs/i18n/zh-Hant/commands.zh-Hant.md");
    let listed: BTreeSet<_> = commands
        .split("## Binary 自我更新")
        .next()
        .expect("public command section")
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|line| line.split_once('`').map(|(name, _)| name.to_owned()))
        .collect();
    let expected_commands: BTreeSet<_> = [
        "init",
        "sync",
        "add",
        "search",
        "repair",
        "remove",
        "list",
        "show",
        "doctor",
        "backup",
        "restore",
        "uninstall",
        "update",
        "upgrade",
        "cleanup",
        "pin",
        "roster",
        "tool",
        "outdated",
    ]
    .map(str::to_owned)
    .into_iter()
    .collect();
    assert_eq!(listed, expected_commands, "public commands reference");
}

#[test]
fn architecture_has_twelve_sections_and_seven_entry_adr_index() {
    let body = read(ARCHITECTURE);
    for section in ARC42_SECTIONS {
        assert!(
            body.lines().any(|line| line == section),
            "missing arc42 section: {section}"
        );
    }
    let index = body
        .split("## 9. 架構決策")
        .nth(1)
        .expect("missing architecture decisions section")
        .split("## 10. 品質需求")
        .next()
        .expect("missing quality requirements boundary");
    assert_eq!(
        index.matches("adr/").count(),
        7,
        "ADR index must have seven links"
    );
}

#[test]
fn adrs_have_locked_sections_and_admission_evidence() {
    for path in ADRS {
        let body = read(path);
        for section in [
            "## Background",
            "## Decision",
            "## Rejected alternatives",
            "## Consequences",
        ] {
            assert!(
                body.lines().any(|line| line == section),
                "missing {section}: {path}"
            );
        }
        for label in [
            "- Impact scope:",
            "- Real alternatives:",
            "- Verifiable evidence:",
        ] {
            assert!(
                body.contains(label),
                "missing admission evidence {label}: {path}"
            );
        }
    }
}

#[test]
fn architecture_and_adr_links_resolve_and_are_doc_synced() {
    for path in std::iter::once(ARCHITECTURE).chain(ADRS) {
        assert_local_links_resolve(path, &read(path));
    }

    let structure_map = read("docs/structure/structure-map.ndjson");
    for path in std::iter::once(ARCHITECTURE).chain(ADRS) {
        assert!(
            structure_map.contains(&format!("\"id\":\"{path}\""))
                && structure_map.contains(&format!("\"path\":\"{path}\",\"syncStatus\":\"ok\"")),
            "doc-sync row is missing or not current: {path}"
        );
    }
}

#[test]
fn topic_links_resolve_and_structure_map_covers_topics() {
    for path in std::iter::once(ARCHITECTURE).chain(TOPICS).chain(ADRS) {
        assert_local_links_resolve(path, &read(path));
    }

    let structure_map = read("docs/structure/structure-map.ndjson");
    for path in TOPICS {
        assert!(
            structure_map.contains(&format!("\"id\":\"{path}\"")),
            "doc-sync: {path}"
        );
    }
}

#[test]
fn stage_3_english_manifest_and_bilingual_structure_are_exact() {
    let repository = root();
    let shape = current_repository_shape();
    let expected_docs: BTreeSet<_> = ENGLISH_PAIRS
        .iter()
        .map(|(path, _)| *path)
        .filter(|path| path.starts_with("docs/") && !path.starts_with("docs/i18n/"))
        .map(str::to_owned)
        .collect();
    let legacy = ["docs/manual.md", "docs/devguide.md"];
    let legacy_count = legacy
        .iter()
        .filter(|path| repository.join(path).exists())
        .count();
    assert!(
        legacy_count == 0 || legacy_count == 2,
        "partial legacy cutover"
    );
    let expected: BTreeSet<_> = expected_docs
        .into_iter()
        .chain(legacy.into_iter().take(legacy_count).map(str::to_owned))
        .chain((legacy_count == 2).then_some("docs/graphify-operations.md".to_owned()))
        .collect();
    match shape {
        RepositoryShape::Source => assert!(repository.join(".dev/project.md").is_file()),
        RepositoryShape::Public => {
            assert!(!repository.join(".dev").exists());
            assert!(!repository.join("AGENTS.md").exists());
        }
    }
    let actual: BTreeSet<_> = fs::read_dir(root().join("docs"))
        .expect("cannot read docs root")
        .filter_map(|entry| {
            let path = entry.expect("invalid docs entry").path();
            (path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
                .then(|| format!("docs/{}", path.file_name().expect("name").to_string_lossy()))
        })
        .collect();
    assert_eq!(actual, expected, "English docs root membership");

    let adr_root = root().join("docs/adr");
    let actual_adrs: BTreeSet<_> = fs::read_dir(&adr_root)
        .expect("cannot read English ADR directory")
        .map(|entry| {
            entry
                .expect("invalid English ADR entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let expected_adrs: BTreeSet<_> = ADRS
        .map(|path| path.rsplit('/').next().expect("ADR basename").to_owned())
        .into_iter()
        .collect();
    assert_eq!(actual_adrs, expected_adrs, "English ADR membership");

    for (english, chinese) in ENGLISH_PAIRS {
        let en = read(english);
        let zh = read(chinese);
        if english == "README.md" {
            assert_eq!(en.lines().next(), Some("# ccync"), "README H1: {english}");
            assert_eq!(zh.lines().next(), Some("# ccync"), "README H1: {chinese}");
        } else if english == "SECURITY.md" {
            let key = frontmatter_value(&zh, "translation_key");
            assert!(
                en.starts_with(&format!("<!-- translation_key: {key} -->\n")),
                "root translation-key comment: {english}"
            );
        } else {
            assert_eq!(frontmatter_keys(&en), METADATA_KEYS, "metadata: {english}");
            assert_eq!(frontmatter_value(&en, "locale"), "en", "locale: {english}");
            assert_eq!(
                frontmatter_value(&en, "status"),
                "stable",
                "status: {english}"
            );
            assert_eq!(
                frontmatter_value(&en, "translation_key"),
                frontmatter_value(&zh, "translation_key"),
                "translation_key: {english}"
            );
            assert_eq!(
                frontmatter_value(&en, "type"),
                frontmatter_value(&zh, "type"),
                "type: {english}"
            );
        }
        assert_eq!(
            heading_shape(&en),
            heading_shape(&zh),
            "H2/H3 parity: {english}"
        );
        assert_eq!(
            en.lines().filter(|line| line.starts_with("# ")).count(),
            1,
            "H1: {english}"
        );
        assert!(
            en.ends_with('\n') && !en.replace("\r\n", "").contains('\r'),
            "invalid line ending: {english}"
        );
        assert!(en.chars().count() > 450, "document is vacuous: {english}");
        assert!(!en.contains("<a id=\""), "explicit HTML anchor: {english}");
        assert_local_links_resolve(english, &en);
    }

    for english in ADRS {
        let en = read(english);
        assert_eq!(frontmatter_keys(&en), METADATA_KEYS, "metadata: {english}");
        assert_eq!(frontmatter_value(&en, "locale"), "en", "locale: {english}");
        assert_eq!(
            frontmatter_value(&en, "status"),
            "stable",
            "status: {english}"
        );
        assert!(!frontmatter_value(&en, "translation_key").is_empty());
        assert_eq!(frontmatter_value(&en, "type"), "adr");
        for heading in [
            "## Background",
            "## Decision",
            "## Rejected alternatives",
            "## Consequences",
        ] {
            assert!(
                en.lines().any(|line| line == heading),
                "missing {heading}: {english}"
            );
        }
        for label in [
            "- Impact scope:",
            "- Real alternatives:",
            "- Verifiable evidence:",
        ] {
            assert!(
                en.contains(label),
                "missing admission evidence {label}: {english}"
            );
        }
        assert!(en.chars().count() > 800, "ADR is vacuous: {english}");
        assert!(en.ends_with('\n') && !en.contains("\r\n"), "LF: {english}");
        assert_local_links_resolve(english, &en);
    }
}

#[test]
fn stage_3_readme_map_and_architecture_adr_index_are_exact() {
    let readme = read("README.md");
    let map = readme
        .split("## Documentation map")
        .nth(1)
        .expect("missing English documentation map")
        .split("\n## ")
        .next()
        .expect("missing map end");
    let rows: Vec<_> = map.lines().filter(|line| line.starts_with("| [")).collect();
    assert_eq!(rows.len(), 11, "README map row count");
    let expected: BTreeSet<_> = ENGLISH_PAIRS
        .iter()
        .map(|(path, _)| *path)
        .filter(|path| *path != "README.md")
        .collect();
    let actual: BTreeSet<_> = rows
        .iter()
        .map(|row| {
            let target = row
                .split_once("](")
                .expect("map link")
                .1
                .split_once(')')
                .expect("map target")
                .0;
            let scope = row.split(')').nth(1).expect("map scope").trim();
            assert!(
                scope.matches(char::is_alphabetic).count() >= 12,
                "vacuous README map scope: {row}"
            );
            assert!(
                root().join(target).exists(),
                "missing README map target: {target}"
            );
            if target != "SECURITY.md" {
                let body = read(target);
                assert_eq!(
                    scope.trim_matches('|').trim(),
                    frontmatter_value(&body, "description"),
                    "README map scope differs from document description: {target}"
                );
            }
            target
        })
        .collect();
    assert_eq!(actual, expected, "README map destinations");

    let architecture = read("docs/architecture.md");
    for heading in ENGLISH_ARC42_SECTIONS {
        assert!(
            architecture.lines().any(|line| line == heading),
            "missing arc42 heading: {heading}"
        );
    }
    let actual_h2: Vec<_> = architecture
        .lines()
        .filter(|line| line.starts_with("## "))
        .collect();
    assert_eq!(
        actual_h2, ENGLISH_ARC42_SECTIONS,
        "arc42 H2 order and membership"
    );
    let index = architecture
        .split("## 9. Architecture Decisions")
        .nth(1)
        .expect("missing ADR index")
        .split("## 10. Quality Requirements")
        .next()
        .expect("missing ADR index end");
    let links: Vec<_> = markdown_links(index)
        .into_iter()
        .filter(|link| link.starts_with("adr/"))
        .collect();
    assert_eq!(links.len(), 7, "ADR index link count");
    let expected_adrs: BTreeSet<_> = ADRS
        .map(|path| format!("adr/{}", path.rsplit('/').next().expect("ADR basename")))
        .into_iter()
        .collect();
    assert_eq!(
        links
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>(),
        expected_adrs,
        "ADR index identities"
    );
}

#[test]
fn deferred_work_inventory_keeps_every_legacy_backlog_item() {
    for path in [
        "docs/deferred-work.md",
        "docs/i18n/zh-Hant/deferred-work.zh-Hant.md",
    ] {
        let inventory = read(path);
        for id in 1..=44 {
            if id == 21 {
                for suffix in ["21-release", "21-cache"] {
                    assert!(
                        inventory.contains(&format!("| {suffix} |")),
                        "missing legacy backlog item {suffix}: {path}"
                    );
                }
            } else {
                assert!(
                    inventory.contains(&format!("| {id} |")),
                    "missing legacy backlog item {id}: {path}"
                );
            }
        }
    }
}

#[test]
fn english_links_resolve() {
    for (english, _) in ENGLISH_PAIRS {
        assert_local_links_resolve(english, &read(english));
    }
    for english in ADRS {
        assert_local_links_resolve(english, &read(english));
    }
}

#[test]
fn stage_3_post_cutover_pointers_and_structure_map_are_current() {
    let shape = current_repository_shape();
    for legacy in [
        "docs/manual.md",
        "docs/devguide.md",
        "docs/graphify-operations.md",
    ] {
        assert!(
            !root().join(legacy).exists(),
            "legacy document remains: {legacy}"
        );
    }

    let map = read("docs/structure/structure-map.ndjson");
    for legacy in [
        "docs/manual.md",
        "docs/devguide.md",
        "docs/graphify-operations.md",
    ] {
        assert!(!map.contains(legacy), "stale structure-map path: {legacy}");
    }
    for (english, _) in ENGLISH_PAIRS {
        assert!(
            map.contains(&format!("\"id\":\"{english}\"")),
            "missing English doc-axis row: {english}"
        );
    }
    for english in ADRS {
        assert!(
            map.contains(&format!("\"id\":\"{english}\"")),
            "missing English ADR doc-axis row: {english}"
        );
    }

    match shape {
        RepositoryShape::Source => {
            for path in ["AGENTS.md", ".dev/project.md"] {
                let body = read(path);
                assert_local_links_resolve(path, &body);
                for legacy in [
                    "docs/manual.md",
                    "docs/devguide.md",
                    "docs/graphify-operations.md",
                ] {
                    assert!(!body.contains(legacy), "stale pointer in {path}: {legacy}");
                }
            }
        }
        RepositoryShape::Public => {
            assert!(!root().join(".dev").exists());
            assert!(!root().join("AGENTS.md").exists());
        }
    }
}

#[test]
fn lifecycle_docs_cover_the_locked_contract_in_both_languages() {
    let english_commands = read("docs/commands.md");
    let chinese_commands = read("docs/i18n/zh-Hant/commands.zh-Hant.md");
    let english_architecture = read("docs/architecture.md");
    let chinese_architecture = read("docs/i18n/zh-Hant/architecture.zh-Hant.md");

    for required in [
        "exact executable",
        "stale",
        "CheckFailed",
        "stderr",
        "exit code",
        "JSON",
        "reload",
        "UpdateAvailable",
        "journal",
        "Gated",
        "fail closed",
        "authorization",
        "schemaVersion",
        "selectionState",
        "updateState",
        "updateFreshness",
        "nextAction",
        "affectedAgents",
        "projectionChanged",
        "reloadAdvice",
        "PATHEXT",
        "--retry-manager-step",
        "SemVer",
        "directory inventory",
    ] {
        assert!(
            english_commands.contains(required) || english_architecture.contains(required),
            "missing English lifecycle contract: {required}"
        );
    }
    for required in [
        "完全相符",
        "過時",
        "CheckFailed",
        "stderr",
        "結束代碼",
        "JSON",
        "重新載入",
        "UpdateAvailable",
        "生命週期日誌",
        "Gated",
        "fail-closed",
        "授權",
        "schemaVersion",
        "selectionState",
        "updateState",
        "updateFreshness",
        "nextAction",
        "affectedAgents",
        "projectionChanged",
        "reloadAdvice",
        "PATHEXT",
        "--retry-manager-step",
        "SemVer",
        "目錄清單",
    ] {
        assert!(
            chinese_commands.contains(required) || chinese_architecture.contains(required),
            "missing zh-Hant lifecycle contract: {required}"
        );
    }
}

#[test]
fn lifecycle_structure_projection_is_linewise_valid_unique_and_stable() {
    let map = read("docs/structure/structure-map.ndjson");
    let expected_keys = [
        "axis",
        "docs",
        "id",
        "kind",
        "lastSynced",
        "lastSyncedRef",
        "layer",
        "path",
        "syncStatus",
    ];
    let mut keys = BTreeSet::new();
    let mut previous: Option<(String, String)> = None;
    let repository = root();

    for (index, line) in map.lines().enumerate() {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("invalid NDJSON line {}: {error}", index + 1));
        let object = value.as_object().expect("structure row must be an object");
        let axis = object["axis"].as_str().expect("axis string");
        let id = object["id"].as_str().expect("id string");
        assert!(
            matches!(object["kind"].as_str(), Some("dir" | "module" | "file")),
            "invalid kind: {id}"
        );
        assert!(object["docs"].is_array(), "docs must be an array: {id}");
        assert!(
            object["docs"]
                .as_array()
                .unwrap()
                .iter()
                .all(serde_json::Value::is_string),
            "docs entries must be strings: {id}"
        );
        assert!(
            object["layer"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "layer must be a non-empty string: {id}"
        );
        assert!(
            object["lastSynced"]
                .as_str()
                .is_some_and(|value| value.len() == 10
                    && value.as_bytes()[4] == b'-'
                    && value.as_bytes()[7] == b'-'),
            "invalid lastSynced date: {id}"
        );
        assert!(
            object["lastSyncedRef"].is_null()
                || object["lastSyncedRef"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
            "invalid lastSyncedRef: {id}"
        );
        let key = (axis.to_owned(), id.to_owned());
        assert!(keys.insert(key.clone()), "duplicate structure key: {key:?}");
        assert!(
            previous.as_ref().is_none_or(|old| old <= &key),
            "unstable row order at {key:?}"
        );
        previous = Some(key);

        let positions: Vec<_> = expected_keys
            .iter()
            .map(|key| {
                assert_eq!(
                    line.matches(&format!("\"{key}\":")).count(),
                    1,
                    "key count for {key} on line {}",
                    index + 1
                );
                line.find(&format!("\"{key}\":")).expect("key position")
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "schema key order on line {}",
            index + 1
        );
        assert_eq!(
            object.len(),
            expected_keys.len(),
            "schema key count on line {}",
            index + 1
        );
        assert_eq!(object["path"].as_str(), Some(id), "path/id mismatch: {id}");
        assert!(matches!(axis, "code" | "doc"), "invalid axis: {axis}");
        assert!(
            repository.join(id).exists(),
            "unresolvable structure path: {id}"
        );
        for doc in object["docs"].as_array().unwrap() {
            let doc = doc.as_str().unwrap();
            assert!(
                repository.join(doc).is_file(),
                "unresolvable doc coverage {doc}: {id}"
            );
        }
        assert!(matches!(
            object["syncStatus"].as_str(),
            Some("ok" | "drift" | "missing")
        ));
        let synced = object["lastSynced"].as_str().unwrap();
        assert!(
            chrono::NaiveDate::parse_from_str(synced, "%Y-%m-%d").is_ok(),
            "invalid calendar date: {id}"
        );
        assert!(
            synced[..4].parse::<u16>().is_ok()
                && synced[5..7]
                    .parse::<u8>()
                    .is_ok_and(|month| (1..=12).contains(&month))
                && synced[8..10]
                    .parse::<u8>()
                    .is_ok_and(|day| (1..=31).contains(&day)),
            "invalid lastSynced date value: {id}"
        );
        if let Some(reference) = object["lastSyncedRef"].as_str() {
            assert!(
                (7..=40).contains(&reference.len())
                    && reference
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "invalid lastSyncedRef: {id}"
            );
        }
        if axis == "doc" {
            assert!(
                matches!(object["kind"].as_str(), Some("file")),
                "doc row kind: {id}"
            );
            if id.starts_with("docs/commands") || id.starts_with("docs/architecture") {
                assert_eq!(
                    object["kind"].as_str(),
                    Some("file"),
                    "reviewed doc row kind: {id}"
                );
            }
            assert!(
                object["docs"].as_array().unwrap().is_empty(),
                "doc rows cannot project to other docs: {id}"
            );
        } else {
            assert!(matches!(
                object["kind"].as_str(),
                Some("dir" | "module" | "file")
            ));
        }
    }

    for path in [
        "crates/ccync-cli/src/commands/outdated.rs",
        "crates/ccync-cli/src/commands/tool.rs",
        "crates/ccync-cli/src/commands/tool_output.rs",
        "crates/ccync-cli/src/commands/tool_refresh.rs",
        "crates/ccync-cli/src/commands/tool_track.rs",
        "crates/ccync-cli/src/commands/upgrade.rs",
        "crates/ccync-engine/src/tool_manager/adapters/cargo.rs",
        "crates/ccync-engine/src/tool_manager/adapters/gates.rs",
        "crates/ccync-engine/src/tool_manager/adapters/mod.rs",
        "crates/ccync-engine/src/tool_manager/adapters/pip.rs",
        "crates/ccync-engine/src/tool_manager/discovery.rs",
        "crates/ccync-engine/src/tool_manager/journal.rs",
        "crates/ccync-engine/src/tool_manager/recovery.rs",
        "crates/ccync-engine/src/tool_manager/store.rs",
        "crates/ccync-engine/src/tool_manager/refresh.rs",
        "crates/ccync-engine/src/tool_manager/evidence.rs",
        "crates/ccync-engine/src/tool_manager/status.rs",
        "crates/ccync-engine/src/tool_manager/track.rs",
        "crates/ccync-engine/src/tool_manager/untrack.rs",
        "crates/ccync-engine/src/tool_manager/upgrade.rs",
    ] {
        let row = map.lines().find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value["id"].as_str() == Some(path)).then_some(value)
        });
        let row = row.unwrap_or_else(|| panic!("missing lifecycle code projection: {path}"));
        assert_eq!(row["axis"].as_str(), Some("code"), "wrong axis: {path}");
        assert_eq!(
            row["syncStatus"].as_str(),
            Some("ok"),
            "stale projection: {path}"
        );
        let expected_docs: &[&str] = if path.starts_with("crates/ccync-engine/") {
            &["docs/commands.md", "docs/architecture.md"]
        } else {
            &["docs/commands.md"]
        };
        for doc in expected_docs {
            assert!(
                row["docs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == doc),
                "missing {doc} projection: {path}"
            );
        }
    }
}
