use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::BackendKind;
use crate::cluster::addon::{CniPlugin, MetalLbConfig, MetalLbMode};
use crate::cluster::spec::NodeResources;
use crate::error::{Error, Result};
use crate::k3s::{ComponentToggles, NodeRole, ServerMode};

#[derive(Debug, Clone)]
pub struct NodeState {
    pub name: String,
    pub role: NodeRole,
    pub artifacts_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ClusterState {
    pub name: String,
    pub backend: BackendKind,
    pub workspace_root: PathBuf,
    pub template: String,
    pub firecracker_kernel: Option<PathBuf>,
    pub firecracker_rootfs: Option<PathBuf>,
    pub api_port: u16,
    pub worker_count: usize,
    pub control_plane_resources: NodeResources,
    pub worker_resources: NodeResources,
    pub enable_rosetta: bool,
    pub network: String,
    pub k3s_version: String,
    pub cni_plugin: CniPlugin,
    pub cilium_version: String,
    pub addons: Vec<String>,
    pub metallb: MetalLbConfig,
    pub server_mode: ServerMode,
    pub component_toggles: ComponentToggles,
    pub kubeconfig_path: PathBuf,
    pub token_path: PathBuf,
    pub nodes: Vec<NodeState>,
}

impl ClusterState {
    #[must_use]
    pub fn state_file(root: &Path) -> PathBuf {
        root.join("cluster.state")
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn save(&self, root: &Path) -> Result<()> {
        fs::create_dir_all(root).map_err(|source| Error::CommandIo {
            program: "create_dir_all".to_owned(),
            source,
        })?;

        let content = self.to_bytes()?;
        fs::write(Self::state_file(root), content).map_err(|source| Error::CommandIo {
            program: "write".to_owned(),
            source,
        })
    }

    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(&PersistedClusterState::from_state(self)).map_err(|error| {
            Error::InvalidArgs(format!(
                "failed to serialize cluster state for {}: {error}",
                self.name
            ))
        })
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn load(root: &Path) -> Result<Self> {
        let content =
            fs::read_to_string(Self::state_file(root)).map_err(|source| Error::CommandIo {
                program: "read_to_string".to_owned(),
                source,
            })?;

        Self::from_bytes(content.as_bytes())
    }

    pub(crate) fn from_bytes(content: &[u8]) -> Result<Self> {
        let persisted: PersistedClusterState = serde_json::from_slice(content)
            .map_err(|error| Error::InvalidArgs(format!("invalid cluster state json: {error}")))?;
        persisted.try_into_state()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedNodeResources {
    cpus: u8,
    memory_gib: u8,
}

impl From<NodeResources> for PersistedNodeResources {
    fn from(value: NodeResources) -> Self {
        Self {
            cpus: value.cpus,
            memory_gib: value.memory_gib,
        }
    }
}

impl From<PersistedNodeResources> for NodeResources {
    fn from(value: PersistedNodeResources) -> Self {
        Self {
            cpus: value.cpus,
            memory_gib: value.memory_gib,
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // CLI arguments / persisted config surface; bool-per-flag is the design
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedComponentToggles {
    disable_traefik: bool,
    disable_servicelb: bool,
    disable_metrics_server: bool,
    disable_local_storage: bool,
    disable_coredns: bool,
    disable_network_policy: bool,
    disable_kube_proxy: bool,
    flannel_backend_none: bool,
}

impl From<ComponentToggles> for PersistedComponentToggles {
    fn from(value: ComponentToggles) -> Self {
        Self {
            disable_traefik: value.disable_traefik,
            disable_servicelb: value.disable_servicelb,
            disable_metrics_server: value.disable_metrics_server,
            disable_local_storage: value.disable_local_storage,
            disable_coredns: value.disable_coredns,
            disable_network_policy: value.disable_network_policy,
            disable_kube_proxy: value.disable_kube_proxy,
            flannel_backend_none: value.flannel_backend_none,
        }
    }
}

impl From<PersistedComponentToggles> for ComponentToggles {
    fn from(value: PersistedComponentToggles) -> Self {
        Self {
            disable_traefik: value.disable_traefik,
            disable_servicelb: value.disable_servicelb,
            disable_metrics_server: value.disable_metrics_server,
            disable_local_storage: value.disable_local_storage,
            disable_coredns: value.disable_coredns,
            disable_network_policy: value.disable_network_policy,
            disable_kube_proxy: value.disable_kube_proxy,
            flannel_backend_none: value.flannel_backend_none,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedMetalLbConfig {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    addresses: Vec<String>,
    #[serde(default)]
    config_file: Option<PathBuf>,
}

impl PersistedMetalLbConfig {
    fn from_state(value: &MetalLbConfig) -> Self {
        Self {
            mode: value.mode.map(MetalLbMode::as_str).map(str::to_owned),
            addresses: value.addresses.clone(),
            config_file: value.config_file.clone(),
        }
    }

    fn try_into_state(self) -> Result<MetalLbConfig> {
        Ok(MetalLbConfig {
            mode: self.mode.as_deref().map(parse_metallb_mode).transpose()?,
            addresses: self.addresses,
            config_file: self.config_file,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedNodeState {
    name: String,
    role: String,
    artifacts_root: PathBuf,
}

impl PersistedNodeState {
    fn from_state(value: &NodeState) -> Self {
        Self {
            name: value.name.clone(),
            role: node_role_label(value.role).to_owned(),
            artifacts_root: value.artifacts_root.clone(),
        }
    }

    fn try_into_state(self) -> Result<NodeState> {
        Ok(NodeState {
            name: self.name,
            role: parse_node_role(&self.role)?,
            artifacts_root: self.artifacts_root,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedClusterState {
    format_version: u32,
    name: String,
    backend: String,
    workspace_root: PathBuf,
    template: String,
    firecracker_kernel: Option<PathBuf>,
    firecracker_rootfs: Option<PathBuf>,
    api_port: u16,
    worker_count: usize,
    control_plane_resources: PersistedNodeResources,
    worker_resources: PersistedNodeResources,
    enable_rosetta: bool,
    network: String,
    k3s_version: String,
    cni_plugin: String,
    cilium_version: String,
    addons: Vec<String>,
    #[serde(default)]
    metallb: PersistedMetalLbConfig,
    server_mode: String,
    component_toggles: PersistedComponentToggles,
    kubeconfig_path: PathBuf,
    token_path: PathBuf,
    nodes: Vec<PersistedNodeState>,
}

impl PersistedClusterState {
    const FORMAT_VERSION: u32 = 1;

    fn from_state(state: &ClusterState) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            name: state.name.clone(),
            backend: state.backend.as_str().to_owned(),
            workspace_root: state.workspace_root.clone(),
            template: state.template.clone(),
            firecracker_kernel: state.firecracker_kernel.clone(),
            firecracker_rootfs: state.firecracker_rootfs.clone(),
            api_port: state.api_port,
            worker_count: state.worker_count,
            control_plane_resources: state.control_plane_resources.into(),
            worker_resources: state.worker_resources.into(),
            enable_rosetta: state.enable_rosetta,
            network: state.network.clone(),
            k3s_version: state.k3s_version.clone(),
            cni_plugin: state.cni_plugin.as_str().to_owned(),
            cilium_version: state.cilium_version.clone(),
            addons: state.addons.clone(),
            metallb: PersistedMetalLbConfig::from_state(&state.metallb),
            server_mode: server_mode_label(state.server_mode).to_owned(),
            component_toggles: state.component_toggles.into(),
            kubeconfig_path: state.kubeconfig_path.clone(),
            token_path: state.token_path.clone(),
            nodes: state
                .nodes
                .iter()
                .map(PersistedNodeState::from_state)
                .collect(),
        }
    }

    fn try_into_state(self) -> Result<ClusterState> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(Error::InvalidArgs(format!(
                "unsupported cluster state format version {}",
                self.format_version
            )));
        }

        Ok(ClusterState {
            name: self.name,
            backend: parse_backend_kind(&self.backend)?,
            workspace_root: self.workspace_root,
            template: self.template,
            firecracker_kernel: self.firecracker_kernel,
            firecracker_rootfs: self.firecracker_rootfs,
            api_port: self.api_port,
            worker_count: self.worker_count,
            control_plane_resources: self.control_plane_resources.into(),
            worker_resources: self.worker_resources.into(),
            enable_rosetta: self.enable_rosetta,
            network: self.network,
            k3s_version: self.k3s_version,
            cni_plugin: parse_cni_plugin(&self.cni_plugin)?,
            cilium_version: self.cilium_version,
            addons: self.addons,
            metallb: self.metallb.try_into_state()?,
            server_mode: parse_server_mode(&self.server_mode)?,
            component_toggles: self.component_toggles.into(),
            kubeconfig_path: self.kubeconfig_path,
            token_path: self.token_path,
            nodes: self
                .nodes
                .into_iter()
                .map(PersistedNodeState::try_into_state)
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

fn parse_backend_kind(value: &str) -> Result<BackendKind> {
    BackendKind::parse(value).ok_or_else(|| {
        Error::InvalidArgs(format!(
            "invalid cluster state: unsupported backend `{value}`"
        ))
    })
}

fn parse_cni_plugin(value: &str) -> Result<CniPlugin> {
    CniPlugin::parse(value).ok_or_else(|| {
        Error::InvalidArgs(format!(
            "invalid cluster state: unsupported cni plugin `{value}`"
        ))
    })
}

fn parse_server_mode(value: &str) -> Result<ServerMode> {
    match value {
        "standard" => Ok(ServerMode::Standard),
        "agentless" => Ok(ServerMode::Agentless),
        _ => Err(Error::InvalidArgs(format!(
            "invalid cluster state: unsupported server mode `{value}`"
        ))),
    }
}

fn parse_metallb_mode(value: &str) -> Result<MetalLbMode> {
    MetalLbMode::parse(value).ok_or_else(|| {
        Error::InvalidArgs(format!(
            "invalid cluster state: unsupported metallb mode `{value}`"
        ))
    })
}

fn parse_node_role(value: &str) -> Result<NodeRole> {
    match value {
        "control-plane" => Ok(NodeRole::ControlPlane),
        "worker" => Ok(NodeRole::Worker),
        _ => Err(Error::InvalidArgs(format!(
            "invalid cluster state: unsupported node role `{value}`"
        ))),
    }
}

fn server_mode_label(value: ServerMode) -> &'static str {
    match value {
        ServerMode::Standard => "standard",
        ServerMode::Agentless => "agentless",
    }
}

fn node_role_label(value: NodeRole) -> &'static str {
    match value {
        NodeRole::ControlPlane => "control-plane",
        NodeRole::Worker => "worker",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::backend::BackendKind;
    use crate::cluster::addon::{CniPlugin, DEFAULT_CILIUM_VERSION, MetalLbConfig};
    use crate::cluster::spec::NodeResources;
    use crate::k3s::{ComponentToggles, NodeRole, ServerMode};

    use super::{ClusterState, NodeState};

    #[test]
    fn state_roundtrips() {
        let root = std::env::temp_dir().join(format!("kalisto-state-{}", std::process::id()));
        let mut component_toggles = ComponentToggles::default();
        CniPlugin::Flannel.apply_component_defaults(&mut component_toggles);

        let state = ClusterState {
            name: "dev".to_owned(),
            backend: BackendKind::Lima,
            workspace_root: std::path::PathBuf::from("/tmp/work"),
            template: "/tmp/template.yaml".to_owned(),
            firecracker_kernel: Some(std::path::PathBuf::from("/tmp/vmlinux")),
            firecracker_rootfs: Some(std::path::PathBuf::from("/tmp/rootfs.ext4")),
            api_port: 16443,
            worker_count: 1,
            control_plane_resources: NodeResources {
                cpus: 8,
                memory_gib: 16,
            },
            worker_resources: NodeResources {
                cpus: 4,
                memory_gib: 12,
            },
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            k3s_version: "v1.33.5+k3s1".to_owned(),
            cni_plugin: CniPlugin::Flannel,
            cilium_version: DEFAULT_CILIUM_VERSION.to_owned(),
            addons: vec!["lb/metallb".to_owned()],
            metallb: MetalLbConfig {
                mode: None,
                addresses: vec!["10.0.0.10-10.0.0.20".to_owned()],
                config_file: None,
            },
            server_mode: ServerMode::Standard,
            component_toggles,
            kubeconfig_path: std::path::PathBuf::from("/tmp/kubeconfig.yaml"),
            token_path: std::path::PathBuf::from("/tmp/token"),
            nodes: vec![
                NodeState {
                    name: "cp".to_owned(),
                    role: NodeRole::ControlPlane,
                    artifacts_root: std::path::PathBuf::from("/tmp/cp"),
                },
                NodeState {
                    name: "w0".to_owned(),
                    role: NodeRole::Worker,
                    artifacts_root: std::path::PathBuf::from("/tmp/w0"),
                },
            ],
        };

        state.save(&root).unwrap();
        let saved = fs::read_to_string(ClusterState::state_file(&root)).unwrap();
        assert!(saved.trim_start().starts_with('{'));
        assert!(saved.contains("\"formatVersion\": 1"));
        assert!(saved.contains("\"backend\": \"lima\""));
        let loaded = ClusterState::load(&root).unwrap();
        assert_eq!(loaded.name, "dev");
        assert_eq!(loaded.backend, BackendKind::Lima);
        assert_eq!(
            loaded.firecracker_kernel,
            Some(std::path::PathBuf::from("/tmp/vmlinux"))
        );
        assert_eq!(
            loaded.firecracker_rootfs,
            Some(std::path::PathBuf::from("/tmp/rootfs.ext4"))
        );
        assert_eq!(loaded.worker_count, 1);
        let mut expected_toggles = ComponentToggles::default();
        CniPlugin::Flannel.apply_component_defaults(&mut expected_toggles);
        assert_eq!(loaded.component_toggles, expected_toggles);
        assert_eq!(loaded.cni_plugin, CniPlugin::Flannel);
        assert_eq!(loaded.cilium_version, DEFAULT_CILIUM_VERSION);
        assert_eq!(loaded.addons, vec!["lb/metallb".to_owned()]);
        assert_eq!(
            loaded.metallb.addresses,
            vec!["10.0.0.10-10.0.0.20".to_owned()]
        );
        assert_eq!(
            loaded.control_plane_resources,
            NodeResources {
                cpus: 8,
                memory_gib: 16,
            }
        );
        assert_eq!(
            loaded.worker_resources,
            NodeResources {
                cpus: 4,
                memory_gib: 12,
            }
        );
        assert_eq!(loaded.nodes.len(), 2);
        assert_eq!(loaded.nodes[1].name, "w0");

        let _ = fs::remove_dir_all(root);
    }
}
