use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime};

use crate::backend::lima::{guest_sudo_shell, shell_escape};
use crate::error::{Error, Result};
use crate::process::{CommandRunner, Output};
use crate::telemetry::raw_streams_enabled;

use super::node::Instance;
use super::util::acceptable_apiserver_status;

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

pub fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let started = SystemTime::now();
    loop {
        if path.is_file()
            && fs::metadata(path)
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false)
        {
            return Ok(());
        }
        if started.elapsed().unwrap_or_default() >= timeout {
            return Err(Error::InvalidArgs(format!(
                "timed out waiting for file {}",
                path.display()
            )));
        }
        thread::sleep(Duration::from_secs(2));
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

pub fn wait_for_stable_guest_success(
    runner: &dyn CommandRunner,
    instance_name: &str,
    command: &str,
    timeout: Duration,
    consecutive_successes: usize,
    interval: Duration,
) -> Result<Output> {
    let started = SystemTime::now();
    let mut successful_checks = 0usize;
    loop {
        match guest_sudo_shell(runner, instance_name, command) {
            Ok(output) => {
                successful_checks += 1;
                if successful_checks >= consecutive_successes {
                    return Ok(output);
                }
            }
            Err(error) => {
                successful_checks = 0;
                if started.elapsed().unwrap_or_default() >= timeout {
                    return Err(error);
                }
            }
        }
        thread::sleep(interval);
    }
}

pub fn wait_for_stable_guest_success_with_tail(
    runner: &dyn CommandRunner,
    instance_name: &str,
    command: &str,
    timeout: Duration,
    consecutive_successes: usize,
    interval: Duration,
    tail: &mut LiveLogTail,
) -> Result<Output> {
    let started = SystemTime::now();
    let mut successful_checks = 0usize;
    loop {
        tail.emit_pending()?;
        match guest_sudo_shell(runner, instance_name, command) {
            Ok(output) => {
                successful_checks += 1;
                if successful_checks >= consecutive_successes {
                    tail.emit_pending()?;
                    return Ok(output);
                }
            }
            Err(error) => {
                successful_checks = 0;
                if started.elapsed().unwrap_or_default() >= timeout {
                    tail.emit_pending()?;
                    return Err(error);
                }
            }
        }
        thread::sleep(interval);
    }
}

pub fn wait_for_host_http(
    runner: &dyn CommandRunner,
    url: &str,
    timeout: Duration,
) -> Result<Output> {
    let started = SystemTime::now();
    loop {
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
            Ok(output) if acceptable_apiserver_status(&output.stdout) => return Ok(output),
            Ok(output) => {
                if started.elapsed().unwrap_or_default() >= timeout {
                    return Err(Error::InvalidArgs(format!(
                        "apiserver returned unexpected status `{}` for {url}",
                        output.stdout.trim()
                    )));
                }
            }
            Err(error) => {
                if started.elapsed().unwrap_or_default() >= timeout {
                    return Err(error);
                }
            }
        }
        thread::sleep(Duration::from_secs(2));
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

pub fn wait_for_worker_process(runner: &dyn CommandRunner, instance_name: &str) -> Result<Output> {
    wait_for_stable_guest_success(
        runner,
        instance_name,
        "ps -ef | grep -F '[k]3s agent' >/dev/null",
        Duration::from_secs(60),
        1,
        Duration::from_secs(2),
    )
}

pub fn wait_for_node_ready(
    runner: &dyn CommandRunner,
    control_plane: &Instance,
    worker_name: &str,
) -> Result<Output> {
    let command = format!(
        "KUBECONFIG={} /usr/local/bin/k3s kubectl get node {} -o jsonpath='{{range .status.conditions[*]}}{{.type}}={{.status}}{{\"\\n\"}}{{end}}' | grep -Fx 'Ready=True'",
        shell_escape(
            control_plane
                .artifacts
                .kubeconfig
                .as_os_str()
                .to_string_lossy()
                .as_ref()
        ),
        shell_escape(worker_name),
    );
    wait_for_stable_guest_success(
        runner,
        &control_plane.name,
        &command,
        Duration::from_secs(180),
        1,
        Duration::from_secs(3),
    )
}
