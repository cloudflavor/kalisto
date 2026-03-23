use std::path::{Path, PathBuf};

use crate::backend::BackendKind;
use crate::cluster::addon::{CniPlugin, DEFAULT_CILIUM_VERSION};
use crate::k3s::{ComponentToggles, ServerMode};
use crate::paths::{config_root, data_root};

pub const DEFAULT_K3S_VERSION: &str = "v1.33.5+k3s1";
pub const DEFAULT_CONTROL_PLANE_CPUS: u8 = 4;
pub const DEFAULT_CONTROL_PLANE_MEMORY_GIB: u8 = 6;
pub const DEFAULT_WORKER_CPUS: u8 = 2;
pub const DEFAULT_WORKER_MEMORY_GIB: u8 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeResources {
    pub cpus: u8,
    pub memory_gib: u8,
}

#[derive(Debug, Clone)]
pub struct ClusterSpec {
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
    pub server_mode: ServerMode,
    pub component_toggles: ComponentToggles,
    pub keep_failed_runtime: bool,
    pub stream_vm_logs: bool,
}

impl ClusterSpec {
    pub fn new(name: String, workspace_root: PathBuf) -> Self {
        let backend = BackendKind::host_default();
        let cni_plugin = CniPlugin::None;
        let mut component_toggles = ComponentToggles::default();
        cni_plugin.apply_component_defaults(&mut component_toggles);
        Self {
            backend,
            template: default_template(&workspace_root),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            api_port: 16443,
            worker_count: 1,
            control_plane_resources: NodeResources {
                cpus: DEFAULT_CONTROL_PLANE_CPUS,
                memory_gib: DEFAULT_CONTROL_PLANE_MEMORY_GIB,
            },
            worker_resources: NodeResources {
                cpus: DEFAULT_WORKER_CPUS,
                memory_gib: DEFAULT_WORKER_MEMORY_GIB,
            },
            enable_rosetta: true,
            network: default_network(backend),
            k3s_version: DEFAULT_K3S_VERSION.to_owned(),
            cni_plugin,
            cilium_version: DEFAULT_CILIUM_VERSION.to_owned(),
            addons: Vec::new(),
            server_mode: ServerMode::Standard,
            component_toggles,
            keep_failed_runtime: false,
            stream_vm_logs: false,
            name,
            workspace_root,
        }
    }

    pub fn default_name(workspace_root: &Path) -> String {
        workspace_root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "kalisto".to_owned())
    }

    pub fn state_root(&self) -> PathBuf {
        Self::state_root_for(&self.name).expect("data root should be resolvable")
    }

    pub fn clusters_root(&self) -> PathBuf {
        Self::clusters_root_for().expect("data root should be resolvable")
    }

    pub fn helper_script(&self) -> PathBuf {
        Self::helper_script_for(&self.workspace_root)
    }

    pub fn runtime_root(&self) -> PathBuf {
        Self::runtime_root_for(&self.workspace_root, &self.name)
    }

    pub fn control_plane_name(&self) -> String {
        format!("kalisto-{}-cp", self.name)
    }

    pub fn state_root_for(name: &str) -> crate::error::Result<PathBuf> {
        Ok(Self::clusters_root_for()?.join(name))
    }

    pub fn clusters_root_for() -> crate::error::Result<PathBuf> {
        Ok(data_root()?.join("clusters"))
    }

    pub fn helper_script_for(workspace_root: &Path) -> PathBuf {
        workspace_root
            .join("scripts")
            .join("macos")
            .join("k3s_server_probe.sh")
    }

    pub fn runtime_root_for(workspace_root: &Path, name: &str) -> PathBuf {
        workspace_root
            .join("target")
            .join("kalisto")
            .join("clusters")
            .join(name)
    }
}

fn default_network(backend: BackendKind) -> String {
    match backend {
        BackendKind::Firecracker => String::new(),
        BackendKind::Lima => "lima:user-v2".to_owned(),
    }
}

pub fn default_template(workspace_root: &Path) -> String {
    if let Ok(config_root) = config_root() {
        let configured = config_root
            .join("lima-node")
            .join("ubuntu-24.04-minimal-k3s-arm64-finalized.yaml");
        if configured.is_file() {
            return configured.display().to_string();
        }

        let configured_seed = config_root
            .join("lima-node")
            .join("ubuntu-24.04-minimal-k3s-arm64.yaml");
        if configured_seed.is_file() {
            return configured_seed.display().to_string();
        }
    }

    let finalized = workspace_root
        .join("artifacts")
        .join("lima-node")
        .join("ubuntu-24.04-minimal-k3s-arm64-finalized.yaml");
    if finalized.is_file() {
        return finalized.display().to_string();
    }

    let seeded = workspace_root
        .join("artifacts")
        .join("lima-node")
        .join("ubuntu-24.04-minimal-k3s-arm64.yaml");
    if seeded.is_file() {
        return seeded.display().to_string();
    }

    "template://default".to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::backend::BackendKind;

    use crate::cluster::addon::{CniPlugin, DEFAULT_CILIUM_VERSION};

    use super::{
        ClusterSpec, DEFAULT_CONTROL_PLANE_CPUS, DEFAULT_CONTROL_PLANE_MEMORY_GIB,
        DEFAULT_WORKER_CPUS, DEFAULT_WORKER_MEMORY_GIB, NodeResources, default_template,
    };

    #[test]
    fn default_name_uses_workspace_basename() {
        assert_eq!(
            ClusterSpec::default_name(std::path::Path::new("/tmp/my-workspace")),
            "my-workspace"
        );
    }

    #[test]
    fn default_template_prefers_finalized_artifact() {
        let root = std::env::temp_dir().join(format!("kalisto-spec-{}", std::process::id()));
        let finalized = root
            .join("artifacts")
            .join("lima-node")
            .join("ubuntu-24.04-minimal-k3s-arm64-finalized.yaml");
        fs::create_dir_all(finalized.parent().unwrap()).unwrap();
        fs::write(&finalized, "vmType: vz\n").unwrap();

        assert_eq!(default_template(&root), finalized.display().to_string());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_cluster_shape_is_single_worker_multinode() {
        let spec = ClusterSpec::new(
            "kalisto".to_owned(),
            std::path::PathBuf::from("/tmp/kalisto-workspace"),
        );

        assert_eq!(spec.backend, BackendKind::host_default());
        assert_eq!(spec.worker_count, 1);
        assert_eq!(spec.cni_plugin, CniPlugin::None);
        assert!(spec.component_toggles.flannel_backend_none);
        assert_eq!(spec.cilium_version, DEFAULT_CILIUM_VERSION);
        assert!(spec.addons.is_empty());
        assert_eq!(
            spec.control_plane_resources,
            NodeResources {
                cpus: DEFAULT_CONTROL_PLANE_CPUS,
                memory_gib: DEFAULT_CONTROL_PLANE_MEMORY_GIB,
            }
        );
        assert_eq!(
            spec.worker_resources,
            NodeResources {
                cpus: DEFAULT_WORKER_CPUS,
                memory_gib: DEFAULT_WORKER_MEMORY_GIB,
            }
        );
        assert!(!spec.keep_failed_runtime);
        assert!(!spec.stream_vm_logs);
        if cfg!(target_os = "linux") {
            assert!(spec.network.is_empty());
        } else {
            assert_eq!(spec.network, "lima:user-v2");
        }
    }
}
