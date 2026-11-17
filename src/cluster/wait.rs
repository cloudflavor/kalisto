use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime};

use crate::backend::{BackendKind, ClusterBackend};
use crate::cluster::node::Instance;
use crate::error::{Error, Result};
use crate::process::{CommandRunner, Output};
use crate::shell::shell_escape;
use crate::telemetry::raw_streams_enabled;

use super::util::acceptable_apiserver_status;

#[derive(Debug, Clone, Copy)]
pub struct GuestWait<'a> {
    pub instance_name: &'a str,
    pub runtime_root: &'a Path,
    pub command: &'a str,
    pub timeout: Duration,
    pub consecutive_successes: usize,
    pub interval: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct GuestFileCopy<'a> {
    pub instance_name: &'a str,
    pub runtime_root: &'a Path,
    pub guest_path: &'a Path,
    pub host_path: &'a Path,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct LiveLogTail {
    path: std::path::PathBuf,
    label: String,
    offset: u64,
}

impl LiveLogTail {
    pub fn new(path: impl Into<std::path::PathBuf>, label: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            label: label.into(),
            offset: 0,
        }
    }

    /// # Errors
    ///
    /// Returns an error when the operation cannot be completed.
    pub fn emit_pending(&mut self) -> Result<()> {
        if !self.path.is_file() {
            return Ok(());
        }

        let mut file = fs::File::open(&self.path).map_err(|source| Error::CommandIo {
            program: "open".to_owned(),
            source,
        })?;
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|source| Error::CommandIo {
                program: "seek".to_owned(),
                source,
            })?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| Error::CommandIo {
                program: "read_to_end".to_owned(),
                source,
            })?;
        if bytes.is_empty() {
            return Ok(());
        }

        self.offset += bytes.len() as u64;
        if !raw_streams_enabled() {
            return Ok(());
        }
        let text = String::from_utf8(bytes)?;
        for line in text.lines() {
            eprintln!("[{}] {}", self.label, line);
        }
        Ok(())
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn wait_for_path_with_tail(
    path: &Path,
    timeout: Duration,
    tail: &mut LiveLogTail,
) -> Result<()> {
    let started = SystemTime::now();
    loop {
        tail.emit_pending()?;
        if path.is_file() && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            tail.emit_pending()?;
            return Ok(());
        }
        if started.elapsed().unwrap_or_default() >= timeout {
            tail.emit_pending()?;
            return Err(Error::InvalidArgs(format!(
                "timed out waiting for file {}",
                path.display()
            )));
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn wait_for_guest_text_file_copy(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    copy: GuestFileCopy<'_>,
    tail: &mut LiveLogTail,
) -> Result<()> {
    let started = SystemTime::now();
    let command = format!(
        "test -s {} && cat {}",
        shell_escape(&copy.guest_path.display().to_string()),
        shell_escape(&copy.guest_path.display().to_string())
    );
    loop {
        tail.emit_pending()?;
        match backend.guest_sudo_shell(runner, copy.instance_name, copy.runtime_root, &command) {
            Ok(output) if !output.stdout.is_empty() => {
                if let Some(parent) = copy.host_path.parent() {
                    fs::create_dir_all(parent).map_err(|source| Error::CommandIo {
                        program: "create_dir_all".to_owned(),
                        source,
                    })?;
                }
                fs::write(copy.host_path, output.stdout).map_err(|source| Error::CommandIo {
                    program: "write".to_owned(),
                    source,
                })?;
                tail.emit_pending()?;
                return Ok(());
            }
            Ok(_) | Err(_) => {
                if started.elapsed().unwrap_or_default() >= copy.timeout {
                    tail.emit_pending()?;
                    return Err(Error::InvalidArgs(format!(
                        "timed out waiting for guest file {} from {}",
                        copy.guest_path.display(),
                        copy.instance_name
                    )));
                }
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn wait_for_stable_guest_success_with_tail(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    wait: GuestWait<'_>,
    tail: &mut LiveLogTail,
) -> Result<Output> {
    let started = SystemTime::now();
    let mut successful_checks = 0usize;
    loop {
        tail.emit_pending()?;
        match backend.guest_sudo_shell(runner, wait.instance_name, wait.runtime_root, wait.command)
        {
            Ok(output) => {
                successful_checks += 1;
                if successful_checks >= wait.consecutive_successes {
                    tail.emit_pending()?;
                    return Ok(output);
                }
            }
            Err(error) => {
                successful_checks = 0;
                if started.elapsed().unwrap_or_default() >= wait.timeout {
                    tail.emit_pending()?;
                    return Err(error);
                }
            }
        }
        thread::sleep(wait.interval);
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn wait_for_stable_guest_success(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    wait: GuestWait<'_>,
) -> Result<Output> {
    let started = SystemTime::now();
    let mut successful_checks = 0usize;
    loop {
        match backend.guest_sudo_shell(runner, wait.instance_name, wait.runtime_root, wait.command)
        {
            Ok(output) => {
                successful_checks += 1;
                if successful_checks >= wait.consecutive_successes {
                    return Ok(output);
                }
            }
            Err(error) => {
                successful_checks = 0;
                if started.elapsed().unwrap_or_default() >= wait.timeout {
                    return Err(error);
                }
            }
        }
        thread::sleep(wait.interval);
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn wait_for_host_http_with_tail(
    runner: &dyn CommandRunner,
    url: &str,
    timeout: Duration,
    tail: &mut LiveLogTail,
) -> Result<Output> {
    let started = SystemTime::now();
    loop {
        tail.emit_pending()?;
        match runner.run(
            "curl",
            &[
                "-ksS".into(),
                "--max-time".into(),
                "5".into(),
                "-o".into(),
                "/dev/null".into(),
                "-w".into(),
                "%{http_code}".into(),
                url.to_owned(),
            ],
        ) {
            Ok(output) if acceptable_apiserver_status(&output.stdout) => {
                tail.emit_pending()?;
                return Ok(output);
            }
            Ok(output) => {
                if started.elapsed().unwrap_or_default() >= timeout {
                    tail.emit_pending()?;
                    return Err(Error::InvalidArgs(format!(
                        "apiserver returned unexpected status `{}` for {url}",
                        output.stdout.trim()
                    )));
                }
            }
            Err(error) => {
                if started.elapsed().unwrap_or_default() >= timeout {
                    tail.emit_pending()?;
                    return Err(error);
                }
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn wait_for_worker_process(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
) -> Result<Output> {
    let artifact_dir = backend.guest_artifact_root(instance_name, runtime_root)?;
    let pid_file = shell_escape(
        artifact_dir
            .join("k3s.pid")
            .as_os_str()
            .to_string_lossy()
            .as_ref(),
    );
    let command = format!(
        "test -s {pid_file} && pid=$(cat {pid_file}) && test -n \"$pid\" && kill -0 \"$pid\" 2>/dev/null",
    );
    wait_for_stable_guest_success(
        backend,
        runner,
        GuestWait {
            instance_name,
            runtime_root,
            command: &command,
            timeout: Duration::from_secs(60),
            consecutive_successes: 1,
            interval: Duration::from_secs(2),
        },
    )
}

/// # Errors
///
/// Returns an error when the operation cannot be completed.
pub fn wait_for_node_ready(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane_name: &str,
    control_plane_runtime_root: &Path,
    control_plane_kubeconfig: &Path,
    node: &Instance,
) -> Result<Output> {
    let kubeconfig = shell_escape(
        control_plane_kubeconfig
            .as_os_str()
            .to_string_lossy()
            .as_ref(),
    );
    let node_name = shell_escape(&node.name);
    let command = if backend.kind() == BackendKind::AppleContainer {
        // A persisted Ready condition can predate this boot and the agent's new
        // serving certificate. Check current identity and the actual kubelet route.
        let boot = backend.guest_shell(
            runner,
            &node.name,
            &node.artifacts.root,
            "cat /proc/sys/kernel/random/boot_id",
        )?;
        let ip = backend.control_plane_host(&node.name, &node.artifacts.root)?;
        let boot = shell_escape(&format!("bootID={}", boot.stdout.trim()));
        let ip = shell_escape(&format!("InternalIP={ip}"));
        let endpoint = shell_escape(&format!("/api/v1/nodes/{}/proxy/healthz", node.name));
        format!(
            concat!(
                "set -eu\n",
                "status=$(KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl ",
                "--request-timeout=5s get node {node_name} -o jsonpath='",
                "{{range .status.conditions[*]}}{{.type}}={{.status}}{{\"\\n\"}}{{end}}",
                "bootID={{.status.nodeInfo.bootID}}{{\"\\n\"}}",
                "{{range .status.addresses[?(@.type==\"InternalIP\")]}}",
                "InternalIP={{.address}}{{\"\\n\"}}{{end}}')\n",
                "printf '%s\\n' \"$status\" | grep -Fx 'Ready=True'\n",
                "printf '%s\\n' \"$status\" | grep -Fx {boot}\n",
                "printf '%s\\n' \"$status\" | grep -Fx {ip}\n",
                "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl ",
                "--request-timeout=5s get --raw={endpoint} | grep -Fx ok"
            ),
            kubeconfig = kubeconfig,
            node_name = node_name,
            boot = boot,
            ip = ip,
            endpoint = endpoint,
        )
    } else {
        format!(
            "KUBECONFIG={kubeconfig} /usr/local/bin/k3s kubectl get node {node_name} -o jsonpath='{{range .status.conditions[*]}}{{.type}}={{.status}}{{\"\\n\"}}{{end}}' | grep -Fx 'Ready=True'",
        )
    };
    wait_for_stable_guest_success(
        backend,
        runner,
        GuestWait {
            instance_name: control_plane_name,
            runtime_root: control_plane_runtime_root,
            command: &command,
            timeout: Duration::from_secs(180),
            consecutive_successes: 1,
            interval: Duration::from_secs(3),
        },
    )
}
