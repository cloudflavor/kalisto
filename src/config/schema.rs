use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::backend::BackendKind;
use crate::cluster::addon::{CniPlugin, MetalLbConfig, MetalLbMode};
use crate::cluster::spec::{ClusterSpec, parse_image_arch};
use crate::error::{Error, Result};
use crate::harness::HarnessConfig;
use crate::k3s::ServerMode;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct KalistoConfig {
    pub cluster: ClusterConfig,
    pub nodes: NodesConfig,
    pub plugins: PluginsConfig,
    pub harness: HarnessConfig,
}

impl KalistoConfig {
    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn apply_to_cluster_spec(&self, spec: &mut ClusterSpec) -> Result<()> {
        if let Some(name) = &self.cluster.name {
            spec.name.clone_from(name);
        }
        if let Some(backend) = &self.cluster.backend {
            let backend = BackendKind::parse(backend).ok_or_else(|| {
                Error::InvalidArgs(format!(
                    "unsupported backend `{backend}` in kalisto.toml; expected one of: firecracker, lima, apple-container"
                ))
            })?;
            if spec.backend != backend {
                spec.network = crate::cluster::spec::default_network(backend);
            }
            spec.backend = backend;
        }
        if let Some(template) = &self.cluster.template {
            spec.template.clone_from(template);
        }
        if let Some(image_arch) = &self.cluster.arch {
            spec.image_arch = Some(
                parse_image_arch(image_arch)
                    .map_err(|message| Error::InvalidArgs(format!("{message} in kalisto.toml")))?,
            );
        }
        if let Some(network) = &self.cluster.network {
            spec.network.clone_from(network);
        }
        if let Some(api_port) = self.cluster.api_port {
            spec.api_port = api_port;
        }
        if let Some(k3s_version) = &self.cluster.k3s_version {
            spec.k3s_version.clone_from(k3s_version);
        }
        if let Some(enable_rosetta) = self.cluster.enable_rosetta {
            spec.enable_rosetta = enable_rosetta;
        }
        if let Some(kernel) = &self.cluster.firecracker_kernel {
            spec.firecracker_kernel = Some(resolve_relative_path(&spec.workspace_root, kernel));
        }
        if let Some(rootfs) = &self.cluster.firecracker_rootfs {
            spec.firecracker_rootfs = Some(resolve_relative_path(&spec.workspace_root, rootfs));
        }

        apply_node_config(&self.nodes, spec)?;
        apply_plugin_config(&self.plugins, spec)?;

        if self.cluster.agentless.unwrap_or(false) {
            spec.server_mode = ServerMode::Agentless;
            spec.worker_count = 0;
        }

        Ok(())
    }

    pub fn resolve_relative_paths(&mut self, project_root: &Path) {
        self.harness.resolve_relative_paths(project_root);
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterConfig {
    pub name: Option<String>,
    pub backend: Option<String>,
    pub template: Option<String>,
    pub arch: Option<String>,
    pub network: Option<String>,
    pub api_port: Option<u16>,
    pub k3s_version: Option<String>,
    pub enable_rosetta: Option<bool>,
    pub agentless: Option<bool>,
    pub firecracker_kernel: Option<PathBuf>,
    pub firecracker_rootfs: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct NodesConfig {
    pub defaults: NodeResourceConfig,
    pub control_plane: NodeResourceConfig,
    pub workers: Vec<WorkerNodeConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct NodeResourceConfig {
    pub cpus: Option<u8>,
    pub memory_gib: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerNodeConfig {
    pub name: Option<String>,
    pub cpus: Option<u8>,
    pub memory_gib: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    pub cilium: CiliumPluginConfig,
    pub flannel: EnabledPluginConfig,
    pub traefik: EnabledPluginConfig,
    pub kong: EnabledPluginConfig,
    pub servicelb: EnabledPluginConfig,
    pub metallb: MetallbPluginConfig,
    pub longhorn: EnabledPluginConfig,
    #[serde(rename = "local-path")]
    pub local_path: EnabledPluginConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct EnabledPluginConfig {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CiliumPluginConfig {
    pub enabled: Option<bool>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct MetallbPluginConfig {
    pub enabled: Option<bool>,
    pub mode: Option<String>,
    pub addresses: Vec<String>,
    pub config_file: Option<PathBuf>,
}

fn apply_node_config(config: &NodesConfig, spec: &mut ClusterSpec) -> Result<()> {
    if let Some(cpus) = config.defaults.cpus {
        spec.worker_resources.cpus = cpus;
    }
    if let Some(memory_gib) = config.defaults.memory_gib {
        spec.worker_resources.memory_gib = memory_gib;
    }
    if let Some(cpus) = config.control_plane.cpus {
        spec.control_plane_resources.cpus = cpus;
    }
    if let Some(memory_gib) = config.control_plane.memory_gib {
        spec.control_plane_resources.memory_gib = memory_gib;
    }
    if !config.workers.is_empty() {
        spec.worker_count = config.workers.len();
        let mut worker_cpus = None;
        let mut worker_memory_gib = None;
        for worker in &config.workers {
            if let Some(cpus) = worker.cpus {
                match worker_cpus {
                    Some(existing) if existing != cpus => {
                        return Err(Error::InvalidArgs(
                            "per-worker CPU overrides in kalisto.toml are not supported yet"
                                .to_owned(),
                        ));
                    }
                    None => worker_cpus = Some(cpus),
                    _ => {}
                }
            }
            if let Some(memory_gib) = worker.memory_gib {
                match worker_memory_gib {
                    Some(existing) if existing != memory_gib => {
                        return Err(Error::InvalidArgs(
                            "per-worker memory overrides in kalisto.toml are not supported yet"
                                .to_owned(),
                        ));
                    }
                    None => worker_memory_gib = Some(memory_gib),
                    _ => {}
                }
            }
        }
        if let Some(cpus) = worker_cpus {
            spec.worker_resources.cpus = cpus;
        }
        if let Some(memory_gib) = worker_memory_gib {
            spec.worker_resources.memory_gib = memory_gib;
        }
    }
    Ok(())
}

fn apply_plugin_config(config: &PluginsConfig, spec: &mut ClusterSpec) -> Result<()> {
    match (config.flannel.enabled, config.cilium.enabled) {
        (Some(true), Some(true)) => {
            return Err(Error::InvalidArgs(
                "kalisto.toml cannot enable both `plugins.flannel` and `plugins.cilium`".to_owned(),
            ));
        }
        (Some(true), _) => {
            spec.cni_plugin = CniPlugin::Flannel;
            spec.cni_plugin
                .apply_component_defaults(&mut spec.component_toggles);
        }
        (_, Some(true)) => {
            spec.cni_plugin = CniPlugin::Cilium;
            spec.cni_plugin
                .apply_component_defaults(&mut spec.component_toggles);
            if let Some(version) = &config.cilium.version {
                spec.cilium_version.clone_from(version);
            }
        }
        _ => {}
    }

    if config.servicelb.enabled == Some(true) {
        spec.component_toggles.disable_servicelb = false;
    }
    if config.traefik.enabled == Some(true) {
        push_addon(spec, "ingress/traefik");
    }
    if config.kong.enabled == Some(true) {
        push_addon(spec, "ingress/kong");
    }
    let metallb = parse_metallb_config(&config.metallb, &spec.workspace_root)?;
    if config.metallb.enabled == Some(false) && !metallb.is_empty() {
        return Err(Error::InvalidArgs(
            "kalisto.toml sets `plugins.metallb.enabled = false` but also defines MetalLB configuration"
                .to_owned(),
        ));
    }
    if config.metallb.enabled == Some(true) || !metallb.is_empty() {
        push_addon(spec, "lb/metallb");
    }
    if config.metallb.enabled == Some(true) || !metallb.is_empty() {
        spec.metallb = metallb;
        spec.metallb.validate()?;
    }
    if config.longhorn.enabled == Some(true) {
        push_addon(spec, "storage/longhorn");
    }
    if config.local_path.enabled == Some(true) {
        push_addon(spec, "storage/local-path");
    }

    Ok(())
}

fn push_addon(spec: &mut ClusterSpec, addon: &str) {
    if !spec.addons.iter().any(|installed| installed == addon) {
        spec.addons.push(addon.to_owned());
    }
}

fn parse_metallb_config(
    config: &MetallbPluginConfig,
    project_root: &Path,
) -> Result<MetalLbConfig> {
    let mode = match &config.mode {
        Some(mode) => Some(MetalLbMode::parse(mode).ok_or_else(|| {
            Error::InvalidArgs(format!(
                "unsupported `plugins.metallb.mode` value `{mode}` in kalisto.toml; expected one of: layer2"
            ))
        })?),
        None => None,
    };
    let config_file = config
        .config_file
        .as_ref()
        .map(|path| resolve_relative_path(project_root, path));
    Ok(MetalLbConfig {
        mode,
        addresses: config.addresses.clone(),
        config_file,
    })
}

fn resolve_relative_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::cluster::addon::{CniPlugin, DEFAULT_CILIUM_VERSION, MetalLbMode};
    use crate::cluster::spec::{ClusterSpec, ImageArch};

    use super::KalistoConfig;

    #[test]
    fn config_applies_cluster_and_plugin_settings_to_spec() {
        let config: KalistoConfig = toml::from_str(
            r#"
[cluster]
name = "dev"
backend = "lima"
api_port = 17443
enable_rosetta = false
arch = "amd64"

[nodes.defaults]
cpus = 3
memory_gib = 5

[nodes.control_plane]
cpus = 6
memory_gib = 8

[[nodes.workers]]
name = "w0"

[plugins.cilium]
enabled = true
version = "1.19.2"

[plugins.kong]
enabled = true

[plugins.metallb]
enabled = true
mode = "layer2"
addresses = ["192.168.50.240-192.168.50.250"]
"#,
        )
        .unwrap();
        let mut spec = ClusterSpec::new("default".to_owned(), PathBuf::from("/workspace"));

        config.apply_to_cluster_spec(&mut spec).unwrap();

        assert_eq!(spec.name, "dev");
        assert_eq!(spec.backend, crate::backend::BackendKind::Lima);
        assert_eq!(spec.network, "lima:user-v2");
        assert_eq!(spec.api_port, 17443);
        assert!(!spec.enable_rosetta);
        assert_eq!(spec.image_arch, Some(ImageArch::X86_64));
        assert_eq!(spec.control_plane_resources.cpus, 6);
        assert_eq!(spec.control_plane_resources.memory_gib, 8);
        assert_eq!(spec.worker_resources.cpus, 3);
        assert_eq!(spec.worker_resources.memory_gib, 5);
        assert_eq!(spec.worker_count, 1);
        assert_eq!(spec.cni_plugin, CniPlugin::Cilium);
        assert_eq!(spec.cilium_version, "1.19.2");
        assert!(spec.addons.iter().any(|addon| addon == "ingress/kong"));
        assert!(spec.addons.iter().any(|addon| addon == "lb/metallb"));
        assert_eq!(spec.metallb.mode, Some(MetalLbMode::Layer2));
        assert_eq!(
            spec.metallb.addresses,
            vec!["192.168.50.240-192.168.50.250".to_owned()]
        );
    }

    #[test]
    fn config_rejects_conflicting_cni_plugins() {
        let config: KalistoConfig = toml::from_str(
            r"
[plugins.cilium]
enabled = true

[plugins.flannel]
enabled = true
",
        )
        .unwrap();
        let mut spec = ClusterSpec::new("default".to_owned(), PathBuf::from("/workspace"));

        let error = config.apply_to_cluster_spec(&mut spec).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cannot enable both `plugins.flannel` and `plugins.cilium`")
        );
        assert_eq!(spec.cilium_version, DEFAULT_CILIUM_VERSION);
    }

    #[test]
    fn metallb_addresses_enable_addon_without_explicit_enabled_flag() {
        let config: KalistoConfig = toml::from_str(
            r#"
[plugins.metallb]
addresses = ["10.10.0.200-10.10.0.220"]
"#,
        )
        .unwrap();
        let mut spec = ClusterSpec::new("default".to_owned(), PathBuf::from("/workspace"));

        config.apply_to_cluster_spec(&mut spec).unwrap();

        assert!(spec.addons.iter().any(|addon| addon == "lb/metallb"));
        assert_eq!(
            spec.metallb.addresses,
            vec!["10.10.0.200-10.10.0.220".to_owned()]
        );
    }

    #[test]
    fn metallb_config_file_is_resolved_relative_to_project_root() {
        let config: KalistoConfig = toml::from_str(
            r#"
[plugins.metallb]
enabled = true
config_file = "manifests/metallb.yaml"
"#,
        )
        .unwrap();
        let mut spec = ClusterSpec::new("default".to_owned(), PathBuf::from("/workspace"));

        config.apply_to_cluster_spec(&mut spec).unwrap();

        assert_eq!(
            spec.metallb.config_file,
            Some(PathBuf::from("/workspace/manifests/metallb.yaml"))
        );
    }

    #[test]
    fn config_parses_named_harness_scenarios() {
        let config: KalistoConfig = toml::from_str(
            r#"
[[harness.scenarios]]
name = "smoke"
namespace = "demo"
manifests = ["k8s/app.yaml"]

[[harness.scenarios.waits]]
kind = "deployment"
name = "api"

[[harness.scenarios.probes]]
type = "command"
name = "pods-present"
command = "kubectl -n \"$KALISTO_NAMESPACE\" get pods"
"#,
        )
        .unwrap();

        assert_eq!(config.harness.scenarios.len(), 1);
        assert_eq!(config.harness.scenarios[0].name, "smoke");
        assert_eq!(config.harness.scenarios[0].namespace, "demo");
    }
}
