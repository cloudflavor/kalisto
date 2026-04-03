use std::fs;
use std::path::{Path, PathBuf};

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
    pub fn state_file(root: &Path) -> PathBuf {
        root.join("cluster.state")
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        fs::create_dir_all(root).map_err(|source| Error::CommandIo {
            program: "create_dir_all".to_owned(),
            source,
        })?;

        let mut lines = vec![
            format!("name\t{}", escape(&self.name)),
            format!("backend\t{}", self.backend.as_str()),
            format!(
                "workspace_root\t{}",
                escape(&self.workspace_root.display().to_string())
            ),
            format!("template\t{}", escape(&self.template)),
            format!(
                "firecracker_kernel\t{}",
                escape(
                    &self
                        .firecracker_kernel
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                )
            ),
            format!(
                "firecracker_rootfs\t{}",
                escape(
                    &self
                        .firecracker_rootfs
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                )
            ),
            format!("api_port\t{}", self.api_port),
            format!("worker_count\t{}", self.worker_count),
            format!("control_plane_cpus\t{}", self.control_plane_resources.cpus),
            format!(
                "control_plane_memory_gib\t{}",
                self.control_plane_resources.memory_gib
            ),
            format!("worker_cpus\t{}", self.worker_resources.cpus),
            format!("worker_memory_gib\t{}", self.worker_resources.memory_gib),
            format!("enable_rosetta\t{}", self.enable_rosetta),
            format!("network\t{}", escape(&self.network)),
            format!("k3s_version\t{}", escape(&self.k3s_version)),
            format!("cni_plugin\t{}", self.cni_plugin.as_str()),
            format!("cilium_version\t{}", escape(&self.cilium_version)),
            format!(
                "metallb_mode\t{}",
                self.metallb
                    .mode
                    .map(MetalLbMode::as_str)
                    .unwrap_or_default()
            ),
            format!(
                "metallb_config_file\t{}",
                escape(
                    &self
                        .metallb
                        .config_file
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                )
            ),
            format!(
                "server_mode\t{}",
                match self.server_mode {
                    ServerMode::Standard => "standard",
                    ServerMode::Agentless => "agentless",
                }
            ),
            format!(
                "disable_traefik\t{}",
                self.component_toggles.disable_traefik
            ),
            format!(
                "disable_servicelb\t{}",
                self.component_toggles.disable_servicelb
            ),
            format!(
                "disable_metrics_server\t{}",
                self.component_toggles.disable_metrics_server
            ),
            format!(
                "disable_local_storage\t{}",
                self.component_toggles.disable_local_storage
            ),
            format!(
                "disable_coredns\t{}",
                self.component_toggles.disable_coredns
            ),
            format!(
                "disable_network_policy\t{}",
                self.component_toggles.disable_network_policy
            ),
            format!(
                "disable_kube_proxy\t{}",
                self.component_toggles.disable_kube_proxy
            ),
            format!(
                "flannel_backend_none\t{}",
                self.component_toggles.flannel_backend_none
            ),
            format!(
                "kubeconfig_path\t{}",
                escape(&self.kubeconfig_path.display().to_string())
            ),
            format!(
                "token_path\t{}",
                escape(&self.token_path.display().to_string())
            ),
        ];

        for node in &self.nodes {
            lines.push(format!(
                "node\t{}\t{}\t{}",
                escape(&node.name),
                match node.role {
                    NodeRole::ControlPlane => "control-plane",
                    NodeRole::Worker => "worker",
                },
                escape(&node.artifacts_root.display().to_string())
            ));
        }

        for addon in &self.addons {
            lines.push(format!("addon\t{}", escape(addon)));
        }
        for address in &self.metallb.addresses {
            lines.push(format!("metallb_address\t{}", escape(address)));
        }

        fs::write(Self::state_file(root), lines.join("\n")).map_err(|source| Error::CommandIo {
            program: "write".to_owned(),
            source,
        })
    }

    pub fn load(root: &Path) -> Result<Self> {
        let content =
            fs::read_to_string(Self::state_file(root)).map_err(|source| Error::CommandIo {
                program: "read_to_string".to_owned(),
                source,
            })?;

        let mut name = None;
        let mut backend = None;
        let mut workspace_root = None;
        let mut template = None;
        let mut firecracker_kernel = None;
        let mut firecracker_rootfs = None;
        let mut api_port = None;
        let mut worker_count = None;
        let mut control_plane_cpus = None;
        let mut control_plane_memory_gib = None;
        let mut worker_cpus = None;
        let mut worker_memory_gib = None;
        let mut enable_rosetta = None;
        let mut network = None;
        let mut k3s_version = None;
        let mut cni_plugin = None;
        let mut cilium_version = None;
        let mut addons = Vec::new();
        let mut metallb_mode = None;
        let mut metallb_config_file = None;
        let mut metallb_addresses = Vec::new();
        let mut server_mode = None;
        let mut disable_traefik = None;
        let mut disable_servicelb = None;
        let mut disable_metrics_server = None;
        let mut disable_local_storage = None;
        let mut disable_coredns = None;
        let mut disable_network_policy = None;
        let mut disable_kube_proxy = None;
        let mut flannel_backend_none = None;
        let mut kubeconfig_path = None;
        let mut token_path = None;
        let mut nodes = Vec::new();

        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            let parts: Vec<&str> = line.split('\t').collect();
            match parts.first().copied() {
                Some("name") => name = parts.get(1).map(|value| unescape(value)),
                Some("backend") => {
                    backend = parts
                        .get(1)
                        .and_then(|value| BackendKind::parse(&unescape(value)))
                }
                Some("workspace_root") => {
                    workspace_root = parts.get(1).map(|value| PathBuf::from(unescape(value)))
                }
                Some("template") => template = parts.get(1).map(|value| unescape(value)),
                Some("firecracker_kernel") => {
                    firecracker_kernel = parts
                        .get(1)
                        .map(|value| unescape(value))
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                }
                Some("firecracker_rootfs") => {
                    firecracker_rootfs = parts
                        .get(1)
                        .map(|value| unescape(value))
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                }
                Some("api_port") => {
                    api_port = parts.get(1).and_then(|value| value.parse::<u16>().ok())
                }
                Some("worker_count") => {
                    worker_count = parts.get(1).and_then(|value| value.parse::<usize>().ok())
                }
                Some("control_plane_cpus") => {
                    control_plane_cpus = parts.get(1).and_then(|value| value.parse::<u8>().ok())
                }
                Some("control_plane_memory_gib") => {
                    control_plane_memory_gib =
                        parts.get(1).and_then(|value| value.parse::<u8>().ok())
                }
                Some("worker_cpus") => {
                    worker_cpus = parts.get(1).and_then(|value| value.parse::<u8>().ok())
                }
                Some("worker_memory_gib") => {
                    worker_memory_gib = parts.get(1).and_then(|value| value.parse::<u8>().ok())
                }
                Some("enable_rosetta") => {
                    enable_rosetta = parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("network") => network = parts.get(1).map(|value| unescape(value)),
                Some("k3s_version") => k3s_version = parts.get(1).map(|value| unescape(value)),
                Some("cni_plugin") => {
                    cni_plugin = parts
                        .get(1)
                        .and_then(|value| CniPlugin::parse(&unescape(value)))
                }
                Some("cilium_version") => {
                    cilium_version = parts.get(1).map(|value| unescape(value))
                }
                Some("addon") => {
                    if let Some(value) = parts.get(1) {
                        addons.push(unescape(value));
                    }
                }
                Some("metallb_mode") => {
                    if let Some(value) = parts.get(1).map(|value| unescape(value))
                        && !value.trim().is_empty()
                    {
                        metallb_mode = Some(MetalLbMode::parse(&value).ok_or_else(|| {
                            Error::InvalidArgs(format!(
                                "invalid cluster state: unsupported metallb_mode `{value}`"
                            ))
                        })?);
                    }
                }
                Some("metallb_config_file") => {
                    metallb_config_file = parts
                        .get(1)
                        .map(|value| unescape(value))
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                }
                Some("metallb_address") => {
                    if let Some(value) = parts.get(1) {
                        metallb_addresses.push(unescape(value));
                    }
                }
                Some("server_mode") => {
                    server_mode = match parts.get(1).copied() {
                        Some("standard") => Some(ServerMode::Standard),
                        Some("agentless") => Some(ServerMode::Agentless),
                        _ => None,
                    }
                }
                Some("disable_traefik") => {
                    disable_traefik = parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("disable_servicelb") => {
                    disable_servicelb = parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("disable_metrics_server") => {
                    disable_metrics_server =
                        parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("disable_local_storage") => {
                    disable_local_storage =
                        parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("disable_coredns") => {
                    disable_coredns = parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("disable_network_policy") => {
                    disable_network_policy =
                        parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("disable_kube_proxy") => {
                    disable_kube_proxy = parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("flannel_backend_none") => {
                    flannel_backend_none = parts.get(1).and_then(|value| value.parse::<bool>().ok())
                }
                Some("kubeconfig_path") => {
                    kubeconfig_path = parts.get(1).map(|value| PathBuf::from(unescape(value)))
                }
                Some("token_path") => {
                    token_path = parts.get(1).map(|value| PathBuf::from(unescape(value)))
                }
                Some("node") if parts.len() == 4 => {
                    let role = match parts[2] {
                        "control-plane" => Some(NodeRole::ControlPlane),
                        "worker" => Some(NodeRole::Worker),
                        _ => None,
                    };
                    if let Some(role) = role {
                        nodes.push(NodeState {
                            name: unescape(parts[1]),
                            role,
                            artifacts_root: PathBuf::from(unescape(parts[3])),
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(Self {
            name: required(name, "name")?,
            backend: required(backend, "backend")?,
            workspace_root: required(workspace_root, "workspace_root")?,
            template: required(template, "template")?,
            firecracker_kernel,
            firecracker_rootfs,
            api_port: required(api_port, "api_port")?,
            worker_count: required(worker_count, "worker_count")?,
            control_plane_resources: NodeResources {
                cpus: required(control_plane_cpus, "control_plane_cpus")?,
                memory_gib: required(control_plane_memory_gib, "control_plane_memory_gib")?,
            },
            worker_resources: NodeResources {
                cpus: required(worker_cpus, "worker_cpus")?,
                memory_gib: required(worker_memory_gib, "worker_memory_gib")?,
            },
            enable_rosetta: required(enable_rosetta, "enable_rosetta")?,
            network: required(network, "network")?,
            k3s_version: required(k3s_version, "k3s_version")?,
            cni_plugin: required(cni_plugin, "cni_plugin")?,
            cilium_version: required(cilium_version, "cilium_version")?,
            addons,
            metallb: MetalLbConfig {
                mode: metallb_mode,
                addresses: metallb_addresses,
                config_file: metallb_config_file,
            },
            server_mode: required(server_mode, "server_mode")?,
            component_toggles: ComponentToggles {
                disable_traefik: required(disable_traefik, "disable_traefik")?,
                disable_servicelb: required(disable_servicelb, "disable_servicelb")?,
                disable_metrics_server: required(disable_metrics_server, "disable_metrics_server")?,
                disable_local_storage: required(disable_local_storage, "disable_local_storage")?,
                disable_coredns: required(disable_coredns, "disable_coredns")?,
                disable_network_policy: required(disable_network_policy, "disable_network_policy")?,
                disable_kube_proxy: required(disable_kube_proxy, "disable_kube_proxy")?,
                flannel_backend_none: required(flannel_backend_none, "flannel_backend_none")?,
            },
            kubeconfig_path: required(kubeconfig_path, "kubeconfig_path")?,
            token_path: required(token_path, "token_path")?,
            nodes,
        })
    }
}

fn required<T>(value: Option<T>, field: &str) -> Result<T> {
    value.ok_or_else(|| Error::InvalidArgs(format!("invalid cluster state: missing {field}")))
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('t') => output.push('\t'),
                Some('n') => output.push('\n'),
                Some('\\') => output.push('\\'),
                Some(other) => {
                    output.push('\\');
                    output.push(other);
                }
                None => output.push('\\'),
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::backend::BackendKind;
    use crate::cluster::addon::{CniPlugin, DEFAULT_CILIUM_VERSION, MetalLbConfig};
    use crate::cluster::spec::NodeResources;
    use crate::error::Error;
    use crate::k3s::{ComponentToggles, NodeRole, ServerMode};

    use super::{ClusterState, NodeState};

    #[test]
    fn state_roundtrips() {
        let root = std::env::temp_dir().join(format!("kalisto-state-{}", std::process::id()));
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
            component_toggles: ComponentToggles::default(),
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
        assert_eq!(loaded.component_toggles, ComponentToggles::default());
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

    #[test]
    fn missing_required_fields_fail_fast() {
        let root =
            std::env::temp_dir().join(format!("kalisto-state-legacy-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            ClusterState::state_file(&root),
            [
                "name\tdev",
                "workspace_root\t/tmp/work",
                "template\ttemplate://default",
                "api_port\t16443",
                "worker_count\t0",
                "enable_rosetta\ttrue",
                "network\tlima:user-v2",
                "k3s_version\tv1.33.5+k3s1",
                "cni_plugin\tflannel",
                "cilium_version\t1.19.2",
                "server_mode\tstandard",
                "disable_traefik\ttrue",
                "disable_servicelb\ttrue",
                "disable_metrics_server\ttrue",
                "disable_local_storage\ttrue",
                "disable_coredns\ttrue",
                "disable_network_policy\ttrue",
                "disable_kube_proxy\tfalse",
                "flannel_backend_none\tfalse",
                "kubeconfig_path\t/tmp/kubeconfig.yaml",
                "token_path\t/tmp/token",
            ]
            .join("\n"),
        )
        .unwrap();

        let error = ClusterState::load(&root).unwrap_err();
        match error {
            Error::InvalidArgs(message) => assert!(message.contains("missing backend")),
            other => panic!("unexpected error: {other}"),
        }

        fs::write(
            ClusterState::state_file(&root),
            [
                "name\tdev",
                "backend\tlima",
                "workspace_root\t/tmp/work",
                "template\ttemplate://default",
                "api_port\t16443",
                "worker_count\t0",
                "enable_rosetta\ttrue",
                "network\tlima:user-v2",
                "k3s_version\tv1.33.5+k3s1",
                "cni_plugin\tflannel",
                "cilium_version\t1.19.2",
                "server_mode\tstandard",
                "disable_traefik\ttrue",
                "disable_servicelb\ttrue",
                "disable_metrics_server\ttrue",
                "disable_local_storage\ttrue",
                "disable_coredns\ttrue",
                "disable_network_policy\ttrue",
                "disable_kube_proxy\tfalse",
                "flannel_backend_none\tfalse",
                "kubeconfig_path\t/tmp/kubeconfig.yaml",
                "token_path\t/tmp/token",
            ]
            .join("\n"),
        )
        .unwrap();

        let error = ClusterState::load(&root).unwrap_err();
        match error {
            Error::InvalidArgs(message) => {
                assert!(message.contains("missing control_plane_cpus"))
            }
            other => panic!("unexpected error: {other}"),
        }

        let _ = fs::remove_dir_all(root);
    }
}
