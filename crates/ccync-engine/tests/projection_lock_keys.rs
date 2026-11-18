//! Ownership rows in `build/lock.json`, queried through the public projection API.
//! Older non-Windows releases wrote each row with `\` separators. Those rows must
//! answer every ownership query the way a current row does. A row that does not
//! decode to a safe absolute path must stay in the lock verbatim without
//! authorizing any write, overwrite, or removal.

use projection::{
    direct_records, forget_direct_record, normalize_managed_artifact_path, run_update_skills,
    standalone_publication_records, DirectArtifactKind, DirectProjectionRecord,
    ManagedDeprojection, ProjectionSource, SkillUpdateOptions, StandaloneProjectionRecord,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const SOURCE_ID: &str = "fixture";
const DIRECT_SKILLS: &str = "directSkillProjectionPaths";
const SKILLS: &str = "skillProjectionPaths";
const ATTRIBUTION: &str = "sourceAttribution";
const STANDALONE: &str = "standalonePublications";

struct Fixture {
    _temp: TempDir,
    home: PathBuf,
    repo_root: PathBuf,
    source_root: PathBuf,
    lock: PathBuf,
    skill: PathBuf,
    /// A user directory that the retired-surface cleanup removes only when the
    /// registry authorizes the mutation.
    retired_skills: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        let repo_root = root.join("repo");
        let source_root = repo_root.join("plugins/ccync-core");
        let skill_source = source_root.join("skills/sample-skill");
        fs::create_dir_all(&skill_source).unwrap();
        fs::write(
            skill_source.join("SKILL.md"),
            "---\nname: sample-skill\ndescription: Fixture skill.\n---\nBody\n",
        )
        .unwrap();
        let lock = home.join(".ccync/build/lock.json");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        let retired_skills = home.join(".gemini/skills");
        fs::create_dir_all(&retired_skills).unwrap();
        fs::write(retired_skills.join("user-note.md"), "user bytes\n").unwrap();
        Self {
            skill: home.join(".agents/skills/direct-skill"),
            _temp: temp,
            home,
            repo_root,
            source_root,
            lock,
            retired_skills,
        }
    }

    fn standalone_record(&self) -> StandaloneProjectionRecord {
        StandaloneProjectionRecord {
            path: self.skill.clone(),
            source_id: SOURCE_ID.into(),
            source_revision: "revision".into(),
            source_fingerprint: "source-fingerprint".into(),
            tree_fingerprint: "tree-fingerprint".into(),
            targets: vec!["codex".into()],
        }
    }

    fn write_lock(&self, meta: Value) {
        let root = json!({ "_ccyncProjection": meta });
        fs::write(&self.lock, serde_json::to_vec_pretty(&root).unwrap()).unwrap();
    }

    fn lock_meta(&self) -> Map<String, Value> {
        let root: Value = serde_json::from_slice(&fs::read(&self.lock).unwrap()).unwrap();
        root["_ccyncProjection"].as_object().unwrap().clone()
    }

    /// Run the skills projection, which persists the registry and runs the
    /// retired-surface cleanup guarded by `can_mutate`.
    fn update_skills(&self) {
        let options = SkillUpdateOptions {
            repo_root: self.repo_root.clone(),
            state_root: self.lock.parent().unwrap().to_path_buf(),
            sources: vec![ProjectionSource {
                root: self.source_root.clone(),
                id: SOURCE_ID.into(),
                persistent: true,
            }],
            user_home: self.home.clone(),
            selected_projection_targets: vec!["opencode".into()],
            dry_run: false,
            replace: true,
        };
        run_update_skills(&options).unwrap();
    }
}

fn current_key(path: &Path) -> String {
    normalize_managed_artifact_path(path)
}

/// The row text that older non-Windows releases wrote for `path`.
#[cfg(not(windows))]
fn legacy_key(path: &Path) -> String {
    normalize_managed_artifact_path(path).replace('/', "\\")
}

fn array_contains(meta: &Map<String, Value>, section: &str, key: &str) -> bool {
    meta.get(section)
        .and_then(Value::as_array)
        .is_some_and(|rows| rows.iter().any(|row| row.as_str() == Some(key)))
}

fn object_value<'a>(meta: &'a Map<String, Value>, section: &str, key: &str) -> Option<&'a Value> {
    meta.get(section)
        .and_then(Value::as_object)
        .and_then(|rows| rows.get(key))
}

#[derive(Debug, PartialEq)]
struct OwnershipAnswers {
    records: Vec<DirectProjectionRecord>,
    standalone: Vec<StandaloneProjectionRecord>,
    removal_authorized: bool,
    registry_empty: bool,
    retired_skills_removed: bool,
    rows_stored_in_current_format: bool,
    rows_left_after_forget: usize,
}

fn expected_answers(fixture: &Fixture) -> OwnershipAnswers {
    OwnershipAnswers {
        records: vec![DirectProjectionRecord {
            kind: DirectArtifactKind::Skill,
            path: PathBuf::from(current_key(&fixture.skill)),
            source_id: Some(SOURCE_ID.into()),
        }],
        standalone: vec![fixture.standalone_record()],
        removal_authorized: true,
        registry_empty: false,
        retired_skills_removed: true,
        rows_stored_in_current_format: true,
        rows_left_after_forget: 0,
    }
}

fn ownership_answers(fixture: &Fixture, encode: fn(&Path) -> String) -> OwnershipAnswers {
    let skill_key = encode(&fixture.skill);
    let proof = serde_json::to_value(fixture.standalone_record()).unwrap();
    fixture.write_lock(json!({
        DIRECT_SKILLS: [skill_key.clone()],
        SKILLS: [encode(&fixture.retired_skills)],
        ATTRIBUTION: { skill_key.clone(): SOURCE_ID },
        STANDALONE: { skill_key.clone(): proof },
    }));

    let records = direct_records(&fixture.lock);
    let standalone = standalone_publication_records(&fixture.lock).unwrap();
    let deprojection = ManagedDeprojection::from_lockfile(&fixture.lock);
    let removal_authorized = deprojection.is_authorized_for_removal(&fixture.skill);
    let registry_empty = deprojection.is_empty();

    fixture.update_skills();
    let retired_skills_removed = !fixture.retired_skills.exists();
    let meta = fixture.lock_meta();
    let stored_key = current_key(&fixture.skill);
    let encoded_row_gone = skill_key == stored_key
        || (!array_contains(&meta, DIRECT_SKILLS, &skill_key)
            && object_value(&meta, ATTRIBUTION, &skill_key).is_none()
            && object_value(&meta, STANDALONE, &skill_key).is_none());
    let rows_stored_in_current_format = encoded_row_gone
        && array_contains(&meta, DIRECT_SKILLS, &stored_key)
        && object_value(&meta, ATTRIBUTION, &stored_key) == Some(&json!(SOURCE_ID))
        && object_value(&meta, STANDALONE, &stored_key).is_some();

    forget_direct_record(
        &fixture.lock,
        DirectArtifactKind::Skill,
        &fixture.skill,
        false,
    )
    .unwrap();
    let meta = fixture.lock_meta();
    let rows_left_after_forget = [&skill_key, &stored_key]
        .iter()
        .map(|key| {
            usize::from(array_contains(&meta, DIRECT_SKILLS, key))
                + usize::from(object_value(&meta, ATTRIBUTION, key).is_some())
                + usize::from(object_value(&meta, STANDALONE, key).is_some())
        })
        .sum::<usize>()
        + direct_records(&fixture.lock).len()
        + standalone_publication_records(&fixture.lock).unwrap().len();

    OwnershipAnswers {
        records,
        standalone,
        removal_authorized,
        registry_empty,
        retired_skills_removed,
        rows_stored_in_current_format,
        rows_left_after_forget,
    }
}

#[test]
fn current_rows_answer_every_ownership_query() {
    let fixture = Fixture::new();
    assert_eq!(
        ownership_answers(&fixture, current_key),
        expected_answers(&fixture)
    );
}

#[cfg(not(windows))]
#[test]
fn legacy_rows_answer_every_ownership_query_like_current_rows() {
    let fixture = Fixture::new();
    assert_ne!(legacy_key(&fixture.skill), current_key(&fixture.skill));
    assert_eq!(
        ownership_answers(&fixture, legacy_key),
        expected_answers(&fixture)
    );
}

/// Rows that must never decode: a `..` component, a relative path, and a `/`
/// inside a `\`-prefixed row. Each one names a path that holds user bytes.
#[cfg(not(windows))]
struct InvalidRows {
    keys: Vec<String>,
    user_paths: Vec<PathBuf>,
}

#[cfg(not(windows))]
impl InvalidRows {
    fn new(fixture: &Fixture) -> Self {
        let escaped = fixture.home.join(".agents/escaped");
        let mixed = fixture.home.join("mixed");
        let relative = fixture.home.join("relative");
        let keys = vec![
            legacy_key(&fixture.home.join(".agents/skills/../escaped")),
            "relative\\direct-skill".to_string(),
            format!("{}/mixed", legacy_key(&fixture.home)),
            format!("{}/skills", legacy_key(&fixture.home.join(".gemini"))),
        ];
        let user_paths = vec![escaped, mixed, relative.join("direct-skill")];
        for path in &user_paths {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("user-note.md"), "user bytes\n").unwrap();
        }
        Self { keys, user_paths }
    }

    fn proof(key: &str) -> Value {
        json!({ "path": key, "sourceId": SOURCE_ID })
    }

    fn assert_preserved(&self, fixture: &Fixture) {
        let meta = fixture.lock_meta();
        for key in &self.keys {
            assert!(array_contains(&meta, DIRECT_SKILLS, key), "{key}");
            assert!(array_contains(&meta, SKILLS, key), "{key}");
            assert_eq!(
                object_value(&meta, ATTRIBUTION, key),
                Some(&json!(SOURCE_ID)),
                "{key}"
            );
            assert_eq!(
                object_value(&meta, STANDALONE, key),
                Some(&Self::proof(key)),
                "{key}"
            );
        }
        for path in self
            .user_paths
            .iter()
            .chain(std::iter::once(&fixture.retired_skills))
        {
            assert_eq!(
                fs::read_to_string(path.join("user-note.md")).unwrap(),
                "user bytes\n",
                "{}",
                path.display()
            );
        }
    }

    fn assert_unauthorized(&self, fixture: &Fixture, deprojection: &ManagedDeprojection) {
        for path in self
            .user_paths
            .iter()
            .chain(std::iter::once(&fixture.retired_skills))
        {
            assert!(
                !deprojection.is_authorized_for_removal(path),
                "{}",
                path.display()
            );
        }
    }
}

#[cfg(not(windows))]
#[test]
fn invalid_legacy_rows_stay_quarantined_verbatim() {
    let fixture = Fixture::new();
    let invalid = InvalidRows::new(&fixture);
    let skill_key = legacy_key(&fixture.skill);
    let mut direct_rows = vec![json!(skill_key)];
    let mut attribution = Map::new();
    let mut proofs = Map::new();
    attribution.insert(skill_key.clone(), json!(SOURCE_ID));
    proofs.insert(
        skill_key.clone(),
        serde_json::to_value(fixture.standalone_record()).unwrap(),
    );
    for key in &invalid.keys {
        direct_rows.push(json!(key));
        attribution.insert(key.clone(), json!(SOURCE_ID));
        proofs.insert(key.clone(), InvalidRows::proof(key));
    }
    fixture.write_lock(json!({
        DIRECT_SKILLS: direct_rows,
        SKILLS: invalid.keys,
        ATTRIBUTION: attribution,
        STANDALONE: proofs,
    }));

    assert_eq!(
        direct_records(&fixture.lock),
        expected_answers(&fixture).records
    );
    assert_eq!(
        standalone_publication_records(&fixture.lock).unwrap(),
        vec![fixture.standalone_record()]
    );
    let deprojection = ManagedDeprojection::from_lockfile(&fixture.lock);
    assert!(deprojection.is_authorized_for_removal(&fixture.skill));
    invalid.assert_unauthorized(&fixture, &deprojection);

    fixture.update_skills();
    invalid.assert_preserved(&fixture);
    let meta = fixture.lock_meta();
    assert!(array_contains(
        &meta,
        DIRECT_SKILLS,
        &current_key(&fixture.skill)
    ));
    assert!(!array_contains(&meta, DIRECT_SKILLS, &skill_key));

    forget_direct_record(
        &fixture.lock,
        DirectArtifactKind::Skill,
        &fixture.skill,
        false,
    )
    .unwrap();
    invalid.assert_preserved(&fixture);
    assert!(direct_records(&fixture.lock).is_empty());
    assert!(standalone_publication_records(&fixture.lock)
        .unwrap()
        .is_empty());
}

#[cfg(not(windows))]
#[test]
fn lock_of_only_invalid_rows_does_not_authorize_first_run_mutation() {
    let fixture = Fixture::new();
    let invalid = InvalidRows::new(&fixture);
    let attribution: Map<String, Value> = invalid
        .keys
        .iter()
        .map(|key| (key.clone(), json!(SOURCE_ID)))
        .collect();
    let proofs: Map<String, Value> = invalid
        .keys
        .iter()
        .map(|key| (key.clone(), InvalidRows::proof(key)))
        .collect();
    fixture.write_lock(json!({
        DIRECT_SKILLS: invalid.keys,
        SKILLS: invalid.keys,
        ATTRIBUTION: attribution,
        STANDALONE: proofs,
    }));

    let deprojection = ManagedDeprojection::from_lockfile(&fixture.lock);
    assert!(!deprojection.is_empty());
    invalid.assert_unauthorized(&fixture, &deprojection);
    assert!(direct_records(&fixture.lock).is_empty());
    assert!(standalone_publication_records(&fixture.lock)
        .unwrap()
        .is_empty());

    fixture.update_skills();
    assert!(fixture.retired_skills.exists());
    invalid.assert_preserved(&fixture);
}
