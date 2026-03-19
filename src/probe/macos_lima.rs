use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::process::{CommandRunner, Output, SystemRunner, ensure_path_exists};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Pass,
    Fail,
    Skip,
}

impl Display for ProbeStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::Fail => write!(f, "FAIL"),
            Self::Skip => write!(f, "SKIP"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub name: &'static str,
    pub status: ProbeStatus,
    pub detail: String,
}

impl ProbeResult {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: ProbeStatus::Pass,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: ProbeStatus::Fail,
            detail: detail.into(),
        }
    }

    fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: ProbeStatus::Skip,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub template: String,
    pub workspace_root: PathBuf,
    pub keep_resources: bool,
    pub enable_rosetta: bool,
    pub network: String,
}

impl ProbeConfig {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            template: "template://default".to_owned(),
            workspace_root,
            keep_resources: false,
            enable_rosetta: true,
            network: "vzNAT".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub results: Vec<ProbeResult>,
}

impl ProbeReport {
    pub fn has_failures(&self) -> bool {
        self.results
            .iter()
            .any(|result| result.status == ProbeStatus::Fail)
    }

    pub fn summary(&self) -> String {
        let passed = self
            .results
            .iter()
            .filter(|result| result.status == ProbeStatus::Pass)
            .count();
        let failed = self
            .results
            .iter()
            .filter(|result| result.status == ProbeStatus::Fail)
            .count();
        let skipped = self
            .results
            .iter()
            .filter(|result| result.status == ProbeStatus::Skip)
            .count();

        format!("summary: {passed} passed, {failed} failed, {skipped} skipped")
    }
}

pub fn run(config: &ProbeConfig) -> ProbeReport {
    let runner = SystemRunner;
    run_with_runner(config, &runner)
}

pub fn run_with_runner(config: &ProbeConfig, runner: &dyn CommandRunner) -> ProbeReport {
    let mut results = Vec::new();

    if !cfg!(target_os = "macos") {
        results.push(ProbeResult::skip(
            "host.os",
            "macOS-only probe; skipping on non-macOS host",
        ));
        return ProbeReport { results };
    }

    if let Err(error) = ensure_path_exists(&config.workspace_root) {
        results.push(ProbeResult::fail("workspace.root", error.to_string()));
        return ProbeReport { results };
    }

    let workspace_root = match canonicalize_path(&config.workspace_root) {
        Ok(path) => path,
        Err(error) => {
            results.push(ProbeResult::fail("workspace.root", error.to_string()));
            return ProbeReport { results };
        }
    };

    let instance_name = format!("kalisto-lima-{}", unique_suffix());
    let mount_probe_dir = workspace_root.join("target").join("lima-mount-probe");
    let mount_probe_file = mount_probe_dir.join("probe.txt");
    let _ = fs::remove_dir_all(&mount_probe_dir);

    match runner.run("limactl", &["--version".into()]) {
        Ok(output) => results.push(ProbeResult::pass(
            "lima.cli",
            first_line(&output.stdout, "limactl is available"),
        )),
        Err(error) => {
            results.push(ProbeResult::fail("lima.cli", error.to_string()));
            return ProbeReport { results };
        }
    }

    match start_instance(runner, config, &instance_name, &workspace_root, None, 2, 4) {
        Ok(output) => results.push(ProbeResult::pass(
            "lima.instance.start",
            first_line(&output.stdout, "lima instance started"),
        )),
        Err(error) => {
            results.push(ProbeResult::fail("lima.instance.start", error.to_string()));
            return finish(ProbeReport { results }, runner, &instance_name, config);
        }
    }

    match runner.run(
        "limactl",
        &[
            "list".into(),
            "--format".into(),
            "{{ .Name }} {{ .Status }} {{ .VMType }}".into(),
            instance_name.clone(),
        ],
    ) {
        Ok(output) => results.push(ProbeResult::pass(
            "lima.instance.status",
            first_line(&output.stdout, "lima instance is running"),
        )),
        Err(error) => {
            results.push(ProbeResult::fail("lima.instance.status", error.to_string()));
            return finish(ProbeReport { results }, runner, &instance_name, config);
        }
    }

    if let Err(error) = fs::create_dir_all(&mount_probe_dir) {
        results.push(ProbeResult::fail("mount.host.prepare", error.to_string()));
        return finish(ProbeReport { results }, runner, &instance_name, config);
    }

    if let Err(error) = fs::write(&mount_probe_file, "host-seeded\n") {
        results.push(ProbeResult::fail("mount.host.prepare", error.to_string()));
        return finish(ProbeReport { results }, runner, &instance_name, config);
    }

    let exec_checks = format!(
        "test -d '{}' && test -d /sys/fs/cgroup && grep -q overlay /proc/filesystems && printf 'lima-touch\\n' >> '{}'",
        shell_escape(workspace_root.as_os_str().to_string_lossy().as_ref()),
        shell_escape(mount_probe_file.as_os_str().to_string_lossy().as_ref()),
    );
    match guest_shell(runner, &instance_name, &exec_checks) {
        Ok(_) => results.push(ProbeResult::pass(
            "lima.instance.exec",
            "guest sees the mounted workspace, cgroup fs, and overlayfs",
        )),
        Err(error) => {
            results.push(ProbeResult::fail("lima.instance.exec", error.to_string()));
            return finish(ProbeReport { results }, runner, &instance_name, config);
        }
    }

    if config.enable_rosetta {
        let rosetta_check = concat!(
            "test -d /proc/sys/fs/binfmt_misc && ",
            "grep -qi rosetta /proc/sys/fs/binfmt_misc/*"
        );
        match guest_shell(runner, &instance_name, rosetta_check) {
            Ok(_) => results.push(ProbeResult::pass(
                "lima.rosetta",
                "Rosetta binfmt handlers are visible inside the guest",
            )),
            Err(error) => results.push(ProbeResult::fail("lima.rosetta", error.to_string())),
        }
    } else {
        results.push(ProbeResult::skip(
            "lima.rosetta",
            "Rosetta check skipped because it is disabled in the probe config",
        ));
    }

    match fs::read_to_string(&mount_probe_file) {
        Ok(contents) if contents.contains("lima-touch") => results.push(ProbeResult::pass(
            "mount.roundtrip",
            "host-to-guest mount is writable and guest changes are visible on the host",
        )),
        Ok(contents) => results.push(ProbeResult::fail(
            "mount.roundtrip",
            format!(
                "probe file did not contain the expected marker after guest write\ncontents:\n{contents}"
            ),
        )),
        Err(error) => results.push(ProbeResult::fail("mount.roundtrip", error.to_string())),
    }

    finish(ProbeReport { results }, runner, &instance_name, config)
}

pub(crate) fn start_instance(
    runner: &dyn CommandRunner,
    config: &ProbeConfig,
    instance_name: &str,
    workspace_root: &Path,
    port_forward: Option<u16>,
    cpus: u8,
    memory_gib: u8,
) -> Result<Output> {
    let mut args = vec![
        "start".into(),
        "--tty=false".into(),
        format!("--name={instance_name}"),
        format!("--cpus={cpus}"),
        format!("--memory={memory_gib}"),
        "--containerd=none".into(),
        "--vm-type=vz".into(),
        "--mount-type=virtiofs".into(),
        format!("--mount-only={}:w", workspace_root.display()),
        format!("--network={}", config.network),
        "--timeout=10m".into(),
    ];
    if config.enable_rosetta {
        args.push("--rosetta".into());
    }
    if let Some(host_port) = port_forward {
        args.push(format!("--port-forward={host_port}:6443,static=true"));
    }
    args.push(config.template.clone());
    runner.run("limactl", &args)
}

pub(crate) fn guest_shell(
    runner: &dyn CommandRunner,
    instance_name: &str,
    command: &str,
) -> Result<Output> {
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

fn finish(
    mut report: ProbeReport,
    runner: &dyn CommandRunner,
    instance_name: &str,
    config: &ProbeConfig,
) -> ProbeReport {
    if config.keep_resources {
        report.results.push(ProbeResult::skip(
            "cleanup",
            format!("kept Lima instance `{instance_name}`"),
        ));
        return report;
    }

    let mut cleanup_errors = Vec::new();
    let _ = runner.run(
        "limactl",
        &[
            "-y".into(),
            "stop".into(),
            "--force".into(),
            instance_name.to_owned(),
        ],
    );
    match runner.run(
        "limactl",
        &[
            "-y".into(),
            "delete".into(),
            "--force".into(),
            instance_name.to_owned(),
        ],
    ) {
        Ok(_) => {}
        Err(error) => cleanup_errors.push(format!("instance cleanup: {error}")),
    }

    if cleanup_errors.is_empty() {
        report.results.push(ProbeResult::pass(
            "cleanup",
            "removed Lima probe resources successfully",
        ));
    } else {
        report
            .results
            .push(ProbeResult::fail("cleanup", cleanup_errors.join("\n")));
    }

    report
}

fn canonicalize_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(|source| Error::CommandIo {
        program: "canonicalize".to_owned(),
        source,
    })
}

pub(crate) fn shell_escape(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

fn first_line(text: &str, fallback: &'static str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}

fn unique_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_millis())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;

    use crate::error::{Error, Result};
    use crate::process::{CommandRunner, Output};

    use super::{ProbeConfig, ProbeStatus, run_with_runner};

    struct FakeRunner {
        responses: RefCell<VecDeque<Result<Output>>>,
        on_run: RefCell<Option<Box<dyn FnMut()>>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<Result<Output>>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                on_run: RefCell::new(None),
            }
        }

        fn with_on_run(responses: Vec<Result<Output>>, on_run: Box<dyn FnMut()>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                on_run: RefCell::new(Some(on_run)),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, _program: &str, _args: &[String]) -> Result<Output> {
            if let Some(callback) = self.on_run.borrow_mut().as_mut() {
                callback();
            }
            self.responses.borrow_mut().pop_front().unwrap_or_else(|| {
                Err(Error::InvalidArgs(
                    "fake runner ran out of scripted responses".to_owned(),
                ))
            })
        }
    }

    fn ok(stdout: &str) -> Result<Output> {
        Ok(Output {
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    #[test]
    fn report_skips_on_non_macos_hosts() {
        if cfg!(target_os = "macos") {
            return;
        }

        let config = ProbeConfig::new(PathBuf::from("."));
        let report = run_with_runner(&config, &FakeRunner::new(Vec::new()));

        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].status, ProbeStatus::Skip);
    }

    #[test]
    fn report_succeeds_for_happy_path() {
        if !cfg!(target_os = "macos") {
            return;
        }

        let workspace_root =
            std::env::temp_dir().join(format!("kalisto-lima-probe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&workspace_root);
        fs::create_dir_all(workspace_root.join("target")).unwrap();

        let probe_file = workspace_root
            .join("target")
            .join("lima-mount-probe")
            .join("probe.txt");
        let probe_file_for_callback = probe_file.clone();

        let runner = FakeRunner::with_on_run(
            vec![
                ok("limactl version 2.1.0"),
                ok("started"),
                ok("kalisto Running vz"),
                ok(""),
                ok(""),
                ok("stopped"),
                ok("deleted"),
            ],
            Box::new(move || {
                if probe_file_for_callback.exists() {
                    let _ = fs::write(&probe_file_for_callback, "host-seeded\nlima-touch\n");
                }
            }),
        );

        let report = run_with_runner(&ProbeConfig::new(workspace_root.clone()), &runner);
        assert!(
            report
                .results
                .iter()
                .all(|result| result.status != ProbeStatus::Fail),
            "{report:#?}"
        );

        let _ = fs::remove_dir_all(workspace_root);
    }
}
