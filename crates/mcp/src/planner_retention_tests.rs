use crate::entry::EntryFingerprint;
use crate::host_render::HostEntry;
use crate::planner::plan_host;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn entry(command: &str) -> HostEntry {
    HostEntry::Json(json!({"command": command}))
}

fn fingerprint(entry: &HostEntry) -> EntryFingerprint {
    entry.fingerprint().unwrap()
}

#[test]
fn retained_owned_live_name_stays_owned_without_an_action() {
    let retained_entry = entry("retained");
    let committed = BTreeSet::from([String::from("retained")]);
    let live = BTreeMap::from([(String::from("retained"), fingerprint(&retained_entry))]);
    let retained = BTreeSet::from([String::from("retained")]);

    let plan = plan_host(
        &committed,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &retained,
    )
    .unwrap();

    assert_eq!(plan.next_owned, committed);
    assert!(plan.overlays.is_empty());
    assert!(plan.removals.is_empty());
    assert!(plan.collisions.is_empty());
}

#[test]
fn retained_owned_name_absent_from_live_is_not_kept() {
    let committed = BTreeSet::from([String::from("retained")]);
    let retained = BTreeSet::from([String::from("retained")]);

    let plan = plan_host(
        &committed,
        &BTreeMap::new(),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &retained,
    )
    .unwrap();

    assert!(plan.next_owned.is_empty());
    assert!(plan.overlays.is_empty());
    assert!(plan.removals.is_empty());
    assert!(plan.collisions.is_empty());
}

#[test]
fn retained_unowned_live_name_has_no_action_or_claim() {
    let live_entry = entry("user");
    let live = BTreeMap::from([(String::from("retained"), fingerprint(&live_entry))]);
    let retained = BTreeSet::from([String::from("retained")]);

    let plan = plan_host(
        &BTreeSet::new(),
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &retained,
    )
    .unwrap();

    assert!(plan.legacy_claims.is_empty());
    assert!(plan.next_owned.is_empty());
    assert!(plan.overlays.is_empty());
    assert!(plan.removals.is_empty());
    assert!(plan.collisions.is_empty());
}

#[test]
fn unretained_owned_live_name_is_removed() {
    let old = entry("old");
    let committed = BTreeSet::from([String::from("old")]);
    let live = BTreeMap::from([(String::from("old"), fingerprint(&old))]);

    let plan = plan_host(
        &committed,
        &live,
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::new(),
    )
    .unwrap();

    assert_eq!(plan.removals, committed);
    assert!(plan.next_owned.is_empty());
}

use crate::host_render::Host;
use crate::routing::{route_decisions, RouteDecision};
use ccync_foundation::mcp::{DeclarationIdentity, McpManifest, McpServerProvenance, McpSourceKind};

fn routed_manifest(hosts: Option<Vec<String>>, enabled: bool, preserved: bool) -> McpManifest {
    let mut manifest: McpManifest =
        serde_json::from_value(json!({"servers": {"srv": {"command": "node"}}})).unwrap();
    manifest.set_provenance(
        "srv".into(),
        McpServerProvenance::new(
            DeclarationIdentity {
                source: "direct".into(),
                server: "srv".into(),
            },
            McpSourceKind::Direct,
            None,
            String::new(),
            hosts,
            enabled,
            preserved,
            false,
        ),
    );
    manifest
}

fn decision(manifest: &McpManifest) -> RouteDecision {
    route_decisions(manifest, None)
        .unwrap()
        .remove("srv")
        .unwrap()
}

#[test]
fn routing_explicit_hosts_publish_only_there() {
    let mut manifest = routed_manifest(Some(vec!["claude".into(), "codex".into()]), true, false);
    let server = manifest.servers.get_mut("srv").unwrap();
    server.server_type = Some("http".into());
    server.command = None;
    server.url = Some("https://example.test/mcp".into());
    let routes = route_decisions(&manifest, None).unwrap();
    let (rendered, _) =
        crate::render_desired_by_host(&manifest, std::path::Path::new("missing"), &routes).unwrap();
    for host in Host::ALL {
        assert_eq!(
            rendered[&host].contains_key("srv"),
            matches!(host, Host::Claude | Host::Codex)
        );
    }
}

#[test]
fn routing_unrouted_stdio_publishes_nowhere_and_keeps_owned_copy() {
    let manifest = routed_manifest(None, true, false);
    assert_eq!(decision(&manifest), RouteDecision::Unrouted);
    let routes = route_decisions(&manifest, None).unwrap();
    let (rendered, _) =
        crate::render_desired_by_host(&manifest, std::path::Path::new("missing"), &routes).unwrap();
    assert!(rendered.values().all(BTreeMap::is_empty));
    retained_owned_live_name_stays_owned_without_an_action();
}

#[test]
fn routing_migrated_route_used_without_explicit_hosts() {
    let manifest = routed_manifest(None, true, false);
    let migration = crate::state::MigrationSection {
        complete: true,
        routes: vec![crate::MigratedRoute::new(
            manifest.provenance["srv"].identity().clone(),
            vec!["codex".into()],
        )],
        unproven: vec![],
    };
    assert_eq!(
        route_decisions(&manifest, Some(&migration)).unwrap()["srv"],
        RouteDecision::Hosts(BTreeSet::from([Host::Codex]))
    );
}

#[test]
fn routing_empty_set_withdraws_only_addressable_owned_copies() {
    let manifest = routed_manifest(Some(vec![]), true, false);
    assert_eq!(decision(&manifest), RouteDecision::Hosts(BTreeSet::new()));
    let committed = BTreeSet::from(["srv".into()]);
    let live = BTreeMap::from([("srv".into(), fingerprint(&entry("node")))]);
    for blocked in [false, true] {
        let unaddressable = if blocked {
            committed.clone()
        } else {
            BTreeSet::new()
        };
        let plan = plan_host(
            &committed,
            &live,
            &unaddressable,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(plan.removals.contains("srv"), !blocked);
        assert_eq!(plan.collisions.contains("srv"), blocked);
    }
}

#[test]
fn routing_source_preserved_publishes_nowhere() {
    assert_eq!(
        decision(&routed_manifest(Some(vec!["claude".into()]), true, true)),
        RouteDecision::SourceHostOnly
    );
}

#[test]
fn routing_missing_secret_retains_only_on_routed_hosts() {
    let mut manifest = routed_manifest(Some(vec!["claude".into()]), true, false);
    manifest.servers.get_mut("srv").unwrap().command = Some("${MCP_SECRET_ROUTING_TEST}".into());
    let routes = route_decisions(&manifest, None).unwrap();
    let (_, skipped) =
        crate::render_desired_by_host(&manifest, std::path::Path::new("missing"), &routes).unwrap();
    assert!(skipped.contains_key("srv"));
    let RouteDecision::Hosts(hosts) = &routes["srv"] else {
        panic!()
    };
    assert!(hosts.contains(&Host::Claude));
    assert!(!hosts.contains(&Host::Codex));
}

#[test]
fn routing_identical_unowned_on_routed_host_is_conflict() {
    let value = entry("node");
    let plan = plan_host(
        &BTreeSet::new(),
        &BTreeMap::from([("srv".into(), fingerprint(&value))]),
        &BTreeSet::new(),
        &BTreeMap::new(),
        &BTreeMap::from([("srv".into(), value)]),
        &BTreeSet::new(),
    )
    .unwrap();
    assert!(plan.collisions.contains("srv"));
}

#[test]
fn routing_conflict_isolates_one_host_and_others_commit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let managed = tmp.path().join("managed.json");
    let claude = tmp.path().join("claude.json");
    let copilot = tmp.path().join("copilot.json");
    std::fs::write(&claude, r#"{"mcpServers":{"srv":{"command":"user"}}}"#).unwrap();
    let paths = BTreeMap::from([
        (Host::Claude, claude.clone()),
        (Host::Copilot, copilot.clone()),
    ]);
    let live = crate::read_live_entries(&paths);
    let desired = BTreeMap::from([
        (
            Host::Claude,
            BTreeMap::from([("srv".into(), entry("node"))]),
        ),
        (
            Host::Copilot,
            BTreeMap::from([("srv".into(), entry("node"))]),
        ),
    ]);
    let report = crate::apply_projection_transaction(
        &managed,
        &paths,
        &desired,
        &live.by_host,
        &live.unaddressable_by_host,
        &BTreeMap::new(),
        &BTreeMap::new(),
        "test",
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(report.host_failures.len(), 1);
    assert_eq!(report.host_failures[0].0, "claude");
    assert!(std::fs::read_to_string(claude).unwrap().contains("user"));
    assert!(std::fs::read_to_string(copilot).unwrap().contains("node"));
    let state =
        crate::state::ProjectedState::load(&crate::state::ProjectedState::path_beside(&managed))
            .unwrap();
    assert!(state.owned(Host::Claude).is_empty());
    assert!(state.owned(Host::Copilot).contains("srv"));
}

#[test]
fn routing_manifest_without_provenance_keeps_all_hosts() {
    let mut manifest = routed_manifest(None, true, false);
    manifest.provenance.clear();
    assert_eq!(
        decision(&manifest),
        RouteDecision::Hosts(Host::ALL.into_iter().collect())
    );
}
