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
    if instance_exists(runner, &instance.name)? {
        let restart_args = vec!["start".into(), "--tty=false".into(), instance.name.clone()];
        return runner.run_live("limactl", &restart_args);
    }

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

fn instance_exists(runner: &dyn CommandRunner, instance_name: &str) -> Result<bool> {
    let args = vec![
        "list".into(),
        "--format".into(),
        "{{ .Name }}".into(),
        instance_name.to_owned(),
    ];
    match runner.run("limactl", &args) {
        Ok(output) => Ok(output
            .stdout
            .lines()
            .any(|line| line.trim() == instance_name)),
        Err(Error::CommandFailed { stderr, .. }) if is_missing_instance_error(&stderr) => Ok(false),
        Err(error) => Err(error),
    }
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
    match runner.run(
        "limactl",
        &[
            "-y".into(),
            "delete".into(),
            "--force".into(),
            instance_name.to_owned(),
        ],
    ) {
        Ok(_) => Ok(()),
        Err(Error::CommandFailed { stderr, .. }) if is_missing_instance_error(&stderr) => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn query_instance_status(runner: &dyn CommandRunner, instance_name: &str) -> Result<String> {
    debug!(instance = %instance_name, "querying lima instance status");
    let args = vec![
        "list".into(),
        "--format".into(),
        "{{ .Name }} {{ .Status }} {{ .VMType }}".into(),
        instance_name.to_owned(),
    ];
    let output = match runner.run("limactl", &args) {
        Ok(output) => output,
        Err(Error::CommandFailed { stderr, .. }) if is_missing_instance_error(&stderr) => {
            return Ok(format!("{instance_name} Stopped vz"));
        }
        Err(error) => return Err(error),
    };
    Ok(output
        .stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "unknown".to_owned()))
}

fn is_missing_instance_error(stderr: &str) -> bool {
    let normalized = stderr.to_ascii_lowercase();
    normalized.contains("no instance matching") || normalized.contains("unmatched instances")
}

pub fn internal_hostname(instance_name: &str) -> String {
    format!("lima-{instance_name}.internal")
}

pub fn shell_escape(input: &str) -> String {
    shared_shell_escape(input)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::{query_instance_status, start_instance};
    use crate::backend::runtime::{BackendConfig, InstanceConfig};
    use crate::error::Error;
    use crate::process::{CommandRunner, Output};

    struct FakeRunner {
        result: crate::error::Result<Output>,
    }

    struct SequencedRunner {
        results: Mutex<Vec<crate::error::Result<Output>>>,
        invocations: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, _program: &str, _args: &[String]) -> crate::error::Result<Output> {
            match &self.result {
                Ok(output) => Ok(output.clone()),
                Err(Error::CommandFailed {
                    program,
                    args,
                    code,
                    stdout,
                    stderr,
                }) => Err(Error::CommandFailed {
                    program: program.clone(),
                    args: args.clone(),
                    code: *code,
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                }),
                Err(Error::CommandIo { program, source }) => Err(Error::CommandIo {
                    program: program.clone(),
                    source: std::io::Error::new(source.kind(), source.to_string()),
                }),
                Err(Error::InvalidArgs(message)) => Err(Error::InvalidArgs(message.clone())),
                Err(Error::MissingPath(path)) => Err(Error::MissingPath(path.clone())),
                Err(Error::Utf8(_)) => unreachable!("utf8 errors are not used in these tests"),
            }
        }
    }

    impl CommandRunner for SequencedRunner {
        fn run(&self, program: &str, args: &[String]) -> crate::error::Result<Output> {
            self.invocations
                .lock()
                .unwrap()
                .push((program.to_owned(), args.to_vec()));
            self.results.lock().unwrap().remove(0)
        }

        fn run_live(&self, program: &str, args: &[String]) -> crate::error::Result<Output> {
            self.run(program, args)
        }
    }

    #[test]
    fn query_instance_status_returns_stopped_for_missing_instance() {
        let runner = FakeRunner {
            result: Err(Error::CommandFailed {
                program: "limactl".to_owned(),
                args: vec![
                    "list".to_owned(),
                    "--format".to_owned(),
                    "{{ .Name }} {{ .Status }} {{ .VMType }}".to_owned(),
                    "kalisto-default-cp".to_owned(),
                ],
                code: Some(1),
                stdout: String::new(),
                stderr: concat!(
                    "time=\"2026-03-23T11:41:06+01:00\" level=warning ",
                    "msg=\"No instance matching kalisto-default-cp found.\"\n",
                    "time=\"2026-03-23T11:41:06+01:00\" level=fatal msg=\"unmatched instances\""
                )
                .to_owned(),
            }),
        };

        let status = query_instance_status(&runner, "kalisto-default-cp").unwrap();

        assert_eq!(status, "kalisto-default-cp Stopped vz");
    }

    #[test]
    fn query_instance_status_returns_formatted_output_when_instance_exists() {
        let runner = FakeRunner {
            result: Ok(Output {
                stdout: "kalisto-default-cp Running vz\n".to_owned(),
                stderr: String::new(),
            }),
        };

        let status = query_instance_status(&runner, "kalisto-default-cp").unwrap();

        assert_eq!(status, "kalisto-default-cp Running vz");
    }

    #[test]
    fn start_instance_restarts_existing_instance_without_template_args() {
        let runner = SequencedRunner {
            results: Mutex::new(vec![
                Ok(Output {
                    stdout: "kalisto-default-cp\n".to_owned(),
                    stderr: String::new(),
                }),
                Ok(Output {
                    stdout: "started\n".to_owned(),
                    stderr: String::new(),
                }),
            ]),
            invocations: Mutex::new(Vec::new()),
        };
        let config = BackendConfig {
            template: "template://ubuntu".to_owned(),
            workspace_root: PathBuf::from("/workspace"),
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            stream_vm_logs: false,
        };
        let instance = InstanceConfig {
            name: "kalisto-default-cp".to_owned(),
            runtime_root: PathBuf::from("/tmp/runtime"),
            host_port_forward: Some(16443),
            cpus: 4,
            memory_gib: 6,
        };

        start_instance(&runner, &config, &instance).unwrap();

        let invocations = runner.invocations.lock().unwrap();
        assert_eq!(invocations.len(), 2);
        assert_eq!(
            invocations[0],
            (
                "limactl".to_owned(),
                vec![
                    "list".to_owned(),
                    "--format".to_owned(),
                    "{{ .Name }}".to_owned(),
                    "kalisto-default-cp".to_owned(),
                ],
            )
        );
        assert_eq!(
            invocations[1],
            (
                "limactl".to_owned(),
                vec![
                    "start".to_owned(),
                    "--tty=false".to_owned(),
                    "kalisto-default-cp".to_owned(),
                ],
            )
        );
    }

    #[test]
    fn start_instance_creates_missing_instance_from_template() {
        let runner = SequencedRunner {
            results: Mutex::new(vec![
                Err(Error::CommandFailed {
                    program: "limactl".to_owned(),
                    args: vec![
                        "list".to_owned(),
                        "--format".to_owned(),
                        "{{ .Name }}".to_owned(),
                        "kalisto-default-cp".to_owned(),
                    ],
                    code: Some(1),
                    stdout: String::new(),
                    stderr: concat!(
                        "time=\"2026-03-23T11:41:06+01:00\" level=warning ",
                        "msg=\"No instance matching kalisto-default-cp found.\"\n",
                        "time=\"2026-03-23T11:41:06+01:00\" level=fatal msg=\"unmatched instances\""
                    )
                    .to_owned(),
                }),
                Ok(Output {
                    stdout: "created\n".to_owned(),
                    stderr: String::new(),
                }),
            ]),
            invocations: Mutex::new(Vec::new()),
        };
        let config = BackendConfig {
            template: "template://ubuntu".to_owned(),
            workspace_root: PathBuf::from("/workspace"),
            enable_rosetta: true,
            network: "lima:user-v2".to_owned(),
            firecracker_kernel: None,
            firecracker_rootfs: None,
            stream_vm_logs: false,
        };
        let instance = InstanceConfig {
            name: "kalisto-default-cp".to_owned(),
            runtime_root: PathBuf::from("/tmp/runtime"),
            host_port_forward: Some(16443),
            cpus: 4,
            memory_gib: 6,
        };

        start_instance(&runner, &config, &instance).unwrap();

        let invocations = runner.invocations.lock().unwrap();
        assert_eq!(invocations.len(), 2);
        assert_eq!(
            invocations[0].1,
            vec!["list", "--format", "{{ .Name }}", "kalisto-default-cp"]
        );
        assert!(
            invocations[1]
                .1
                .iter()
                .any(|arg| arg == "--name=kalisto-default-cp")
        );
        assert!(
            invocations[1]
                .1
                .iter()
                .any(|arg| arg == "template://ubuntu")
        );
        assert!(
            !invocations[1]
                .1
                .iter()
                .any(|arg| arg == "kalisto-default-cp")
        );
    }
}
