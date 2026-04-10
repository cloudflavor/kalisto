use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessConfig {
    pub scenarios: Vec<ScenarioConfig>,
}

impl HarnessConfig {
    pub fn resolve_relative_paths(&mut self, project_root: &Path) {
        for scenario in &mut self.scenarios {
            for manifest in &mut scenario.manifests {
                if !manifest.is_absolute() {
                    *manifest = project_root.join(&*manifest);
                }
            }
        }
    }

    pub fn scenario(&self, name: &str) -> Option<&ScenarioConfig> {
        self.scenarios.iter().find(|scenario| scenario.name == name)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScenarioConfig {
    pub name: String,
    pub namespace: String,
    pub cluster_lifecycle: ScenarioClusterLifecycle,
    pub cluster: ScenarioClusterConfig,
    pub manifests: Vec<PathBuf>,
    pub waits: Vec<WaitConfig>,
    pub probes: Vec<ProbeConfig>,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            namespace: default_namespace(),
            cluster_lifecycle: ScenarioClusterLifecycle::default(),
            cluster: ScenarioClusterConfig::default(),
            manifests: Vec::new(),
            waits: Vec::new(),
            probes: Vec::new(),
        }
    }
}

impl ScenarioConfig {
    pub fn validate(&self) -> crate::error::Result<()> {
        if self.name.trim().is_empty() {
            return Err(crate::error::Error::InvalidArgs(
                "harness scenarios require a non-empty `name`".to_owned(),
            ));
        }
        if self.namespace.trim().is_empty() {
            return Err(crate::error::Error::InvalidArgs(format!(
                "harness scenario `{}` requires a non-empty `namespace`",
                self.name
            )));
        }
        if self.manifests.is_empty() && self.waits.is_empty() && self.probes.is_empty() {
            return Err(crate::error::Error::InvalidArgs(format!(
                "harness scenario `{}` must define at least one manifest, wait, or probe",
                self.name
            )));
        }
        Ok(())
    }
}

fn default_namespace() -> String {
    "default".to_owned()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ScenarioClusterConfig {
    pub workers: usize,
    pub agentless: bool,
    pub addons: Vec<String>,
}

impl Default for ScenarioClusterConfig {
    fn default() -> Self {
        Self {
            workers: 0,
            agentless: false,
            addons: vec!["cni/flannel".to_owned()],
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ScenarioClusterLifecycle {
    Ephemeral,
    #[default]
    Persistent,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum WaitConfig {
    Deployment {
        name: String,
        #[serde(default = "default_timeout_secs")]
        timeout_secs: u64,
    },
    #[serde(rename = "statefulset")]
    StatefulSet {
        name: String,
        #[serde(default = "default_timeout_secs")]
        timeout_secs: u64,
    },
    #[serde(rename = "daemonset")]
    DaemonSet {
        name: String,
        #[serde(default = "default_timeout_secs")]
        timeout_secs: u64,
    },
    Job {
        name: String,
        #[serde(default = "default_timeout_secs")]
        timeout_secs: u64,
    },
}

impl WaitConfig {
    pub fn label(&self) -> String {
        match self {
            Self::Deployment { name, .. } => format!("deployment/{name}"),
            Self::StatefulSet { name, .. } => format!("statefulset/{name}"),
            Self::DaemonSet { name, .. } => format!("daemonset/{name}"),
            Self::Job { name, .. } => format!("job/{name}"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ProbeConfig {
    Command(CommandProbeConfig),
}

impl ProbeConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Command(probe) => &probe.name,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandProbeConfig {
    pub name: String,
    pub command: String,
}

fn default_timeout_secs() -> u64 {
    180
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        HarnessConfig, ProbeConfig, ScenarioClusterConfig, ScenarioClusterLifecycle, WaitConfig,
    };

    #[test]
    fn harness_config_resolves_relative_manifest_paths() {
        let mut config: HarnessConfig = toml::from_str(
            r#"
[[scenarios]]
name = "smoke"
namespace = "demo"
manifests = ["k8s/app.yaml"]
"#,
        )
        .unwrap();

        config.resolve_relative_paths(Path::new("/workspace"));

        assert_eq!(
            config.scenarios[0].manifests[0],
            Path::new("/workspace/k8s/app.yaml")
        );
    }

    #[test]
    fn scenario_toml_parses_waits_and_command_probes() {
        let config: HarnessConfig = toml::from_str(
            r#"
[[scenarios]]
name = "smoke"
cluster_lifecycle = "ephemeral"

[scenarios.cluster]
workers = 0
agentless = false
addons = ["cni/flannel"]

[[scenarios.waits]]
kind = "deployment"
name = "api"
timeout_secs = 90

[[scenarios.probes]]
type = "command"
name = "deployment-ready"
command = "kubectl -n \"$KALISTO_NAMESPACE\" get deploy api"
"#,
        )
        .unwrap();

        let scenario = &config.scenarios[0];
        assert_eq!(
            scenario.cluster_lifecycle,
            ScenarioClusterLifecycle::Ephemeral
        );
        assert_eq!(
            scenario.cluster,
            ScenarioClusterConfig {
                workers: 0,
                agentless: false,
                addons: vec!["cni/flannel".to_owned()]
            }
        );
        assert_eq!(
            scenario.waits,
            vec![WaitConfig::Deployment {
                name: "api".to_owned(),
                timeout_secs: 90
            }]
        );
        assert_eq!(
            scenario.probes,
            vec![ProbeConfig::Command(super::CommandProbeConfig {
                name: "deployment-ready".to_owned(),
                command: "kubectl -n \"$KALISTO_NAMESPACE\" get deploy api".to_owned()
            })]
        );
    }

    #[test]
    fn scenario_validation_rejects_empty_steps() {
        let config: HarnessConfig = toml::from_str(
            r#"
[[scenarios]]
name = "empty"
"#,
        )
        .unwrap();

        let error = config.scenarios[0].validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must define at least one manifest")
        );
    }

    #[test]
    fn scenario_cluster_defaults_to_single_schedulable_control_plane() {
        let config: HarnessConfig = toml::from_str(
            r#"
[[scenarios]]
name = "smoke"

[[scenarios.probes]]
type = "command"
name = "pods"
command = "kubectl get pods"
"#,
        )
        .unwrap();

        assert_eq!(
            config.scenarios[0].cluster,
            ScenarioClusterConfig {
                workers: 0,
                agentless: false,
                addons: vec!["cni/flannel".to_owned()]
            }
        );
    }
}
