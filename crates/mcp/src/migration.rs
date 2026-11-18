use crate::host_render::Host;
use crate::state::{MigrationSection, ProjectedState, UnprovenRoute};
use crate::{McpManifest, MigratedRoute};
use std::collections::BTreeMap;

pub(crate) fn migrate_legacy_routes(
    state: &ProjectedState,
    prior: Option<&McpManifest>,
    desired: &McpManifest,
) -> MigrationSection {
    if let Some(section) = &state.migration {
        return section.clone();
    }
    let mut routes: BTreeMap<(String, String), MigratedRoute> = BTreeMap::new();
    let mut unproven = Vec::new();
    for host in Host::ALL {
        for name in state.owned(host) {
            let (Some(provenance), Some(server)) =
                (desired.provenance.get(&name), desired.servers.get(&name))
            else {
                continue;
            };
            if server.command.is_none() || provenance.hosts().is_some() {
                continue;
            }
            let mut missing = Vec::new();
            if !provenance.enabled() {
                missing.push("disabled".to_string());
            }
            match prior.and_then(|p| p.servers.get(&name)) {
                None => missing.push("no-prior-definition".to_string()),
                Some(previous)
                    if serde_json::to_value(previous).ok() != serde_json::to_value(server).ok() =>
                {
                    missing.push("prior-definition-changed".to_string())
                }
                _ => {}
            }
            if prior
                .and_then(|p| p.provenance.get(&name))
                .is_some_and(|p| {
                    p.identity() != provenance.identity()
                        || p.unresolved_digest() != provenance.unresolved_digest()
                })
            {
                missing.push("identity-changed".to_string());
            }
            if missing.is_empty() {
                let identity = provenance.identity();
                let route = routes
                    .entry((identity.source.clone(), identity.server.clone()))
                    .or_insert_with(|| MigratedRoute {
                        identity: identity.clone(),
                        digest: provenance.unresolved_digest().to_string(),
                        hosts: Vec::new(),
                    });
                route.hosts.push(host.as_str().to_string());
            } else {
                unproven.push(UnprovenRoute {
                    host: host.as_str().to_string(),
                    server: name,
                    missing,
                });
            }
        }
    }
    MigrationSection {
        complete: true,
        routes: routes.into_values().collect(),
        unproven,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccync_foundation::mcp::{DeclarationIdentity, McpServerProvenance, McpSourceKind};
    use std::collections::BTreeSet;

    fn manifest(command: &str, hosts: Option<Vec<String>>) -> McpManifest {
        let mut manifest: McpManifest =
            serde_json::from_value(serde_json::json!({"servers":{"srv":{"command":command}}}))
                .unwrap();
        manifest.set_provenance(
            "srv".into(),
            McpServerProvenance::new(
                DeclarationIdentity {
                    source: "package".into(),
                    server: "srv".into(),
                },
                McpSourceKind::Package,
                None,
                "digest".into(),
                hosts,
                true,
                false,
                false,
            ),
        );
        manifest
    }
    fn owned() -> ProjectedState {
        let mut state = ProjectedState::new();
        for host in [Host::Claude, Host::Codex] {
            state.set_owned(host, BTreeSet::from(["srv".into()]));
        }
        state
    }
    #[test]
    fn migration_v1_owned_unrouted_stdio_gets_fixed_route() {
        let state = owned();
        let original = state.clone();
        let m = manifest("node", None);
        let result = migrate_legacy_routes(&state, Some(&m), &m);
        assert_eq!(result.routes.len(), 1);
        assert_eq!(result.routes[0].hosts(), &["claude", "codex"]);
        assert!(result.unproven.is_empty());
        assert_eq!(state, original);
    }
    #[test]
    fn migration_changed_definition_stays_unproven() {
        let state = owned();
        let before = state.clone();
        let result =
            migrate_legacy_routes(&state, Some(&manifest("old", None)), &manifest("new", None));
        assert!(result.routes.is_empty());
        assert!(result
            .unproven
            .iter()
            .all(|u| u.missing == ["prior-definition-changed"]));
        assert_eq!(state, before);
    }
    #[test]
    fn migration_explicit_route_or_remote_is_skipped() {
        let state = owned();
        let mut m = manifest("node", Some(vec![]));
        assert!(migrate_legacy_routes(&state, Some(&m), &m)
            .unproven
            .is_empty());
        m.servers.get_mut("srv").unwrap().command = None;
        let result = migrate_legacy_routes(&state, Some(&m), &m);
        assert!(result.routes.is_empty() && result.unproven.is_empty());
    }
    #[test]
    fn migration_rerun_never_widens_or_revives() {
        let mut state = owned();
        let m = manifest("node", None);
        let mut section = migrate_legacy_routes(&state, Some(&m), &m);
        section.routes[0].hosts.pop();
        state.migration = Some(section.clone());
        state.set_owned(Host::Agy, BTreeSet::from(["srv".into()]));
        assert_eq!(migrate_legacy_routes(&state, Some(&m), &m), section);
    }
    #[test]
    fn migration_inspect_never_migrates() {
        let temp = tempfile::tempdir().unwrap();
        let managed = temp.path().join("managed.json");
        let path = ProjectedState::path_beside(&managed);
        owned().save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        crate::prepare_projection_recovery(
            &managed,
            None,
            &Default::default(),
            &Default::default(),
            &Default::default(),
            crate::PrepareMode::Inspect,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
    #[test]
    fn migration_state_v1_loads_and_unknown_version_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        std::fs::write(&path, r#"{"version":1,"hosts":{}}"#).unwrap();
        assert!(ProjectedState::load(&path).unwrap().migration.is_none());
        std::fs::write(&path, r#"{"version":3,"hosts":{}}"#).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(ProjectedState::load(&path).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
    #[test]
    fn restored_routes_are_idempotent_and_do_not_grant_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let managed = temp.path().join("managed.json");
        let route = MigratedRoute::new(
            DeclarationIdentity {
                source: "package".into(),
                server: "srv".into(),
            },
            vec!["codex".into(), "claude".into(), "codex".into()],
        );
        assert!(crate::read_migrated_routes(&managed).unwrap().is_empty());
        assert!(!ProjectedState::path_beside(&managed).exists());
        crate::commit_restored_routes(&managed, &[route.clone()]).unwrap();
        let path = ProjectedState::path_beside(&managed);
        let before = std::fs::read(&path).unwrap();
        let read = crate::read_migrated_routes(&managed).unwrap();
        assert_eq!(read[0].hosts(), &["claude", "codex"]);
        assert!(ProjectedState::load(&path).unwrap().hosts.is_empty());
        crate::commit_restored_routes(&managed, &[route]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let collision = MigratedRoute::new(
            DeclarationIdentity {
                source: "other".into(),
                server: "srv".into(),
            },
            vec!["codex".into()],
        );
        assert!(crate::commit_restored_routes(&managed, &[collision]).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}
