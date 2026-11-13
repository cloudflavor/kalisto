use std::path::PathBuf;

use crate::error::Result;
use crate::process::{CommandRunner, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Firecracker,
    Lima,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Firecracker => "firecracker",
            Self::Lima => "lima",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "firecracker" => Some(Self::Firecracker),
            "lima" => Some(Self::Lima),
            _ => None,
        }
    }

    pub fn host_default() -> Self {
        if cfg!(target_os = "linux") {
            Self::Firecracker
        } else {
            Self::Lima
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub template: String,
    pub workspace_root: PathBuf,
    pub enable_rosetta: bool,
    pub network: String,
    pub firecracker_kernel: Option<PathBuf>,
    pub firecracker_rootfs: Option<PathBuf>,
    pub stream_vm_logs: bool,
    pub boot_assets_root: Option<crate::pull::PreparedAssets>,
}

#[derive(Debug, Clone)]
pub struct InstanceConfig {
    pub name: String,
    pub runtime_root: PathBuf,
    pub host_port_forward: Option<u16>,
    pub cpus: u8,
    pub memory_gib: u8,
}

pub trait ClusterBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    fn check_available(&self, runner: &dyn CommandRunner) -> Result<()>;

    fn start_instance(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance: &InstanceConfig,
    ) -> Result<Output>;

    fn stop_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
        force: bool,
    ) -> Result<()>;

    fn delete_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
    ) -> Result<()>;

    fn query_instance_status(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
    ) -> Result<String>;

    fn guest_artifact_root(
        &self,
        instance_name: &str,
        runtime_root: &std::path::Path,
    ) -> Result<PathBuf>;

    fn guest_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
        command: &str,
    ) -> Result<Output>;

    fn guest_sudo_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
        command: &str,
    ) -> Result<Output>;

    fn control_plane_host(
        &self,
        instance_name: &str,
        runtime_root: &std::path::Path,
    ) -> Result<String>;

    fn host_api_endpoint(
        &self,
        instance_name: &str,
        runtime_root: &std::path::Path,
        requested_port: u16,
    ) -> Result<String>;

    fn prepare_cluster_network(
        &self,
        _runner: &dyn CommandRunner,
        _config: &BackendConfig,
        _instance_names: &[String],
    ) -> Result<()> {
        Ok(())
    }

    fn cleanup_cluster_network(
        &self,
        _runner: &dyn CommandRunner,
        _config: &BackendConfig,
        _instance_names: &[String],
    ) -> Result<()> {
        Ok(())
    }
}

pub fn backend_for(kind: BackendKind) -> &'static dyn ClusterBackend {
    match kind {
        BackendKind::Firecracker => &crate::backend::firecracker::BACKEND,
        BackendKind::Lima => &crate::backend::lima::BACKEND,
    }
}

#[cfg(test)]
mod tests {
    use super::BackendKind;

    #[test]
    fn backend_kind_roundtrips() {
        assert_eq!(
            BackendKind::parse(BackendKind::Lima.as_str()),
            Some(BackendKind::Lima)
        );
        assert_eq!(
            BackendKind::parse(BackendKind::Firecracker.as_str()),
            Some(BackendKind::Firecracker)
        );
    }

    #[test]
    fn host_default_matches_compilation_target() {
        if cfg!(target_os = "linux") {
            assert_eq!(BackendKind::host_default(), BackendKind::Firecracker);
        } else {
            assert_eq!(BackendKind::host_default(), BackendKind::Lima);
        }
    }
}
