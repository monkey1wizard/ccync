//! Pure normal-mode ownership planning for one host.
//!
//! Given, for a single host, the committed owned-name set, the current live
//! entry fingerprints, the *prior* manifest's rendered entries, and the *desired*
//! rendered entries, decide per server name exactly one action:
//!
//! - **overlay** — write the desired entry (name absent live, or owned and
//!   changed).
//! - **no-op** — desired already equals live (owned); skip the write.
//! - **removal** — an owned name no longer desired; delete it from the host.
//! - **collision** — the host has a same-named entry ccync does *not* own (not
//!   committed), *or* one whose live layout that
//!   host's writer cannot faithfully address. Fail closed: never overwrite it.
//!
//! Pure: no I/O, and no classification of interrupted transactions (that is the
//! separate recovery classifier).

use crate::entry::EntryFingerprint;
use crate::host_render::HostEntry;
use crate::transaction::ProjectionTransaction;
use crate::McpUpdateError;
use std::collections::{BTreeMap, BTreeSet};

/// The planned per-host actions plus the owned-name set that results once the
/// plan is applied. Names are partitioned; a collision blocks writing that name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HostPlan {
    /// Un-committed names adopted via exact prior-render match (migration).
    pub legacy_claims: BTreeSet<String>,
    /// Names to write the desired entry for.
    pub overlays: BTreeSet<String>,
    /// Desired names whose live entry already matches (skip the write).
    pub noops: BTreeSet<String>,
    /// Owned names no longer desired, still present live (delete them).
    pub removals: BTreeSet<String>,
    /// Unowned same-name entries (fail closed — never overwritten).
    pub collisions: BTreeSet<String>,
    /// Owned-name set after this plan is applied.
    pub next_owned: BTreeSet<String>,
}

/// Plan the normal-mode actions for one host. See the module docs for the
/// decision rules. Fingerprinting a Codex TOML entry can fail; hence `Result`.
///
/// `unaddressable` names live entries this host's writer cannot faithfully
/// address (currently: a Codex server defined as an inline key under a plain
/// `[mcp_servers]` table, which the line-based writer cannot strip). Ownership
/// of such a name is never claimed and never acted on — writing it would append
/// a duplicate key and removing it would silently do nothing — so it fails
/// closed as a collision no matter what the committed state says.
pub(crate) fn plan_host(
    committed: &BTreeSet<String>,
    live: &BTreeMap<String, EntryFingerprint>,
    unaddressable: &BTreeSet<String>,
    _prior: &BTreeMap<String, HostEntry>,
    desired: &BTreeMap<String, HostEntry>,
    retained: &BTreeSet<String>,
) -> Result<HostPlan, McpUpdateError> {
    let mut plan = HostPlan::default();
    debug_assert!(retained.iter().all(|name| !desired.contains_key(name)));

    let owned = committed.clone();

    // Retained names are skipped desired entries. Keep ownership only when the
    // live entry still exists; an absent entry has no live proof to retain.
    for name in retained {
        if owned.contains(name) && live.contains_key(name) {
            plan.next_owned.insert(name.clone());
        }
    }

    // Desired names: overlay / no-op / collision.
    for (name, desired_entry) in desired {
        let desired_fp = desired_entry.fingerprint()?;
        match live.get(name) {
            None => {
                // Nothing live under this name: safe to write and own.
                plan.overlays.insert(name.clone());
                plan.next_owned.insert(name.clone());
            }
            Some(live_fp) => {
                if unaddressable.contains(name) {
                    // The writer cannot replace this entry in place; overlaying
                    // would leave the original beside a duplicate definition.
                    plan.collisions.insert(name.clone());
                } else if owned.contains(name) {
                    if live_fp == &desired_fp {
                        plan.noops.insert(name.clone());
                    } else {
                        plan.overlays.insert(name.clone());
                    }
                    plan.next_owned.insert(name.clone());
                } else {
                    // Same-named entry ccync cannot prove it owns → fail closed.
                    plan.collisions.insert(name.clone());
                }
            }
        }
    }

    // Removals: owned names no longer desired. Delete them if still present live;
    // either way they leave the owned set.
    for name in &owned {
        if desired.contains_key(name) {
            continue;
        }
        if retained.contains(name) {
            continue;
        }
        if !live.contains_key(name) {
            continue;
        }
        if unaddressable.contains(name) {
            // The writer would strip nothing, so recording this as a removal
            // would journal `post=absent`, drop ownership, and report success
            // while the entry survived — permanently unremovable.
            plan.collisions.insert(name.clone());
        } else {
            plan.removals.insert(name.clone());
        }
    }

    Ok(plan)
}

// ─────────────────────────────────────────────────────────────────────────────
// Interrupted-transaction recovery (separate from normal planning)
// ─────────────────────────────────────────────────────────────────────────────

/// Classification of one journaled transaction entry against the current live
/// fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryClass {
    /// Live == pre: the write did not land. Prior ownership is preserved as-is.
    NotLanded,
    /// Live == post: the write landed.
    Landed,
    /// Live is neither pre nor post: ambiguous. Fail closed; never change
    /// ownership.
    Third,
}

/// Outcome of classifying an interrupted transaction. `landed_by_host` names the
/// pairs to grant ownership; `third_states` names the ambiguous pairs that make
/// recovery fatal. NotLanded pairs appear only in `classes` — they preserve
/// prior ownership by producing no grant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecoveryOutcome {
    /// One `(host, name, class)` per journaled entry, in journal order.
    pub classes: Vec<(String, String, RecoveryClass)>,
    /// host -> names whose write landed (grant ownership).
    pub landed_by_host: BTreeMap<String, BTreeSet<String>>,
    pub removed_by_host: BTreeMap<String, BTreeSet<String>>,
    /// `(host, name)` pairs in the ambiguous third state — fail closed.
    pub third_states: Vec<(String, String)>,
}

impl RecoveryOutcome {
    /// True when any pair is ambiguous (recovery must fail closed).
    pub(crate) fn has_third_state(&self) -> bool {
        !self.third_states.is_empty()
    }
}

/// Classify an interrupted transaction against the current live fingerprints.
///
/// `live` maps `(host, name)` to the live entry's fingerprint; a pair absent from
/// the map is treated as `absent`. For each journaled entry, the live token is
/// compared to `pre` (not landed), then `post` (landed), else third (ambiguous).
/// Pure: it never mutates ownership — it only reports what the caller may grant
/// and which pairs are fatal.
pub(crate) fn classify_recovery(
    txn: &ProjectionTransaction,
    live: &BTreeMap<(String, String), EntryFingerprint>,
) -> RecoveryOutcome {
    let mut out = RecoveryOutcome::default();
    for entry in &txn.entries {
        let key = (entry.host.clone(), entry.name.clone());
        let live_token = live
            .get(&key)
            .map(|fp| fp.as_token())
            .unwrap_or_else(|| EntryFingerprint::Absent.as_token());

        let class = if live_token == entry.pre {
            RecoveryClass::NotLanded
        } else if live_token == entry.post {
            RecoveryClass::Landed
        } else {
            RecoveryClass::Third
        };

        match class {
            RecoveryClass::Landed => {
                let target = if entry.post == EntryFingerprint::Absent.as_token() {
                    &mut out.removed_by_host
                } else {
                    &mut out.landed_by_host
                };
                target
                    .entry(entry.host.clone())
                    .or_default()
                    .insert(entry.name.clone());
            }
            RecoveryClass::NotLanded => { /* preserve prior ownership: no grant */ }
            RecoveryClass::Third => out.third_states.push(key.clone()),
        }
        out.classes
            .push((entry.host.clone(), entry.name.clone(), class));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn je(v: serde_json::Value) -> HostEntry {
        HostEntry::Json(v)
    }

    fn fp(entry: &HostEntry) -> EntryFingerprint {
        entry.fingerprint().unwrap()
    }

    fn names(set: &BTreeSet<String>) -> Vec<&str> {
        set.iter().map(|s| s.as_str()).collect()
    }

    /// "the writer can address every live name here" — true for all JSON hosts
    /// and for any Codex file whose servers each have their own table header.
    fn all_addressable() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn projection_planner_overlay_and_noop_for_owned() {
        let keep = je(json!({"command":"keep"}));
        let update_old = je(json!({"command":"old"}));
        let update_new = je(json!({"command":"new"}));
        let fresh = je(json!({"command":"fresh"}));

        let committed: BTreeSet<String> = ["keep".into(), "update".into()].into();
        let live: BTreeMap<String, EntryFingerprint> = [
            ("keep".into(), fp(&keep)),
            ("update".into(), fp(&update_old)),
        ]
        .into();
        let prior: BTreeMap<String, HostEntry> = BTreeMap::new();
        let desired: BTreeMap<String, HostEntry> = [
            ("keep".into(), keep.clone()),
            ("update".into(), update_new.clone()),
            ("brand-new".into(), fresh.clone()),
        ]
        .into();

        let plan = plan_host(
            &committed,
            &live,
            &all_addressable(),
            &prior,
            &desired,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&plan.noops), vec!["keep"]);
        assert_eq!(names(&plan.overlays), vec!["brand-new", "update"]);
        assert!(plan.removals.is_empty());
        assert!(plan.collisions.is_empty());
        assert_eq!(names(&plan.next_owned), vec!["brand-new", "keep", "update"]);
    }

    #[test]
    fn projection_planner_removes_owned_undesired() {
        let old = je(json!({"command":"old"}));
        let committed: BTreeSet<String> = ["old".into()].into();
        let live: BTreeMap<String, EntryFingerprint> = [("old".into(), fp(&old))].into();
        let plan = plan_host(
            &committed,
            &live,
            &all_addressable(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&plan.removals), vec!["old"]);
        assert!(plan.next_owned.is_empty());
        assert!(plan.overlays.is_empty());
    }

    #[test]
    fn projection_planner_unowned_same_name_is_collision_fail_closed() {
        let user_entry = je(json!({"command":"user-bin"}));
        let ccync_wants = je(json!({"command":"ccync-bin"}));
        // No committed ownership, no prior render → cannot prove ownership.
        let live: BTreeMap<String, EntryFingerprint> = [("shared".into(), fp(&user_entry))].into();
        let desired: BTreeMap<String, HostEntry> = [("shared".into(), ccync_wants)].into();

        let plan = plan_host(
            &BTreeSet::new(),
            &live,
            &all_addressable(),
            &BTreeMap::new(),
            &desired,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&plan.collisions), vec!["shared"]);
        assert!(
            plan.overlays.is_empty(),
            "must not overwrite an unowned entry"
        );
        assert!(plan.next_owned.is_empty());
        assert!(!plan.collisions.is_empty());
    }

    #[test]
    fn projection_planner_exact_match_bootstrap_only() {
        // Matching a prior render does not establish ownership.
        let m = je(json!({"command":"m"}));
        let m2 = je(json!({"command":"m2"}));
        let user = je(json!({"command":"user"}));
        let ccync_user = je(json!({"command":"ccync"}));

        let prior: BTreeMap<String, HostEntry> =
            [("m".into(), m.clone()), ("m2".into(), m2.clone())].into();
        let live: BTreeMap<String, EntryFingerprint> = [
            ("m".into(), fp(&m)),
            ("m2".into(), fp(&m2)),
            ("user".into(), fp(&user)),
        ]
        .into();
        let desired: BTreeMap<String, HostEntry> =
            [("m".into(), m.clone()), ("user".into(), ccync_user)].into();

        let plan = plan_host(
            &BTreeSet::new(),
            &live,
            &all_addressable(),
            &prior,
            &desired,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(plan.legacy_claims.is_empty());
        assert!(plan.noops.is_empty());
        assert!(plan.removals.is_empty());
        assert_eq!(names(&plan.collisions), vec!["m", "user"]);
        assert!(plan.next_owned.is_empty());
    }

    #[test]
    fn projection_planner_unaddressable_live_layout_is_a_collision() {
        // A Codex server the semantic reader sees but the line-based writer
        // cannot strip (an inline key under a plain `[mcp_servers]` table).
        let entry = je(json!({"command":"x"}));
        let unaddressable: BTreeSet<String> = ["foo".into()].into();
        let live: BTreeMap<String, EntryFingerprint> = [("foo".into(), fp(&entry))].into();

        // (a) still desired, and ccync even has committed ownership of it:
        //     overlaying would append a second definition beside the surviving
        //     inline key → duplicate key → Codex cannot load its config.
        let desired: BTreeMap<String, HostEntry> = [("foo".into(), entry.clone())].into();
        let plan = plan_host(
            &["foo".to_string()].into(),
            &live,
            &unaddressable,
            &BTreeMap::new(),
            &desired,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&plan.collisions), vec!["foo"], "must fail closed");
        assert!(
            plan.overlays.is_empty(),
            "must not overlay an unaddressable entry"
        );
        assert!(plan.noops.is_empty(), "must not silently call it a no-op");

        // (b) no longer desired, and owned: a removal would strip nothing, yet
        //     journal post=absent and drop ownership → silent non-removal,
        //     permanently unremovable.
        let plan = plan_host(
            &["foo".to_string()].into(),
            &live,
            &unaddressable,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&plan.collisions), vec!["foo"], "must fail closed");
        assert!(
            plan.removals.is_empty(),
            "must not journal a removal the writer cannot perform"
        );

        // (c) an exact prior-render match must NOT be legacy-claimed: the
        //     layouts are semantically identical, so the fingerprint matches.
        let prior: BTreeMap<String, HostEntry> = [("foo".into(), entry.clone())].into();
        let plan = plan_host(
            &BTreeSet::new(),
            &live,
            &unaddressable,
            &prior,
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(
            plan.legacy_claims.is_empty(),
            "a fingerprint match must not claim an entry the writer cannot address"
        );
        assert!(plan.removals.is_empty());
        assert!(plan.next_owned.is_empty());

        // Control: the same name with its own table header is addressable and
        // behaves exactly as before.
        let plan = plan_host(
            &["foo".to_string()].into(),
            &live,
            &all_addressable(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&plan.removals), vec!["foo"]);
        assert!(plan.collisions.is_empty());
    }

    #[test]
    fn projection_planner_hosts_are_independent() {
        // Same function, different per-host inputs → different plans.
        let a = je(json!({"command":"a"}));
        let b = je(json!({"command":"b"}));
        let host1 = plan_host(
            &BTreeSet::new(),
            &BTreeMap::new(),
            &all_addressable(),
            &BTreeMap::new(),
            &[("a".into(), a.clone())].into(),
            &BTreeSet::new(),
        )
        .unwrap();
        let host2 = plan_host(
            &BTreeSet::new(),
            &BTreeMap::new(),
            &all_addressable(),
            &BTreeMap::new(),
            &[("b".into(), b.clone())].into(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(names(&host1.overlays), vec!["a"]);
        assert_eq!(names(&host2.overlays), vec!["b"]);
    }

    // --- recovery classifier ---
    use crate::entry::fingerprint_json;
    use crate::host_render::Host;
    use crate::transaction::ProjectionTransaction;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn transaction_recovery_classifier_grants_landed_preserves_pre_rejects_third() {
        let new = fingerprint_json(&json!({"command":"new"}));
        let old = fingerprint_json(&json!({"command":"old"}));
        let weird = fingerprint_json(&json!({"command":"weird"}));

        let mut txn = ProjectionTransaction::new("t");
        // Landed: pre=old, post=new, live=new.
        txn.record(Host::Claude, "landed", &old, &new);
        // Not landed: pre=old, post=new, live=old → preserve prior ownership.
        txn.record(Host::Copilot, "notlanded", &old, &new);
        // Third: pre=old, post=new, live=weird → ambiguous fatal.
        txn.record(Host::Codex, "third", &old, &new);
        // Landed removal: pre=old, post=absent, live=absent (not in map).
        txn.record(Host::OpenCode, "removed", &old, &EntryFingerprint::Absent);

        let live: BTreeMap<(String, String), EntryFingerprint> = [
            (("claude".to_string(), "landed".to_string()), new.clone()),
            (
                ("copilot".to_string(), "notlanded".to_string()),
                old.clone(),
            ),
            (("codex".to_string(), "third".to_string()), weird.clone()),
            // opencode/removed intentionally absent.
        ]
        .into();

        let out = classify_recovery(&txn, &live);

        // Only landed pairs grant ownership.
        assert_eq!(out.landed_by_host.get("claude").unwrap(), &set(&["landed"]));
        assert_eq!(
            out.removed_by_host.get("opencode").unwrap(),
            &set(&["removed"])
        );
        assert!(!out.landed_by_host.contains_key("opencode"));
        assert!(
            !out.landed_by_host.contains_key("copilot"),
            "not-landed grants nothing (prior ownership preserved)"
        );
        // Third state named exactly and fatal.
        assert_eq!(
            out.third_states,
            vec![("codex".to_string(), "third".to_string())]
        );
        assert!(out.has_third_state());
    }

    #[test]
    fn transaction_recovery_classifier_clean_when_all_landed() {
        let new = fingerprint_json(&json!({"command":"new"}));
        let old = fingerprint_json(&json!({"command":"old"}));
        let mut txn = ProjectionTransaction::new("t");
        txn.record(Host::Claude, "a", &old, &new);
        let live: BTreeMap<(String, String), EntryFingerprint> =
            [(("claude".to_string(), "a".to_string()), new.clone())].into();
        let out = classify_recovery(&txn, &live);
        assert!(!out.has_third_state());
        assert_eq!(out.landed_by_host.get("claude").unwrap(), &set(&["a"]));
    }
}
