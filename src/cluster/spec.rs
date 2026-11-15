use std::path::{Path, PathBuf};

use crate::backend::BackendKind;
use crate::cluster::addon::{CniPlugin, DEFAULT_CILIUM_VERSION, MetalLbConfig};
use crate::k3s::{ComponentToggles, ServerMode};
use crate::paths::{artifacts_root, config_root, data_root};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageArch {
    Aarch64,
    X86_64,
}

impl ImageArch {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "arm64" | "aarch64" => Some(Self::Aarch64),
            "amd64" | "x86_64" => Some(Self::X86_64),
            _ => None,
        }
    }

    #[must_use]
    pub fn host_default() -> Self {
        match std::env::consts::ARCH {
            "x86_64" | "amd64" => Self::X86_64,
            _ => Self::Aarch64,
        }
    }

    #[must_use]
    pub fn lima_template_value(self) -> &'static str {
        match self {
            Self::Aarch64 => "aarch64",
            Self::X86_64 => "x86_64",
        }
    }

    #[must_use]
    pub fn lima_artifact_suffix(self) -> &'static str {
        match self {
            Self::Aarch64 => "arm64",
            Self::X86_64 => "amd64",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClusterSpec {
    pub name: String,
    pub backend: BackendKind,
    pub workspace_root: PathBuf,
    pub template: String,
    pub image_arch: Option<ImageArch>,
    pub firecracker_kernel: Option<PathBuf>,
    pub firecracker_rootfs: Option<PathBuf>,
    pub api_port: u16,
    pub worker_count: usize,
    pub requested_worker_count: Option<usize>,
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
    pub keep_failed_runtime: bool,
    pub stream_vm_logs: bool,
}

impl ClusterSpec {
    #[must_use]
    pub fn new(name: String, workspace_root: PathBuf) -> Self {
        let backend = BackendKind::host_default();
        let cni_plugin = CniPlugin::None;
        let mut component_toggles = ComponentToggles::default();
        cni_plugin.apply_component_defaults(&mut component_toggles);
        Self {
            backend,
            template: default_template(&workspace_root),
            image_arch: None,
            firecracker_kernel: None,
            firecracker_rootfs: None,
            api_port: 16443,
            worker_count: 0,
            requested_worker_count: None,
            control_plane_resources: NodeResources {
                cpus: DEFAULT_CONTROL_PLANE_CPUS,
                memory_gib: DEFAULT_CONTROL_PLANE_MEMORY_GIB,
            },
            worker_resources: NodeResources {
                cpus: DEFAULT_WORKER_CPUS,
                memory_gib: DEFAULT_WORKER_MEMORY_GIB,
            },
            enable_rosetta: false,
            network: default_network(backend),
            k3s_version: DEFAULT_K3S_VERSION.to_owned(),
            cni_plugin,
            cilium_version: DEFAULT_CILIUM_VERSION.to_owned(),
            addons: Vec::new(),
            metallb: MetalLbConfig::default(),
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
            .map_or_else(|| "kalisto".to_owned(), ToOwned::to_owned)
    }

    /// # Panics
    ///
    /// Panics when the user-scoped Kalisto data root cannot be resolved.
    #[must_use]
    pub fn state_root(&self) -> PathBuf {
        Self::state_root_for(&self.name).expect("data root should be resolvable")
    }

    /// # Panics
    ///
    /// Panics when the user-scoped Kalisto data root cannot be resolved.
    #[must_use]
    pub fn clusters_root(&self) -> PathBuf {
        Self::clusters_root_for().expect("data root should be resolvable")
    }

    #[must_use]
    pub fn helper_script(&self) -> PathBuf {
        Self::helper_script_for(&self.workspace_root)
    }

    #[must_use]
    pub fn runtime_root(&self) -> PathBuf {
        Self::runtime_root_for(&self.workspace_root, &self.name)
    }

    #[must_use]
    pub fn control_plane_name(&self) -> String {
        format!("kalisto-{}-cp", self.name)
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn state_root_for(name: &str) -> crate::error::Result<PathBuf> {
        Ok(Self::clusters_root_for()?.join(name))
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn clusters_root_for() -> crate::error::Result<PathBuf> {
        Ok(data_root()?.join("clusters"))
    }

    #[must_use]
    pub fn helper_script_for(workspace_root: &Path) -> PathBuf {
        workspace_root
            .join("scripts")
            .join("macos")
            .join("k3s_server_probe.sh")
    }

    /// # Panics
    ///
    /// Panics when the user-scoped Kalisto data root cannot be resolved.
    #[must_use]
    pub fn runtime_root_for(_workspace_root: &Path, name: &str) -> PathBuf {
        Self::state_root_for(name)
            .expect("data root should be resolvable")
            .join("runtime")
    }
}

fn default_network(backend: BackendKind) -> String {
    match backend {
        BackendKind::Firecracker => "slirp4netns".to_owned(),
        BackendKind::Lima => "lima:user-v2".to_owned(),
    }
}

fn default_lima_image_arch_suffix() -> &'static str {
    ImageArch::host_default().lima_artifact_suffix()
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn parse_image_arch(value: &str) -> std::result::Result<ImageArch, String> {
    ImageArch::parse(value).ok_or_else(|| {
        format!("unsupported arch `{value}`; expected one of: amd64, x86_64, arm64, aarch64")
    })
}

fn default_lima_finalized_template_filename() -> String {
    format!(
        "ubuntu-24.04-minimal-k3s-{}-finalized.yaml",
        default_lima_image_arch_suffix()
    )
}

fn default_lima_seed_template_filename() -> String {
    format!(
        "ubuntu-24.04-minimal-k3s-{}.yaml",
        default_lima_image_arch_suffix()
    )
}

#[must_use]
pub fn default_template(workspace_root: &Path) -> String {
    if let Some(template) = lima_template_under(artifacts_root().ok().as_deref()) {
        return template;
    }

    if let Some(template) = lima_template_under_workspace_ancestors(workspace_root) {
        return template;
    }

    if let Ok(config_root) = config_root()
        && let Some(template) = lima_template_under(Some(&config_root))
    {
        return template;
    }

    "template://default".to_owned()
}

fn lima_template_under_workspace_ancestors(workspace_root: &Path) -> Option<String> {
    for root in workspace_root.ancestors() {
        if let Some(template) = lima_template_under(Some(&root.join("artifacts"))) {
            return Some(template);
        }
    }
    None
}

fn lima_template_under(root: Option<&Path>) -> Option<String> {
    let root = root?;
    let finalized = root
        .join("lima-node")
        .join(default_lima_finalized_template_filename());
    if finalized.is_file() {
        return Some(finalized.display().to_string());
    }

    let seeded = root
        .join("lima-node")
        .join(default_lima_seed_template_filename());
    if seeded.is_file() {
        return Some(seeded.display().to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::backend::BackendKind;
    use crate::test_support::env_lock;

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
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root = std::env::temp_dir().join(format!("kalisto-spec-{}", std::process::id()));
        let data_home = root.join("xdg-data");
        let finalized = root
            .join("artifacts")
            .join("lima-node")
            .join(super::default_lima_finalized_template_filename());
        fs::create_dir_all(finalized.parent().unwrap()).unwrap();
        fs::write(&finalized, "vmType: vz\n").unwrap();

        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }

        assert_eq!(default_template(&root), finalized.display().to_string());

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_template_prefers_xdg_artifact_root_over_workspace() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root = std::env::temp_dir().join(format!("kalisto-spec-xdg-{}", std::process::id()));
        let data_home = root.join("xdg-data");
        let workspace_root = root.join("workspace");
        let workspace_template = workspace_root
            .join("artifacts")
            .join("lima-node")
            .join(super::default_lima_finalized_template_filename());
        let xdg_template = data_home
            .join("kalisto")
            .join("artifacts")
            .join("lima-node")
            .join(super::default_lima_finalized_template_filename());
        fs::create_dir_all(workspace_template.parent().unwrap()).unwrap();
        fs::create_dir_all(xdg_template.parent().unwrap()).unwrap();
        fs::write(&workspace_template, "workspace\n").unwrap();
        fs::write(&xdg_template, "xdg\n").unwrap();

        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }

        assert_eq!(
            default_template(&workspace_root),
            xdg_template.display().to_string()
        );

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_template_finds_artifacts_in_workspace_ancestors() {
        let _guard = env_lock();
        let original_xdg_data_home = std::env::var_os("XDG_DATA_HOME");
        let root =
            std::env::temp_dir().join(format!("kalisto-spec-ancestor-{}", std::process::id()));
        let data_home = root.join("xdg-data");
        let workspace_root = root.join("examples").join("harness").join("job-smoke");
        let ancestor_template = root
            .join("artifacts")
            .join("lima-node")
            .join(super::default_lima_finalized_template_filename());
        fs::create_dir_all(workspace_root.as_path()).unwrap();
        fs::create_dir_all(ancestor_template.parent().unwrap()).unwrap();
        fs::write(&ancestor_template, "ancestor\n").unwrap();

        unsafe {
            std::env::set_var("XDG_DATA_HOME", &data_home);
        }

        assert_eq!(
            default_template(&workspace_root),
            ancestor_template.display().to_string()
        );

        restore_env("XDG_DATA_HOME", original_xdg_data_home);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_cluster_shape_is_control_plane_only() {
        let spec = ClusterSpec::new(
            "kalisto".to_owned(),
            std::path::PathBuf::from("/tmp/kalisto-workspace"),
        );

        assert_eq!(spec.backend, BackendKind::host_default());
        assert_eq!(spec.worker_count, 0);
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
            assert_eq!(spec.network, "slirp4netns");
        } else {
            assert_eq!(spec.network, "lima:user-v2");
        }
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe {
                std::env::set_var(name, value);
            },
            None => unsafe {
                std::env::remove_var(name);
            },
        }
    }
}
