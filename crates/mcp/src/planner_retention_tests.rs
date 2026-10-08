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
