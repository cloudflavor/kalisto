use std::path::{Path, PathBuf};

use tracing::debug;

use crate::error::{Error, Result};
use crate::process::{CommandRunner, Output};

#[derive(Debug, Clone)]
pub struct Config {
    pub template: String,
    pub workspace_root: PathBuf,
    pub enable_rosetta: bool,
    pub network: String,
}

#[derive(Debug, Clone)]
pub struct InstanceConfig {
    pub name: String,
    pub host_port_forward: Option<u16>,
    pub cpus: u8,
    pub memory_gib: u8,
}

pub fn canonicalize_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(|source| Error::CommandIo {
        program: "canonicalize".to_owned(),
        source,
    })
}

pub fn start_instance(
    runner: &dyn CommandRunner,
    config: &Config,
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

pub fn stop_and_delete_instance(runner: &dyn CommandRunner, instance_name: &str) -> Result<()> {
    debug!(instance = %instance_name, "stopping lima instance");
    let _ = runner.run(
        "limactl",
        &[
            "-y".into(),
            "stop".into(),
            "--force".into(),
            instance_name.to_owned(),
        ],
    );
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
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}
