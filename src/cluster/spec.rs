use std::path::{Path, PathBuf};

use crate::k3s::{ComponentToggles, ServerMode};

#[derive(Debug, Clone)]
pub struct ClusterSpec {
    pub name: String,
    pub workspace_root: PathBuf,
    pub template: String,
    pub api_port: u16,
    pub worker_count: usize,
    pub enable_rosetta: bool,
    pub network: String,
    pub k3s_version: String,
    pub server_mode: ServerMode,
    pub component_toggles: ComponentToggles,
}

impl ClusterSpec {
    pub fn new(name: String, workspace_root: PathBuf) -> Self {
        Self {
            template: default_template(&workspace_root),
            api_port: 16443,
            worker_count: 0,
            enable_rosetta: true,
            network: "vzNAT".to_owned(),
            k3s_version: "v1.33.5+k3s1".to_owned(),
            server_mode: ServerMode::Standard,
            component_toggles: ComponentToggles::default(),
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
        self.workspace_root
            .join(".kalisto")
            .join("clusters")
            .join(&self.name)
    }

    pub fn helper_script(&self) -> PathBuf {
        self.workspace_root
            .join("scripts")
            .join("macos")
            .join("k3s_server_probe.sh")
    }

    pub fn control_plane_name(&self) -> String {
        format!("kalisto-{}-cp", self.name)
    }
}

pub fn default_template(workspace_root: &Path) -> String {
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

    use super::{ClusterSpec, default_template};

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
}
