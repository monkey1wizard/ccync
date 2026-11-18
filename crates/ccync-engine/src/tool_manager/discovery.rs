use crate::tool_manager::model::{EvidenceLevel, InstallationIdentity, SelectionState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryQuery {
    pub online: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryCandidate {
    pub candidate_id: String,
    pub identity: InstallationIdentity,
    pub evidence_level: EvidenceLevel,
    pub trackable: bool,
    pub recipe_compatible: bool,
    pub path_only: bool,
    #[serde(default)]
    pub shadowed_evidence: Vec<EvidenceLevel>,
}

impl DiscoveryCandidate {
    pub fn new(
        identity: InstallationIdentity,
        evidence_level: EvidenceLevel,
        trackable: bool,
        recipe_compatible: bool,
        path_only: bool,
    ) -> Self {
        let candidate_id = candidate_id(&identity);
        Self {
            candidate_id,
            identity,
            evidence_level,
            trackable: trackable && evidence_level != EvidenceLevel::Unknown,
            recipe_compatible,
            path_only,
            shadowed_evidence: vec![],
        }
    }
}

fn normalized_identity(identity: &InstallationIdentity) -> InstallationIdentity {
    let fields = identity
        .fields
        .iter()
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect::<BTreeMap<_, _>>();
    InstallationIdentity {
        adapter_kind: identity.adapter_kind,
        fields,
    }
}

pub fn candidate_id(identity: &InstallationIdentity) -> String {
    let normalized = normalized_identity(identity);
    let kind = serde_json::to_string(&normalized.adapter_kind).expect("adapter kind serializes");
    let bytes = serde_json::to_vec(&normalized.fields).expect("identity fields serialize");
    let mut hash = Sha256::new();
    hash.update(kind.as_bytes());
    hash.update([0]);
    hash.update(bytes);
    hex::encode(hash.finalize())[..16].to_string()
}

fn rank(level: EvidenceLevel) -> u8 {
    match level {
        EvidenceLevel::Verified => 4,
        EvidenceLevel::Manageable => 3,
        EvidenceLevel::Heuristic => 2,
        EvidenceLevel::Unknown => 1,
    }
}

pub fn merge_candidates(
    observations: impl IntoIterator<Item = DiscoveryCandidate>,
) -> Vec<DiscoveryCandidate> {
    let mut merged: BTreeMap<String, DiscoveryCandidate> = BTreeMap::new();
    for mut observation in observations {
        canonicalize_manual_path(&mut observation.identity);
        observation.candidate_id = candidate_id(&observation.identity);
        if observation.path_only && observation.evidence_level == EvidenceLevel::Verified {
            observation.evidence_level = EvidenceLevel::Heuristic;
        }
        if observation.evidence_level == EvidenceLevel::Unknown {
            observation.trackable = false;
        }
        let key = observation.candidate_id.clone();
        if let Some(current) = merged.get_mut(&key) {
            if rank(observation.evidence_level) > rank(current.evidence_level) {
                observation.shadowed_evidence.push(current.evidence_level);
                observation
                    .shadowed_evidence
                    .extend(current.shadowed_evidence.iter().copied());
                *current = observation;
            } else {
                current.shadowed_evidence.push(observation.evidence_level);
                current
                    .shadowed_evidence
                    .extend(observation.shadowed_evidence);
            }
        } else {
            merged.insert(key, observation);
        }
    }
    merged.into_values().collect()
}

fn canonicalize_manual_path(identity: &mut InstallationIdentity) {
    if identity.adapter_kind != crate::tool_manager::model::AdapterKind::ManualPath {
        return;
    }
    let Some(invoked) = identity.fields.get_mut("invokedPath") else {
        return;
    };
    let path = PathBuf::from(invoked.as_str());
    *invoked = std::fs::canonicalize(&path)
        .map(|path| canonical_executable_path(&path.to_string_lossy()))
        .unwrap_or_else(|_| canonical_executable_path(invoked))
        .to_owned();
}

/// Return candidates whose executable identity matches a user supplied selector.
///
/// A bare selector compares executable names. A path-like selector compares the
/// canonical executable path and keeps directory identity significant.
pub fn filter_by_executable_selector<'a>(
    candidates: &'a [DiscoveryCandidate],
    selector: &str,
) -> Vec<&'a DiscoveryCandidate> {
    candidates
        .iter()
        .filter(|candidate| {
            candidate
                .identity
                .fields
                .get("executable")
                .or_else(|| candidate.identity.fields.get("invokedPath"))
                .is_some_and(|executable| executable_selector_matches(executable, selector))
        })
        .collect()
}

/// Return the canonical executable identity used by exact selection and sorting.
pub fn canonical_executable_path(value: &str) -> String {
    let stripped = strip_verbatim_prefix(value);
    let path = Path::new(&stripped);
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let absolute = if resolved.is_absolute() {
        resolved
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(resolved)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    let mut result = normalized.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        result.make_ascii_lowercase();
    }
    result
}

fn strip_verbatim_prefix(value: &str) -> String {
    if let Some(path) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{path}");
    }
    value
        .strip_prefix(r"\\?\")
        .or_else(|| value.strip_prefix(r"\\.\"))
        .unwrap_or(value)
        .to_owned()
}

fn executable_basename(value: &str) -> &str {
    value.rsplit(['/', '\\']).next().unwrap_or(value)
}

fn normalized_executable_name(value: &str) -> (String, Option<String>) {
    let mut name = executable_basename(&strip_verbatim_prefix(value)).to_owned();
    if cfg!(windows) {
        name.make_ascii_lowercase();
        let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        for extension in extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
        {
            let extension = extension.to_ascii_lowercase();
            if name.ends_with(&extension) {
                name.truncate(name.len() - extension.len());
                return (name, Some(extension));
            }
        }
    }
    (name, None)
}

fn path_like_selector(selector: &str) -> bool {
    selector.contains('/')
        || selector.contains('\\')
        || selector.starts_with('.')
        || (selector.len() >= 2 && selector.as_bytes()[1] == b':')
}

fn executable_selector_matches(executable: &str, selector: &str) -> bool {
    if path_like_selector(selector) {
        return canonical_executable_path(executable) == canonical_executable_path(selector);
    }
    let (executable_name, executable_extension) = normalized_executable_name(executable);
    let (selector_name, selector_extension) = normalized_executable_name(selector);
    executable_name == selector_name
        && (executable_extension.is_none()
            || selector_extension.is_none()
            || executable_extension == selector_extension)
}

fn candidate_sort_key(candidate: &DiscoveryCandidate) -> (String, String, String) {
    let executable = candidate
        .identity
        .fields
        .get("executable")
        .or_else(|| candidate.identity.fields.get("invokedPath"))
        .map(|value| canonical_executable_path(value))
        .unwrap_or_default();
    (
        serde_json::to_string(&candidate.identity.adapter_kind).unwrap_or_default(),
        executable,
        candidate.candidate_id.clone(),
    )
}

fn sort_candidates(candidates: &mut [&DiscoveryCandidate]) {
    candidates.sort_by_key(|candidate| candidate_sort_key(candidate));
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub state: SelectionState,
    pub selected: Option<String>,
    pub limited_candidate_ids: Vec<String>,
    pub requires_accept_limited_evidence: bool,
}

pub fn select(candidates: &[DiscoveryCandidate]) -> Selection {
    let mut verified_candidates: Vec<_> = candidates
        .iter()
        .filter(|c| {
            c.evidence_level == EvidenceLevel::Verified
                && c.trackable
                && c.recipe_compatible
                && !c.path_only
        })
        .collect();
    sort_candidates(&mut verified_candidates);
    let mut verified: Vec<_> = verified_candidates
        .into_iter()
        .map(|c| c.candidate_id.clone())
        .collect();
    if verified.len() == 1 {
        return Selection {
            state: SelectionState::Unique,
            selected: Some(verified.remove(0)),
            limited_candidate_ids: vec![],
            requires_accept_limited_evidence: false,
        };
    }
    if verified.len() > 1 {
        return Selection {
            state: SelectionState::Ambiguous,
            selected: None,
            limited_candidate_ids: verified,
            requires_accept_limited_evidence: false,
        };
    }
    let mut limited: Vec<_> = candidates
        .iter()
        .filter(|c| {
            c.evidence_level != EvidenceLevel::Verified
                && c.evidence_level != EvidenceLevel::Unknown
                && c.trackable
                && c.recipe_compatible
        })
        .map(|c| c.candidate_id.clone())
        .collect();
    limited.sort();
    limited.dedup();
    Selection {
        state: SelectionState::None,
        selected: None,
        requires_accept_limited_evidence: !limited.is_empty(),
        limited_candidate_ids: limited,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::model::AdapterKind;
    fn c(name: &str, evidence_level: EvidenceLevel, path_only: bool) -> DiscoveryCandidate {
        DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields: [("name".into(), name.into())].into(),
            },
            evidence_level,
            true,
            true,
            path_only,
        )
    }
    #[test]
    fn merges_duplicates_and_retains_shadowed_evidence() {
        let merged = merge_candidates([
            c("a", EvidenceLevel::Heuristic, false),
            c(" a ", EvidenceLevel::Verified, false),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].evidence_level, EvidenceLevel::Verified);
        assert_eq!(merged[0].shadowed_evidence, [EvidenceLevel::Heuristic]);
    }

    #[test]
    fn merge_candidates_preserves_identity_field_case() {
        let observation = DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::Cargo,
                fields: [("packageName".into(), "example".into())].into(),
            },
            EvidenceLevel::Verified,
            true,
            true,
            false,
        );
        let expected_id = candidate_id(&observation.identity);

        let merged = merge_candidates([observation]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].identity.fields["packageName"], "example");
        assert_eq!(merged[0].candidate_id, expected_id);
    }
    #[test]
    fn selects_one_verified_and_marks_multiple_ambiguous() {
        assert_eq!(
            select(&[c("a", EvidenceLevel::Verified, false)]).state,
            SelectionState::Unique
        );
        assert_eq!(
            select(&[
                c("a", EvidenceLevel::Verified, false),
                c("b", EvidenceLevel::Verified, false)
            ])
            .state,
            SelectionState::Ambiguous
        );
    }
    #[test]
    fn heuristic_unknown_and_path_only_are_limited() {
        let heuristic = c("h", EvidenceLevel::Heuristic, false);
        let result = select(std::slice::from_ref(&heuristic));
        assert_eq!(result.state, SelectionState::None);
        assert!(result.requires_accept_limited_evidence);
        assert_eq!(result.limited_candidate_ids, [heuristic.candidate_id]);
        let mut unknown = c("u", EvidenceLevel::Unknown, false);
        unknown.trackable = true;
        assert!(!select(&[unknown]).requires_accept_limited_evidence);
        let result = select(&merge_candidates([c("p", EvidenceLevel::Verified, true)]));
        assert_eq!(result.state, SelectionState::None);
        assert!(result.requires_accept_limited_evidence);
    }
    #[test]
    fn candidate_ids_are_stable_across_input_order() {
        let a = c("a", EvidenceLevel::Heuristic, false);
        let b = c("b", EvidenceLevel::Heuristic, false);
        let ids = |v| {
            merge_candidates(v)
                .into_iter()
                .map(|c| c.candidate_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids([a.clone(), b.clone()]), ids([b, a]));
    }

    fn executable_candidate(adapter_kind: AdapterKind, executable: &str) -> DiscoveryCandidate {
        DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind,
                fields: [("executable".into(), executable.into())].into(),
            },
            EvidenceLevel::Verified,
            true,
            true,
            false,
        )
    }

    #[test]
    fn exact_selector_matches_bare_name_and_not_other_directories() {
        let candidates = vec![
            executable_candidate(AdapterKind::Cargo, "/opt/one/tool"),
            executable_candidate(AdapterKind::Cargo, "/opt/two/tool"),
            executable_candidate(AdapterKind::Cargo, "/opt/one/other"),
        ];
        let matches = filter_by_executable_selector(&candidates, "tool");
        assert_eq!(matches.len(), 2);
        assert!(filter_by_executable_selector(&candidates, "missing").is_empty());
        assert_eq!(
            filter_by_executable_selector(&candidates, "/opt/one/tool").len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_bare_selector_is_case_sensitive() {
        let candidates = vec![executable_candidate(AdapterKind::Cargo, "/opt/Tool")];
        assert!(filter_by_executable_selector(&candidates, "tool").is_empty());
        assert_eq!(filter_by_executable_selector(&candidates, "Tool").len(), 1);
    }

    #[test]
    fn ambiguous_selection_is_sorted_by_adapter_path_and_candidate() {
        let candidates = vec![
            executable_candidate(AdapterKind::InterpreterPip, "/z/tool"),
            executable_candidate(AdapterKind::Cargo, "/z/tool"),
            executable_candidate(AdapterKind::Cargo, "/a/tool"),
        ];
        let selection = select(&candidates);
        let expected = vec![
            candidates[2].candidate_id.clone(),
            candidates[1].candidate_id.clone(),
            candidates[0].candidate_id.clone(),
        ];
        assert_eq!(selection.state, SelectionState::Ambiguous);
        assert_eq!(selection.limited_candidate_ids, expected);
    }

    #[test]
    fn manual_path_invoked_path_is_canonicalized_before_identity_matching() {
        let candidate = DiscoveryCandidate::new(
            InstallationIdentity {
                adapter_kind: AdapterKind::ManualPath,
                fields: [("invokedPath".into(), "./target/../target/tool".into())].into(),
            },
            EvidenceLevel::Heuristic,
            true,
            false,
            true,
        );
        let merged = merge_candidates([candidate]);
        assert_eq!(
            merged[0].identity.fields["invokedPath"],
            canonical_executable_path("./target/../target/tool")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_selector_accepts_verbatim_drive_and_pathext_variants() {
        let candidates = vec![executable_candidate(
            AdapterKind::Cargo,
            r"C:\Tools\RTK.EXE",
        )];
        assert_eq!(filter_by_executable_selector(&candidates, "rtk").len(), 1);
        assert_eq!(
            filter_by_executable_selector(&candidates, "RTK.EXE").len(),
            1
        );
        assert_eq!(
            filter_by_executable_selector(&candidates, r"\\?\C:\Tools\rtk.exe").len(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_selector_preserves_unc_directory_identity() {
        let candidates = vec![executable_candidate(
            AdapterKind::Cargo,
            r"\\server\share\tools\rtk.exe",
        )];
        assert_eq!(
            filter_by_executable_selector(&candidates, r"\\?\UNC\server\share\tools\rtk.exe").len(),
            1
        );
        assert!(
            filter_by_executable_selector(&candidates, r"\\other-server\share\tools\rtk.exe")
                .is_empty()
        );
    }
}
