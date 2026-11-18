//! Closed built-in delivery recipes for tracked external tools.
use super::{
    model::{AdapterKind, DeliveryId, InstallationIdentity},
    probe::{ProbeRequest, ProbeRunner},
};
use std::{collections::BTreeMap, fs, path::Path, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryResult {
    pub named_value: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("delivery is incompatible with the recorded installation identity")]
    Incompatible,
    #[error("delivery command failed")]
    CommandFailed,
    #[error("delivery created an unexpected path: {0}")]
    UnexpectedPath(String),
    #[error("delivery filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}

pub fn compatible(delivery: DeliveryId, identity: &InstallationIdentity) -> bool {
    match delivery {
        DeliveryId::GraphifyAgents => {
            identity.adapter_kind == AdapterKind::InterpreterPip
                && identity
                    .fields
                    .get("distribution")
                    .is_some_and(|v| v.eq_ignore_ascii_case("graphifyy"))
                && identity
                    .fields
                    .get("interpreter")
                    .is_some_and(|v| Path::new(v).is_absolute())
        }
        DeliveryId::RtkPath => {
            identity.adapter_kind == AdapterKind::Cargo
                && identity.fields.get("name").is_some_and(|v| v == "rtk")
                && identity
                    .fields
                    .get("executable")
                    .is_some_and(|v| Path::new(v).is_absolute())
        }
        DeliveryId::ObserveOnly => false,
    }
}

pub fn run_recipe(
    delivery: DeliveryId,
    identity: &InstallationIdentity,
    staging: &Path,
    runner: &dyn ProbeRunner,
) -> Result<DeliveryResult, DeliveryError> {
    if !compatible(delivery, identity) {
        return Err(DeliveryError::Incompatible);
    }
    match delivery {
        DeliveryId::GraphifyAgents => {
            fs::create_dir_all(staging)?;
            let request = ProbeRequest {
                program: identity
                    .fields
                    .get("interpreter")
                    .expect("compatible interpreter")
                    .into(),
                argv: vec![
                    "-m".into(),
                    "graphify".into(),
                    "install".into(),
                    "--project".into(),
                    "agents".into(),
                ],
                env: BTreeMap::new(),
                working_directory: staging.to_path_buf(),
                timeout: Duration::from_secs(120),
                output_byte_cap: 1024 * 1024,
            };
            let output = runner.run(&request);
            if output.exit_code != Some(0)
                || output.timed_out
                || output.spawn_error.is_some()
                || output.stdout_truncated
                || output.stderr_truncated
            {
                return Err(DeliveryError::CommandFailed);
            }
            validate_graphify_tree(staging)?;
            Ok(DeliveryResult { named_value: None })
        }
        DeliveryId::RtkPath => Ok(DeliveryResult {
            named_value: identity.fields.get("executable").cloned(),
        }),
        DeliveryId::ObserveOnly => Err(DeliveryError::Incompatible),
    }
}

fn validate_graphify_tree(root: &Path) -> Result<(), DeliveryError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name() != ".agents" {
            return Err(DeliveryError::UnexpectedPath(
                entry.path().display().to_string(),
            ));
        }
        for agent in fs::read_dir(entry.path())? {
            let agent = agent?;
            if agent.file_name() != "skills" {
                return Err(DeliveryError::UnexpectedPath(
                    agent.path().display().to_string(),
                ));
            }
            for skill in fs::read_dir(agent.path())? {
                let skill = skill?;
                if skill.file_name() != "graphify" {
                    return Err(DeliveryError::UnexpectedPath(
                        skill.path().display().to_string(),
                    ));
                }
            }
        }
    }
    if !root.join(".agents/skills/graphify").is_dir() {
        return Err(DeliveryError::UnexpectedPath(
            "missing .agents/skills/graphify/".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_manager::probe::{FixtureProbeRunner, ProbeOutput};
    use std::collections::BTreeMap;
    fn identity(kind: AdapterKind, fields: &[(&str, &str)]) -> InstallationIdentity {
        InstallationIdentity {
            adapter_kind: kind,
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }
    #[test]
    fn graphify_runs_exact_argv_in_staging_and_rejects_extra_paths() {
        let interpreter = std::env::current_exe().unwrap();
        let id = identity(
            AdapterKind::InterpreterPip,
            &[
                ("interpreter", interpreter.to_str().unwrap()),
                ("distribution", "graphifyy"),
            ],
        );
        let runner = FixtureProbeRunner::new([ProbeOutput {
            exit_code: Some(0),
            ..Default::default()
        }]);
        let staging = tempfile::tempdir().unwrap();
        fs::create_dir_all(staging.path().join(".agents/skills/graphify")).unwrap();
        run_recipe(DeliveryId::GraphifyAgents, &id, staging.path(), &runner).unwrap();
        let req = runner.requests().remove(0);
        assert_eq!(
            req.argv,
            ["-m", "graphify", "install", "--project", "agents"]
        );
        assert_eq!(req.working_directory, staging.path());
        fs::write(staging.path().join("unexpected"), b"x").unwrap();
        let runner = FixtureProbeRunner::new([ProbeOutput {
            exit_code: Some(0),
            ..Default::default()
        }]);
        assert!(matches!(
            run_recipe(DeliveryId::GraphifyAgents, &id, staging.path(), &runner),
            Err(DeliveryError::UnexpectedPath(_))
        ));
    }
    #[test]
    fn rtk_compatibility_and_observe_only_denial() {
        let executable = std::env::current_exe().unwrap();
        let executable = executable.to_string_lossy();
        let rtk = identity(
            AdapterKind::Cargo,
            &[("name", "rtk"), ("executable", executable.as_ref())],
        );
        assert!(compatible(DeliveryId::RtkPath, &rtk));
        assert_eq!(
            run_recipe(
                DeliveryId::RtkPath,
                &rtk,
                Path::new("."),
                &FixtureProbeRunner::default()
            )
            .unwrap()
            .named_value
            .as_deref(),
            Some(executable.as_ref())
        );
        assert!(!compatible(DeliveryId::ObserveOnly, &rtk));
        assert!(run_recipe(
            DeliveryId::ObserveOnly,
            &rtk,
            Path::new("."),
            &FixtureProbeRunner::default()
        )
        .is_err());
    }
}
