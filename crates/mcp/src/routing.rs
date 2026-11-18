use crate::host_render::Host;
use crate::state::MigrationSection;
use crate::{McpManifest, McpUpdateError};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RouteDecision {
    Hosts(BTreeSet<Host>),
    Unrouted,
    SourceHostOnly,
}

fn hosts(keys: &[String]) -> Result<BTreeSet<Host>, McpUpdateError> {
    keys.iter()
        .map(|key| {
            Host::ALL
                .into_iter()
                .find(|host| host.as_str() == key)
                .ok_or_else(|| {
                    McpUpdateError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("unknown MCP host: {key}"),
                    ))
                })
        })
        .collect()
}

pub(crate) fn route_decisions(
    manifest: &McpManifest,
    migration: Option<&MigrationSection>,
) -> Result<BTreeMap<String, RouteDecision>, McpUpdateError> {
    manifest
        .servers
        .iter()
        .map(|(name, server)| {
            let decision = match manifest.provenance.get(name) {
                None => RouteDecision::Hosts(Host::ALL.into_iter().collect()),
                Some(p) if p.source_preserved() => RouteDecision::SourceHostOnly,
                Some(p) if !p.enabled() => RouteDecision::Hosts(BTreeSet::new()),
                Some(p) if p.hosts().is_some() => RouteDecision::Hosts(hosts(p.hosts().unwrap())?),
                Some(p) => {
                    let route = migration
                        .and_then(|m| m.routes.iter().find(|r| r.identity() == p.identity()));
                    if server.command.is_some() {
                        match route {
                            Some(r) => RouteDecision::Hosts(hosts(r.hosts())?),
                            None => RouteDecision::Unrouted,
                        }
                    } else {
                        RouteDecision::Unrouted
                    }
                }
            };
            Ok((name.clone(), decision))
        })
        .collect()
}
