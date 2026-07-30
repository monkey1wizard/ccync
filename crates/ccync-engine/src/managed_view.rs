//! Unified managed-item view over the lockfile.
//!
//! ccync's lockfile carries two independent namespaces that both describe
//! items the user actually has installed:
//! - `_personalPlugins` — ccync-catalog-managed plugins (reference-is-truth;
//!   `pluginId` / `source` / `pin` / `held`).
//! - `_adoptedItems` — items `ccync init` found already installed in a master
//!   agent and adopted read-only (`name` / `sourceId` / `source` / `version`).
//!
//! Every other command (`list`, `show`, `upgrade`, `remove`) should see items
//! through a single merged view rather than re-deriving namespace logic per
//! verb. This module provides that single merged view: pure, read-only, and
//! panic-free over an arbitrary `serde_json::Value`.

use serde_json::Value;

const PERSONAL_KEY: &str = "_personalPlugins";
const ADOPTED_KEY: &str = "_adoptedItems";

/// Where a managed item's truth is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Came from `_personalPlugins` (ccync-catalog-managed).
    Personal,
    /// Came from `_adoptedItems` (adopted from a master agent, read-only).
    Adopted,
}

/// What kind of managed content this item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Plugin,
    McpServer,
    Skill,
}

impl ItemKind {
    /// Classify from an adopted entry's `sourceId` tag (see `adopt.rs::source_id`).
    fn from_source_id(source_id: &str) -> ItemKind {
        if source_id.contains("mcp") {
            ItemKind::McpServer
        } else if source_id.contains("skill") {
            ItemKind::Skill
        } else {
            ItemKind::Plugin
        }
    }
}

/// A single item in the merged managed view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedItem {
    /// Stable identifier: `pluginId` for personal entries, `name` for adopted
    /// entries.
    pub id: String,
    pub kind: ItemKind,
    pub origin: Origin,
    /// Resolvable source reference, when known (absent for MCP/skill items
    /// and for unresolved adopted plugin sources).
    pub source: Option<String>,
    /// Pinned/resolved commit sha, when recorded (personal entries only).
    pub pin: Option<String>,
    /// `true` when the user has pinned this item against `upgrade` (personal
    /// entries only; `ccync pin`).
    pub held: bool,
    /// `true` when an adopted entry with this id is shadowed by a personal
    /// entry of the same id (personal always wins duplicate ids). Always
    /// `false` for personal entries and for adopted entries with no personal
    /// duplicate.
    pub shadowed: bool,
}

/// Read `_personalPlugins` as a raw entry array. Missing/malformed → empty.
fn personal_array(lock: &Value) -> &[Value] {
    lock.get(PERSONAL_KEY)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Read `_adoptedItems` as a raw entry array. Missing/malformed → empty.
fn adopted_array(lock: &Value) -> &[Value] {
    lock.get(ADOPTED_KEY)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn personal_item(entry: &Value) -> Option<ManagedItem> {
    let id = entry.get("pluginId").and_then(Value::as_str)?.to_string();
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_string);
    let pin = ccync_foundation::pins::entry_pin(entry);
    let held = entry.get("held").and_then(Value::as_bool).unwrap_or(false);
    Some(ManagedItem {
        id,
        kind: ItemKind::Plugin,
        origin: Origin::Personal,
        source,
        pin,
        held,
        shadowed: false,
    })
}

fn adopted_item(entry: &Value) -> Option<ManagedItem> {
    let id = entry.get("name").and_then(Value::as_str)?.to_string();
    let kind = entry
        .get("sourceId")
        .and_then(Value::as_str)
        .map(ItemKind::from_source_id)
        .unwrap_or(ItemKind::Plugin);
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(ManagedItem {
        id,
        kind,
        origin: Origin::Adopted,
        source,
        pin: None,
        held: false,
        shadowed: false,
    })
}

/// Build the merged managed view from a lockfile `Value`.
///
/// Reads `_personalPlugins` and `_adoptedItems`; either namespace missing or
/// malformed is treated as empty (no panic). When the same id appears in both
/// namespaces, the personal entry wins: it is the one returned, and the
/// shadowed adopted duplicate is omitted from this view. Use
/// [`build_managed_view_with_shadows`] for a view that keeps shadowed adopted
/// entries visible (marked `shadowed: true`) instead of dropping them.
pub fn build_managed_view(lock: &Value) -> Vec<ManagedItem> {
    build_managed_view_with_shadows(lock)
        .into_iter()
        .filter(|item| !(item.origin == Origin::Adopted && item.shadowed))
        .collect()
}

/// Build the merged managed view, keeping shadowed adopted entries in the
/// output (marked `shadowed: true`) rather than dropping them. Personal
/// entries always win duplicate ids: for a shared id the personal entry is
/// unmodified and the adopted entry is retained only with `shadowed: true`.
///
/// Order: personal entries first (in lockfile order), then adopted entries
/// (in lockfile order, shadowed ones included at the end).
pub fn build_managed_view_with_shadows(lock: &Value) -> Vec<ManagedItem> {
    let personal: Vec<ManagedItem> = personal_array(lock)
        .iter()
        .filter_map(personal_item)
        .collect();
    let personal_ids: std::collections::HashSet<String> =
        personal.iter().map(|i| i.id.clone()).collect();

    let mut out = personal;
    for entry in adopted_array(lock) {
        let Some(mut item) = adopted_item(entry) else {
            continue;
        };
        if personal_ids.contains(&item.id) {
            item.shadowed = true;
        }
        out.push(item);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_personal_and_adopted_namespaces() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "p1", "source": "https://x/p1.git", "pin": "sha1" }
            ],
            "_adoptedItems": [
                { "name": "a1", "adoptedFrom": "claude", "version": "1.0.0", "sourceId": "claude-mcp" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 2);
        let p1 = view.iter().find(|i| i.id == "p1").unwrap();
        assert_eq!(p1.origin, Origin::Personal);
        assert_eq!(p1.kind, ItemKind::Plugin);
        assert_eq!(p1.pin.as_deref(), Some("sha1"));
        assert!(!p1.shadowed);

        let a1 = view.iter().find(|i| i.id == "a1").unwrap();
        assert_eq!(a1.origin, Origin::Adopted);
        assert_eq!(a1.kind, ItemKind::McpServer);
        assert!(!a1.shadowed);
    }

    #[test]
    fn classifies_by_explicit_source_id_namespace() {
        let lock = json!({
            "_adoptedItems": [
                { "name": "plug@mkt", "sourceId": "claude-plugin", "adoptedFrom": "claude", "version": "1.0.0" },
                { "name": "srv", "sourceId": "claude-mcp", "adoptedFrom": "claude", "version": null },
                { "name": "skill-x", "sourceId": "claude-skill", "adoptedFrom": "claude", "version": null }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 3);
        assert_eq!(
            view.iter().find(|i| i.id == "plug@mkt").unwrap().kind,
            ItemKind::Plugin
        );
        assert_eq!(
            view.iter().find(|i| i.id == "srv").unwrap().kind,
            ItemKind::McpServer
        );
        assert_eq!(
            view.iter().find(|i| i.id == "skill-x").unwrap().kind,
            ItemKind::Skill
        );
    }

    #[test]
    fn personal_wins_duplicate_id_and_adopted_is_shadowed() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "dup", "source": "https://x/dup.git", "pin": "newsha" }
            ],
            "_adoptedItems": [
                { "name": "dup", "sourceId": "claude-plugin", "adoptedFrom": "claude", "version": "0.1.0", "source": "mkt:dup@0.1.0" }
            ]
        });

        // Visible view: personal wins, adopted duplicate is suppressed.
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        let dup = &view[0];
        assert_eq!(dup.origin, Origin::Personal);
        assert_eq!(dup.pin.as_deref(), Some("newsha"));

        // Shadow-inclusive view: the adopted duplicate is retained, marked shadowed.
        let with_shadows = build_managed_view_with_shadows(&lock);
        assert_eq!(with_shadows.len(), 2);
        let personal_dup = with_shadows
            .iter()
            .find(|i| i.origin == Origin::Personal)
            .unwrap();
        assert!(
            !personal_dup.shadowed,
            "personal entry itself is never shadowed"
        );
        let adopted_dup = with_shadows
            .iter()
            .find(|i| i.origin == Origin::Adopted)
            .unwrap();
        assert!(
            adopted_dup.shadowed,
            "adopted duplicate must be marked shadowed"
        );
    }

    #[test]
    fn missing_namespaces_do_not_panic() {
        // Completely empty object.
        let empty = json!({});
        assert!(build_managed_view(&empty).is_empty());
        assert!(build_managed_view_with_shadows(&empty).is_empty());

        // Only personal present.
        let only_personal = json!({
            "_personalPlugins": [{ "pluginId": "p", "source": "s" }]
        });
        assert_eq!(build_managed_view(&only_personal).len(), 1);

        // Only adopted present.
        let only_adopted = json!({
            "_adoptedItems": [{ "name": "a", "sourceId": "claude-mcp" }]
        });
        assert_eq!(build_managed_view(&only_adopted).len(), 1);

        // Namespaces present but wrong shape (not arrays) must not panic.
        let malformed = json!({
            "_personalPlugins": "not-an-array",
            "_adoptedItems": { "not": "an-array" }
        });
        assert!(build_managed_view(&malformed).is_empty());

        // Lockfile is a non-object Value entirely.
        let not_object = json!("just a string");
        assert!(build_managed_view(&not_object).is_empty());

        // Entries missing required id fields are skipped, not panicking.
        let missing_ids = json!({
            "_personalPlugins": [{ "source": "s" }],
            "_adoptedItems": [{ "sourceId": "claude-mcp" }]
        });
        assert!(build_managed_view(&missing_ids).is_empty());
    }

    #[test]
    fn held_field_is_read_for_personal_entries() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "held-one", "source": "s", "pin": "sha", "held": true },
                { "pluginId": "not-held", "source": "s", "pin": "sha" }
            ]
        });
        let view = build_managed_view(&lock);
        assert!(view.iter().find(|i| i.id == "held-one").unwrap().held);
        assert!(!view.iter().find(|i| i.id == "not-held").unwrap().held);
    }

    #[test]
    fn duplicate_ids_within_same_namespace_do_not_panic_or_dedup() {
        // Two personal entries sharing an id: cross-namespace dedup only
        // targets personal-vs-adopted collisions, so both survive here and
        // neither is marked shadowed.
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "same", "source": "https://x/a.git" },
                { "pluginId": "same", "source": "https://x/b.git" }
            ],
            "_adoptedItems": [
                { "name": "same-adopted", "sourceId": "claude-plugin" },
                { "name": "same-adopted", "sourceId": "claude-mcp" }
            ]
        });
        let view = build_managed_view(&lock);
        let personal_dups: Vec<_> = view.iter().filter(|i| i.id == "same").collect();
        assert_eq!(
            personal_dups.len(),
            2,
            "same-namespace personal duplicates are not deduped"
        );
        assert!(personal_dups.iter().all(|i| !i.shadowed));

        let adopted_dups: Vec<_> = view.iter().filter(|i| i.id == "same-adopted").collect();
        assert_eq!(
            adopted_dups.len(),
            2,
            "same-namespace adopted duplicates are not deduped"
        );
        assert!(adopted_dups.iter().all(|i| !i.shadowed));
    }

    #[test]
    fn unknown_source_id_defaults_to_plugin_and_missing_source_id_defaults_to_plugin() {
        let lock = json!({
            "_adoptedItems": [
                { "name": "weird", "sourceId": "totally-unrecognized-tag" },
                { "name": "no-tag" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(
            view.iter().find(|i| i.id == "weird").unwrap().kind,
            ItemKind::Plugin
        );
        assert_eq!(
            view.iter().find(|i| i.id == "no-tag").unwrap().kind,
            ItemKind::Plugin
        );
    }

    #[test]
    fn one_namespace_malformed_does_not_suppress_the_other() {
        // Adopted namespace is malformed (object, not array); personal is
        // well-formed. The malformed namespace must degrade to empty without
        // affecting the valid namespace's entries.
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "still-here", "source": "s" }
            ],
            "_adoptedItems": { "not": "an-array" }
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].id, "still-here");
    }

    #[test]
    fn old_lockfile_no_adopted_items_shows_personal_only() {
        // An older lockfile that has only `_personalPlugins` and no `_adoptedItems`
        // key must return the personal items and never panic.
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "legacy-plugin", "source": "https://github.com/user/legacy.git" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].id, "legacy-plugin");
        assert_eq!(view[0].origin, Origin::Personal);
    }

    #[test]
    fn personal_pin_preserved_in_managed_view() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "pinned-plugin", "source": "https://github.com/user/pinned.git", "pin": "abc123def456xyz" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].pin.as_deref(), Some("abc123def456xyz"));
    }

    #[test]
    fn personal_held_flag_preserved_in_managed_view() {
        let lock = json!({
            "_personalPlugins": [
                { "pluginId": "held-plugin", "source": "https://github.com/user/held.git", "held": true }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert!(view[0].held);
    }

    #[test]
    fn old_lockfile_adopted_item_without_baseline_version_is_visible() {
        // Old-format adopted entry has no `baselineVersion` field.
        // `build_managed_view` must still include it (origin=Adopted) and never panic.
        let lock = json!({
            "_adoptedItems": [
                { "name": "old-adopted-plugin", "adoptedFrom": "claude", "version": "1.0.0", "sourceId": "claude-plugin" }
            ]
        });
        let view = build_managed_view(&lock);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].id, "old-adopted-plugin");
        assert_eq!(view[0].origin, Origin::Adopted);
    }
}
