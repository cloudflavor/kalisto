use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime};

use crate::backend::ClusterBackend;
use crate::error::{Error, Result};
use crate::process::{CommandRunner, Output};
use crate::shell::shell_escape;
use crate::telemetry::raw_streams_enabled;

use super::util::acceptable_apiserver_status;

#[derive(Debug, Clone)]
pub struct GuestWait<'a> {
    pub instance_name: &'a str,
    pub runtime_root: &'a Path,
    pub command: &'a str,
    pub timeout: Duration,
    pub consecutive_successes: usize,
    pub interval: Duration,
}

#[derive(Debug, Clone)]
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

pub fn wait_for_path_with_tail(
    path: &Path,
    timeout: Duration,
    tail: &mut LiveLogTail,
) -> Result<()> {
    let started = SystemTime::now();
    loop {
        tail.emit_pending()?;
        if path.is_file()
            && fs::metadata(path)
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false)
        {
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

pub fn wait_for_worker_process(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    instance_name: &str,
    runtime_root: &Path,
) -> Result<Output> {
    wait_for_stable_guest_success(
        backend,
        runner,
        GuestWait {
            instance_name,
            runtime_root,
            command: "ps -ef | grep -F '[k]3s agent' >/dev/null",
            timeout: Duration::from_secs(60),
            consecutive_successes: 1,
            interval: Duration::from_secs(2),
        },
    )
}

pub fn wait_for_node_ready(
    backend: &dyn ClusterBackend,
    runner: &dyn CommandRunner,
    control_plane_name: &str,
    control_plane_runtime_root: &Path,
    control_plane_kubeconfig: &Path,
    worker_name: &str,
) -> Result<Output> {
    let command = format!(
        "KUBECONFIG={} /usr/local/bin/k3s kubectl get node {} -o jsonpath='{{range .status.conditions[*]}}{{.type}}={{.status}}{{\"\\n\"}}{{end}}' | grep -Fx 'Ready=True'",
        shell_escape(
            control_plane_kubeconfig
                .as_os_str()
                .to_string_lossy()
                .as_ref()
        ),
        shell_escape(worker_name),
    );
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

fn wait_for_stable_guest_success(
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
