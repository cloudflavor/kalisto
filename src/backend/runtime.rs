use std::path::PathBuf;

use crate::error::Result;
use crate::process::{CommandRunner, Output};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Firecracker,
    Lima,
    AppleContainer,
}

impl BackendKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Firecracker => "firecracker",
            Self::Lima => "lima",
            Self::AppleContainer => "apple-container",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "firecracker" => Some(Self::Firecracker),
            "lima" => Some(Self::Lima),
            "apple-container" => Some(Self::AppleContainer),
            _ => None,
        }
    }

    #[must_use]
    pub fn host_default() -> Self {
        if cfg!(target_os = "linux") {
            Self::Firecracker
        } else if cfg!(target_os = "macos") {
            Self::AppleContainer
        } else {
            Self::Lima
        }
    }

    #[must_use]
    pub(crate) fn uses_prepared_assets(self) -> bool {
        matches!(self, Self::Firecracker | Self::Lima | Self::AppleContainer)
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

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn check_available(&self, runner: &dyn CommandRunner) -> Result<()>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn start_instance(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance: &InstanceConfig,
    ) -> Result<Output>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn stop_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
        force: bool,
    ) -> Result<()>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn delete_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
    ) -> Result<()>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn query_instance_status(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
    ) -> Result<String>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn guest_artifact_root(
        &self,
        instance_name: &str,
        runtime_root: &std::path::Path,
    ) -> Result<PathBuf>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn guest_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
        command: &str,
    ) -> Result<Output>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn guest_sudo_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
        command: &str,
    ) -> Result<Output>;

    /// Make a host file available in the guest with mode 0600.
    ///
    /// The default uses the identical host paths exposed by shared runtime
    /// mounts. Backends without those mounts must implement file transport.
    ///
    /// # Errors
    ///
    /// Returns an error when the source is unavailable or installation fails.
    fn copy_file_to_guest(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        runtime_root: &std::path::Path,
        source: &std::path::Path,
        destination: &std::path::Path,
    ) -> Result<()> {
        let quote = |path: &std::path::Path| {
            format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
        };
        let command = if source == destination {
            format!("test -f {0} && chmod 0600 -- {0}", quote(destination))
        } else {
            format!(
                "install -D -m 0600 -- {} {}",
                quote(source),
                quote(destination)
            )
        };
        self.guest_sudo_shell(runner, instance_name, runtime_root, &command)?;
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn control_plane_host(
        &self,
        instance_name: &str,
        runtime_root: &std::path::Path,
    ) -> Result<String>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn host_api_endpoint(
        &self,
        instance_name: &str,
        runtime_root: &std::path::Path,
        requested_port: u16,
    ) -> Result<String>;

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn prepare_cluster_network(
        &self,
        _runner: &dyn CommandRunner,
        _config: &BackendConfig,
        _instance_names: &[String],
    ) -> Result<()> {
        Ok(())
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    fn cleanup_cluster_network(
        &self,
        _runner: &dyn CommandRunner,
        _config: &BackendConfig,
        _instance_names: &[String],
    ) -> Result<()> {
        Ok(())
    }
}

#[must_use]
pub fn backend_for(kind: BackendKind) -> &'static dyn ClusterBackend {
    match kind {
        BackendKind::Firecracker => &crate::backend::firecracker::BACKEND,
        BackendKind::Lima => &crate::backend::lima::BACKEND,
        BackendKind::AppleContainer => &crate::backend::apple_container::BACKEND,
    }
}

#[cfg(test)]
mod tests {
    use super::BackendKind;

    #[test]
    fn every_backend_uses_prepared_assets() {
        for backend in [
            BackendKind::Firecracker,
            BackendKind::Lima,
            BackendKind::AppleContainer,
        ] {
            assert!(backend.uses_prepared_assets());
        }
    }

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
        } else if cfg!(target_os = "macos") {
            assert_eq!(BackendKind::host_default(), BackendKind::AppleContainer);
        } else {
            assert_eq!(BackendKind::host_default(), BackendKind::Lima);
        }
    }
}
