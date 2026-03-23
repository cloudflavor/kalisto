use std::path::{Path, PathBuf};

use tracing::debug;

use crate::backend::runtime::{BackendConfig, BackendKind, ClusterBackend, InstanceConfig};
use crate::error::{Error, Result};
use crate::process::{CommandRunner, Output};
use crate::shell::shell_escape as shared_shell_escape;

#[derive(Debug, Clone, Copy)]
pub struct LimaBackend;

pub const BACKEND: LimaBackend = LimaBackend;

pub fn canonicalize_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(|source| Error::CommandIo {
        program: "canonicalize".to_owned(),
        source,
    })
}

impl ClusterBackend for LimaBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Lima
    }

    fn check_available(&self, runner: &dyn CommandRunner) -> Result<()> {
        runner.run("limactl", &["--version".into()])?;
        Ok(())
    }

    fn start_instance(
        &self,
        runner: &dyn CommandRunner,
        config: &BackendConfig,
        instance: &InstanceConfig,
    ) -> Result<Output> {
        self::start_instance(runner, config, instance)
    }

    fn stop_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        _runtime_root: &Path,
        _force: bool,
    ) -> Result<()> {
        self::stop_instance(runner, instance_name)
    }

    fn delete_instance(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        _runtime_root: &Path,
    ) -> Result<()> {
        self::delete_instance(runner, instance_name)
    }

    fn query_instance_status(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        _runtime_root: &Path,
    ) -> Result<String> {
        self::query_instance_status(runner, instance_name)
    }

    fn guest_artifact_root(&self, _instance_name: &str, runtime_root: &Path) -> Result<PathBuf> {
        Ok(runtime_root.to_path_buf())
    }

    fn guest_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        _runtime_root: &Path,
        command: &str,
    ) -> Result<Output> {
        self::guest_shell(runner, instance_name, command)
    }

    fn guest_sudo_shell(
        &self,
        runner: &dyn CommandRunner,
        instance_name: &str,
        _runtime_root: &Path,
        command: &str,
    ) -> Result<Output> {
        self::guest_sudo_shell(runner, instance_name, command)
    }

    fn control_plane_host(&self, instance_name: &str, _runtime_root: &Path) -> Result<String> {
        Ok(self::internal_hostname(instance_name))
    }

    fn host_api_endpoint(
        &self,
        _instance_name: &str,
        _runtime_root: &Path,
        requested_port: u16,
    ) -> Result<String> {
        Ok(format!("127.0.0.1:{requested_port}"))
    }
}

pub fn start_instance(
    runner: &dyn CommandRunner,
    config: &BackendConfig,
    instance: &InstanceConfig,
) -> Result<Output> {
    debug!(
        instance = %instance.name,
        template = %config.template,
        workspace_root = %config.workspace_root.display(),
        cpus = instance.cpus,
        memory_gib = instance.memory_gib,
        host_port_forward = ?instance.host_port_forward,
        rosetta = config.enable_rosetta,
        network = %config.network,
        "starting lima instance"
    );
    let mut args = vec![
        "start".into(),
        "--tty=false".into(),
        format!("--name={}", instance.name),
        format!("--cpus={}", instance.cpus),
        format!("--memory={}", instance.memory_gib),
        "--containerd=none".into(),
        "--vm-type=vz".into(),
        "--mount-type=virtiofs".into(),
        format!("--mount-only={}:w", config.workspace_root.display()),
        format!("--network={}", config.network),
        "--timeout=10m".into(),
    ];
    if config.enable_rosetta {
        args.push("--rosetta".into());
    }
    if let Some(host_port) = instance.host_port_forward {
        args.push(format!("--port-forward={host_port}:6443,static=true"));
    }
    args.push(config.template.clone());
    runner.run_live("limactl", &args)
}

pub fn guest_shell(
    runner: &dyn CommandRunner,
    instance_name: &str,
    command: &str,
) -> Result<Output> {
    debug!(instance = %instance_name, command, "running guest shell command");
    runner.run(
        "limactl",
        &[
            "-y".into(),
            "shell".into(),
            instance_name.to_owned(),
            "sh".into(),
            "-lc".into(),
            command.to_owned(),
        ],
    )
}

pub fn guest_sudo_shell(
    runner: &dyn CommandRunner,
    instance_name: &str,
    command: &str,
) -> Result<Output> {
    debug!(instance = %instance_name, command, "running guest sudo shell command");
    runner.run(
        "limactl",
        &[
            "-y".into(),
            "shell".into(),
            instance_name.to_owned(),
            "sudo".into(),
            "sh".into(),
            "-lc".into(),
            command.to_owned(),
        ],
    )
}

pub fn stop_instance(runner: &dyn CommandRunner, instance_name: &str) -> Result<()> {
    debug!(instance = %instance_name, "stopping lima instance");
    runner.run(
        "limactl",
        &[
            "-y".into(),
            "stop".into(),
            "--force".into(),
            instance_name.to_owned(),
        ],
    )?;
    Ok(())
}

pub fn delete_instance(runner: &dyn CommandRunner, instance_name: &str) -> Result<()> {
    debug!(instance = %instance_name, "deleting lima instance");
    runner.run(
        "limactl",
        &[
            "-y".into(),
            "delete".into(),
            "--force".into(),
            instance_name.to_owned(),
        ],
    )?;
    Ok(())
}

pub fn query_instance_status(runner: &dyn CommandRunner, instance_name: &str) -> Result<String> {
    debug!(instance = %instance_name, "querying lima instance status");
    let output = runner.run(
        "limactl",
        &[
            "list".into(),
            "--format".into(),
            "{{ .Name }} {{ .Status }} {{ .VMType }}".into(),
            instance_name.to_owned(),
        ],
    )?;
    Ok(output
        .stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "unknown".to_owned()))
}

pub fn internal_hostname(instance_name: &str) -> String {
    format!("lima-{instance_name}.internal")
}

pub fn shell_escape(input: &str) -> String {
    shared_shell_escape(input)
}
